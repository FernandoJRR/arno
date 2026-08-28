//! The agent loop (SPEC §5.3, §6).
//!
//! Runs exclusively on the FIFO worker task (AGENTS.md #5): one order in
//! flight makes session access and confirmation redemption race-free.

pub mod context;

use crate::api::error::ApiError;
use crate::state::AppState;
use crate::stores::pending::TakeOutcome;
use contract::{ErrorCode, Response, StructuredItem};
use std::sync::Arc;
use std::time::Instant;

/// Tool dispatch seam. Backed by the rmcp client registry (SPEC §4.2,
/// `mcp::McpRegistry`); with no backends configured, `NoBackends` reports
/// `backend_unavailable`. Redemption tests substitute a recording double.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Read-only dispatch from the normal path (SPEC §6 step 5). The caller
    /// guarantees the tool passed the destructive classifier, so the single
    /// transport-level retry of SPEC §8 applies.
    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ErrorCode>;

    /// Redemption-path dispatch of a frozen payload: never retried — the
    /// confirmation gate is the retry mechanism (SPEC §4.3, AGENTS.md #3).
    async fn execute_frozen(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ErrorCode> {
        self.execute(tool, args).await
    }

    /// Merged, namespaced tool schemas for prompt assembly; empty when no
    /// backends are configured. Static for the process lifetime.
    fn tool_schemas(&self) -> &[serde_json::Value] {
        &[]
    }
}

/// M0 stand-in until the MCP connection manager lands (SPEC §5.5).
pub struct NoBackends;

#[async_trait::async_trait]
impl ToolExecutor for NoBackends {
    async fn execute(
        &self,
        _tool: &str,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value, ErrorCode> {
        Err(ErrorCode::BackendUnavailable)
    }
}

/// Built fresh per order (never baked in at compile time — the harness runs
/// for days) so the model has real grounding for date-relative reasoning.
/// Without this, a model asked for "recent" or "this month" data may scope a
/// `from_date`/`to_date` filter using its own stale training-era belief about
/// "today" instead of the real date, silently returning zero matches for a
/// perfectly valid, correctly-dispatched query — observed live against Securo
/// (SPEC §8: this is a reliability constraint, not a Securo-specific fix).
fn system_prompt() -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!(
        "You are the home-server harness assistant called Arno. \
         Current date and time: {now}. \
         You interpret orders and answer with the help of the available tools. \
         Be terse and factual."
    )
}

/// Entry point from the queue worker. Contract-level failures map onto the
/// documented error codes; adapters see exactly one error surface.
pub async fn handle(
    st: &Arc<AppState>,
    client_id: &str,
    order: contract::Order,
) -> Result<Response, ApiError> {
    let cfg = &st.cfg;
    validate_attachments(&order, cfg.attach_max_bytes)?;

    if let Some(msg_id) = order.client_msg_id.as_deref()
        && st
            .dedup
            .check_and_record(client_id, msg_id, cfg.dedup_window)
    {
        return Err(ApiError(ErrorCode::DuplicateOrder));
    }

    // SPEC §6 step 3 — redemption path: frozen payload dispatched verbatim,
    // zero model involvement.
    if let Some(token) = order.confirmation_token.as_deref() {
        return redeem(st, client_id, &order.session_id, token).await;
    }

    normal_path(st, client_id, order).await
}

fn validate_attachments(order: &contract::Order, cap: usize) -> Result<(), ApiError> {
    for att in &order.attachments {
        if let Some(raw_len) = att
            .validate()
            .map_err(|_| ApiError(ErrorCode::InvalidOrder))?
            && raw_len > cap
        {
            return Err(ApiError(ErrorCode::InvalidOrder));
        }
    }
    Ok(())
}

async fn redeem(
    st: &Arc<AppState>,
    client_id: &str,
    session_id: &str,
    token: &str,
) -> Result<Response, ApiError> {
    match st
        .pending
        .take(client_id, session_id, token, st.cfg.confirm_ttl)
    {
        TakeOutcome::Action(action) => {
            // Deleted-before-dispatch already happened inside take(); a failed
            // execution is NOT retried — the safe direction (SPEC §4.3).
            let outcome = st.executor.execute_frozen(&action.tool, &action.args).await;
            // Audit before reporting the outcome (SPEC §12.1): the write to
            // Securo (or whichever backend) already happened either way — a
            // failure to record it is an ops alarm, never a reason to hide
            // the real execution result from the caller.
            if let Some(audit) = &st.audit {
                let record_outcome = match &outcome {
                    Ok(result) => crate::audit::RecordOutcome::Ok(result),
                    Err(code) => crate::audit::RecordOutcome::Err(code.as_str()),
                };
                if let Err(e) = audit.record(
                    client_id,
                    session_id,
                    &action.tool,
                    &action.args,
                    record_outcome,
                ) {
                    tracing::error!(error = %e, tool = %action.tool, "audit log write failed");
                }
            }
            match outcome {
                Ok(result) => Ok(Response {
                    text: format!("Executed `{}`.", action.tool),
                    needs_confirmation: None,
                    confirmation_token: None,
                    structured: Some(vec![StructuredItem {
                        tool: action.tool,
                        args: action.args,
                        result,
                    }]),
                }),
                Err(code) => Err(ApiError(code)),
            }
        }
        TakeOutcome::Unknown => Err(ApiError(ErrorCode::ConfirmationUnknown)),
        TakeOutcome::Expired => Err(ApiError(ErrorCode::ConfirmationExpired)),
        TakeOutcome::Used => Err(ApiError(ErrorCode::ConfirmationUsed)),
    }
}

async fn normal_path(
    st: &Arc<AppState>,
    client_id: &str,
    order: contract::Order,
) -> Result<Response, ApiError> {
    let key = (client_id.to_owned(), order.session_id.clone());
    st.sessions.append(
        &key,
        crate::model::ChatMessage::new(crate::model::Role::User, order.text),
    );

    let deadline = Instant::now() + st.cfg.order_budget;
    let mut dispatched_tool_calls: u32 = 0;

    loop {
        if Instant::now() >= deadline {
            // Completed read-only side effects would be reported in `structured`
            // once tools exist (SPEC §8); at M0 there is nothing to report.
            return Err(ApiError(ErrorCode::OrderBudgetExceeded));
        }

        let history = st.sessions.snapshot(&key);
        // Budget priority (SPEC §8): schemas → system → newest history first.
        let schemas = st.executor.tool_schemas();
        let prompt = system_prompt();
        let reserved = context::estimate_system(&prompt) + context::estimate_schemas(schemas);
        let messages = {
            let mut msgs = vec![crate::model::ChatMessage::new(
                crate::model::Role::System,
                prompt,
            )];
            msgs.extend(context::trim(&history, st.cfg.num_ctx, reserved));
            msgs
        };

        let req = crate::model::CompletionRequest {
            messages,
            tools: schemas.to_vec(),
            context_tokens: st.cfg.num_ctx,
        };
        match st
            .provider
            .complete(req)
            .await
            .map_err(model_error_to_api)?
        {
            crate::model::CompletionOutput::Final(text) => {
                st.sessions.append(
                    &key,
                    crate::model::ChatMessage::new(crate::model::Role::Assistant, text.clone()),
                );
                return Ok(Response::text(text));
            }
            crate::model::CompletionOutput::ToolCalls(calls) => {
                dispatched_tool_calls += calls.len() as u32;
                for call in calls {
                    // Classification is model-authored ahead of time, never
                    // ahead of this specific order (SPEC §4.2, AGENTS.md #3):
                    // this is a synchronous lookup against the persisted
                    // policy, not a model call.
                    if st.policy.is_destructive(&call.name, &call.args) {
                        // Freeze exact payload + mint token; NEVER dispatch here,
                        // NEVER auto-retry (SPEC §4.3, §8).
                        let token = st.pending.freeze(
                            client_id,
                            &order.session_id,
                            call.name.clone(),
                            call.args.clone(),
                        );
                        return Ok(Response {
                            text: format!(
                                "This would run `{}` with {}. \
                                 Resend with confirmation_token to execute it.",
                                call.name,
                                render_args(&call.args)
                            ),
                            needs_confirmation: Some(true),
                            confirmation_token: Some(token),
                            structured: None,
                        });
                    }
                    let result = st
                        .executor
                        .execute(&call.name, &call.args)
                        .await
                        .unwrap_or_else(|code| serde_json::Value::String(code.as_str().to_owned()));
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::tool_result(call.id.clone(), result.to_string()),
                    );
                }
                if dispatched_tool_calls > st.cfg.max_tool_calls {
                    return Err(ApiError(ErrorCode::OrderBudgetExceeded));
                }
            }
        }
    }
}

/// Renders a tool call's arguments generically for the confirmation prompt —
/// informed consent from the mechanical prompt alone (SPEC §4.3), without
/// depending on the model having already shown a preview earlier in the
/// conversation. No backend-specific field names (AGENTS.md #1): this is a
/// plain key=value dump of whatever the tool's own schema produced.
fn render_args(args: &serde_json::Value) -> String {
    match args.as_object() {
        Some(map) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", "),
        _ => "no arguments".to_owned(),
    }
}

fn model_error_to_api(e: crate::model::ModelError) -> ApiError {
    tracing::warn!(error = %e, "model provider failure");
    ApiError(ErrorCode::BackendUnavailable)
}
