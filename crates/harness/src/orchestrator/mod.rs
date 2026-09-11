//! The agent loop (SPEC §5.3, §6).
//!
//! Runs exclusively on the FIFO worker task (AGENTS.md #5): one order in
//! flight makes session access and confirmation redemption race-free.

pub mod context;

use crate::api::error::ApiError;
use crate::confirm;
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
///
/// The field-preservation rule below addresses a distinct, separately
/// observed failure: a small model correcting one bad field after a tool
/// error (e.g. a malformed `group_id`) tends to rebuild the whole payload
/// from only the fields it's actively reasoning about, silently dropping
/// other previously-correct ones (e.g. a user-specified `date`) rather than
/// carrying them forward — even when its own prior attempt, containing the
/// correct value, is right there in context (`assistant_tool_calls`, above).
fn system_prompt() -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!(
        "You are the home-server harness assistant called Arno. \
         Current date and time: {now}. \
         You interpret orders and answer with the help of the available tools. \
         Be terse and factual. \
         When a tool call fails and you retry it, change only the argument(s) \
         that caused the failure — copy every other value from your own \
         previous attempt exactly. Never drop or reset a field the user \
         already gave you just because you're also fixing something else."
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

    // SPEC §6 step 3 — redemption path, unchanged: explicit token always
    // wins, even in token_only mode.
    if let Some(token) = order.confirmation_token.as_deref() {
        return redeem(st, client_id, &order.session_id, token).await;
    }

    // SPEC §4.3 revised, AGENTS.md #3: model judges consent only; Confirm
    // still dispatches through the unchanged redeem(). peek_latest doesn't
    // consume, so Unrelated/failure just falls through to normal_path.
    if cfg.confirm_mode == crate::config::ConfirmMode::Model
        && let Some((token, action)) =
            st.pending
                .peek_latest(client_id, &order.session_id, cfg.confirm_ttl)
    {
        let verdict = confirm::classify(
            st.provider.as_ref(),
            &action.tool,
            &action.args,
            &order.text,
        )
        .await;
        match verdict {
            Some(confirm::Verdict::Confirm) => {
                return redeem(st, client_id, &order.session_id, &token).await;
            }
            Some(confirm::Verdict::Reject) => {
                return Ok(reject_pending(st, client_id, &order.session_id, &token, action).await);
            }
            Some(confirm::Verdict::Unrelated) | None => {
                // Falls through to normal_path as an ordinary order.
            }
        }
    }

    normal_path(st, client_id, order).await
}

/// Cancels a pending action the model judged declined — single-use via
/// `PendingStore::cancel`, so it can never later be redeemed. Mirrors
/// `redeem()`'s footprint: transcript only, no `SessionStore` touch.
async fn reject_pending(
    st: &Arc<AppState>,
    client_id: &str,
    session_id: &str,
    token: &str,
    action: crate::stores::pending::PendingAction,
) -> Response {
    st.pending.cancel(client_id, session_id, token);
    if let Some(transcript) = &st.transcript
        && let Err(e) = transcript.record(
            client_id,
            session_id,
            crate::transcript::Event::ConfirmationCancelled {
                tool: &action.tool,
                args: &action.args,
            },
        )
    {
        tracing::error!(error = %e, tool = %action.tool, "transcript log write failed");
    }
    Response::text(format!("Cancelled `{}`.", action.tool))
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
            // Broader conversational record (SPEC §12.2) — deliberately
            // duplicates data the audit log also holds: that log is the
            // narrow financial ledger, this is the general narrative that
            // happens to include writes too.
            if let Some(transcript) = &st.transcript {
                let ser_outcome: crate::transcript::SerOutcome = match &outcome {
                    Ok(result) => crate::transcript::Outcome::Ok(result).into(),
                    Err(code) => crate::transcript::Outcome::Err(code.as_str()).into(),
                };
                if let Err(e) = transcript.record(
                    client_id,
                    session_id,
                    crate::transcript::Event::ConfirmationRedeemed {
                        tool: &action.tool,
                        args: &action.args,
                        outcome: ser_outcome,
                    },
                ) {
                    tracing::error!(error = %e, tool = %action.tool, "transcript log write failed");
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
    if let Some(transcript) = &st.transcript
        && let Err(e) = transcript.record(
            client_id,
            &order.session_id,
            crate::transcript::Event::UserMessage { text: &order.text },
        )
    {
        tracing::error!(error = %e, "transcript log write failed");
    }
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
                // Rescue FIRST (SPEC §11 decision #16): a Final reply whose
                // body contains exactly one tool-call-shaped JSON object is
                // re-interpreted as the call the model meant to make. It then
                // flows through the SAME classification/dispatch path below
                // (this branch sets `text = ""` and falls into the ToolCalls
                // arm via a synthetic re-entry — implemented inline to keep
                // the loop's single-writer structure).
                if let Some(call) = rescue_text_tool_call(&text) {
                    tracing::info!(tool = %call.name, "rescued text tool call");
                    let calls = vec![call];
                    dispatched_tool_calls += calls.len() as u32;
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::assistant_tool_calls(calls.clone()),
                    );
                    for call in calls {
                        if let Some(transcript) = &st.transcript
                            && let Err(e) = transcript.record(
                                client_id,
                                &order.session_id,
                                crate::transcript::Event::ToolCall {
                                    call_id: &call.id,
                                    tool: &call.name,
                                    args: &call.args,
                                },
                            )
                        {
                            tracing::error!(error = %e, tool = %call.name, "transcript log write failed");
                        }
                        if st.policy.is_destructive(&call.name, &call.args) {
                            let token = st.pending.freeze(
                                client_id,
                                &order.session_id,
                                call.name.clone(),
                                call.args.clone(),
                            );
                            if let Some(transcript) = &st.transcript
                                && let Err(e) = transcript.record(
                                    client_id,
                                    &order.session_id,
                                    crate::transcript::Event::ConfirmationRequested {
                                        tool: &call.name,
                                        args: &call.args,
                                    },
                                )
                            {
                                tracing::error!(error = %e, tool = %call.name, "transcript log write failed");
                            }
                            return Ok(Response {
                                text: format!(
                                    "This would run `{}` with {}. Reply to confirm or say no to cancel.",
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
                            .unwrap_or_else(|code| {
                                serde_json::Value::String(code.as_str().to_owned())
                            });
                        if let Some(transcript) = &st.transcript
                            && let Err(e) = transcript.record(
                                client_id,
                                &order.session_id,
                                crate::transcript::Event::ToolResult {
                                    call_id: &call.id,
                                    tool: &call.name,
                                    result: &result,
                                },
                            )
                        {
                            tracing::error!(error = %e, tool = %call.name, "transcript log write failed");
                        }
                        st.sessions.append(
                            &key,
                            crate::model::ChatMessage::tool_result(
                                call.id.clone(),
                                result.to_string(),
                            ),
                        );
                        if looks_like_error(&result) {
                            st.sessions.append(
                                &key,
                                crate::model::ChatMessage::new(
                                    crate::model::Role::User,
                                    RETRY_NUDGE,
                                ),
                            );
                        }
                    }
                    if dispatched_tool_calls > st.cfg.max_tool_calls {
                        return Err(ApiError(ErrorCode::OrderBudgetExceeded));
                    }
                    continue; // loop: let the model speak with the real result
                }
                // Claim guard (SPEC §11 decision #16): a Final that asserts
                // execution with NO tool call this order is the observed
                // qwen3:4b lie. Withhold it, correct the model, loop —
                // bounded by budget; on exhaustion the text IS delivered
                // (never silently swallowed) with an ops warn.
                if claims_execution_without_tool(&text, dispatched_tool_calls)
                    && Instant::now() + st.cfg.order_budget / 4 < deadline
                {
                    tracing::warn!("final claims execution but no tool ran this order; nudging");
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::new(crate::model::Role::Assistant, text.clone()),
                    );
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::new(crate::model::Role::User, FALSE_CLAIM_NUDGE),
                    );
                    continue;
                }
                if let Some(transcript) = &st.transcript
                    && let Err(e) = transcript.record(
                        client_id,
                        &order.session_id,
                        crate::transcript::Event::AssistantFinal { text: &text },
                    )
                {
                    tracing::error!(error = %e, "transcript log write failed");
                }
                st.sessions.append(
                    &key,
                    crate::model::ChatMessage::new(crate::model::Role::Assistant, text.clone()),
                );
                return Ok(Response::text(text));
            }
            crate::model::CompletionOutput::ToolCalls(calls) => {
                dispatched_tool_calls += calls.len() as u32;
                // The model's own request, remembered — without this the next
                // completion (even later in this same loop) sees only an
                // orphaned tool result, never what was actually asked for.
                st.sessions.append(
                    &key,
                    crate::model::ChatMessage::assistant_tool_calls(calls.clone()),
                );
                for call in calls {
                    if let Some(transcript) = &st.transcript
                        && let Err(e) = transcript.record(
                            client_id,
                            &order.session_id,
                            crate::transcript::Event::ToolCall {
                                call_id: &call.id,
                                tool: &call.name,
                                args: &call.args,
                            },
                        )
                    {
                        tracing::error!(error = %e, tool = %call.name, "transcript log write failed");
                    }
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
                        if let Some(transcript) = &st.transcript
                            && let Err(e) = transcript.record(
                                client_id,
                                &order.session_id,
                                crate::transcript::Event::ConfirmationRequested {
                                    tool: &call.name,
                                    args: &call.args,
                                },
                            )
                        {
                            tracing::error!(error = %e, tool = %call.name, "transcript log write failed");
                        }
                        return Ok(Response {
                            // SPEC §4.3 revised: most clients now confirm by
                            // replying naturally; confirmation_token below
                            // still works too, just isn't the only way.
                            text: format!(
                                "This would run `{}` with {}. Reply to confirm or say no to cancel.",
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
                    if let Some(transcript) = &st.transcript
                        && let Err(e) = transcript.record(
                            client_id,
                            &order.session_id,
                            crate::transcript::Event::ToolResult {
                                call_id: &call.id,
                                tool: &call.name,
                                result: &result,
                            },
                        )
                    {
                        tracing::error!(error = %e, tool = %call.name, "transcript log write failed");
                    }
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::tool_result(call.id.clone(), result.to_string()),
                    );
                    // A static system-prompt rule competes poorly against
                    // whatever's most recent in context (small-model recency
                    // bias — the same effect that drops a field on correction
                    // also works in our favor here). So instead of only
                    // relying on system_prompt(), inject a fresh, situational
                    // instruction right when a failure actually happened,
                    // maximally recent for the next completion. This cannot
                    // force a tool call (Ollama's `tool_choice` is a documented
                    // no-op on this runtime — verified live), only nudge; the
                    // model can still legitimately choose to ask the user a
                    // question instead of retrying blind.
                    if looks_like_error(&result) {
                        st.sessions.append(
                            &key,
                            crate::model::ChatMessage::new(crate::model::Role::User, RETRY_NUDGE),
                        );
                    }
                }
                if dispatched_tool_calls > st.cfg.max_tool_calls {
                    return Err(ApiError(ErrorCode::OrderBudgetExceeded));
                }
            }
        }
    }
}

const RETRY_NUDGE: &str = "\
The previous tool call did not succeed. If you have enough information, call \
the tool again right now with corrected arguments — keep every previously- \
correct value, change only what caused the failure. If you genuinely need \
more information, ask the user one direct question instead. Do not say you \
will retry or check something later without actually doing it in this turn.";

/// Backend-agnostic (AGENTS.md #1) heuristic, not a guarantee: our own
/// harness-defined tags (`tool_error`, the `backend_unavailable` error code)
/// are always reliable; a bare top-level `error`/`errors` key is a common
/// enough REST/API convention to treat as a weak signal. A false positive
/// just adds a harmless extra nudge; a false negative just falls back to
/// today's behavior — so erring permissive here costs little either way.
fn looks_like_error(result: &serde_json::Value) -> bool {
    match result {
        serde_json::Value::Object(map) => {
            map.contains_key("tool_error")
                || map.contains_key("error")
                || map.contains_key("errors")
        }
        serde_json::Value::String(s) => s == contract::ErrorCode::BackendUnavailable.as_str(),
        _ => false,
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

/// Small models sometimes emit the tool call as literal text (a fenced JSON
/// block or a bare `{...}` object) instead of the structured `tool_calls`
/// field the native API parses — the harness currently discards it and the
/// user sees a model that "answered" without acting (SPEC §11 decision #16).
/// Rescue: a Final reply containing EXACTLY ONE parseable
/// `{"tool": name, "arguments": {...}}` (or `{"name": ...}`) object becomes a
/// real dispatch. 0 or >1 candidates → None (never guess — AGENTS.md #3
/// spirit: don't invent consent-shaped actions). The rescued call flows
/// through the same classification/dispatch path as a structured call.
fn rescue_text_tool_call(text: &str) -> Option<crate::model::ToolCall> {
    let mut candidates: Vec<crate::model::ToolCall> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Balanced-brace scan (string-aware enough: skip braces inside quotes).
            let mut depth = 0usize;
            let mut in_str = false;
            let mut escaped = false;
            let mut end = None;
            for (off, &b) in bytes[i..].iter().enumerate() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match b {
                    b'\\' if in_str => escaped = true,
                    b'"' => in_str = !in_str,
                    b'{' if !in_str => depth += 1,
                    b'}' if !in_str => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i + off + 1);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(end) = end {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[i..end])
                    && let Some(call) = parse_rescued_object(&v)
                {
                    candidates.push(call);
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

/// Shape check for one rescued candidate: string `tool`/`name` + object
/// `arguments`/`args`. Anything else is not a tool call.
fn parse_rescued_object(v: &serde_json::Value) -> Option<crate::model::ToolCall> {
    let obj = v.as_object()?;
    let name = obj
        .get("tool")
        .or_else(|| obj.get("name"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())?;
    let args = match obj.get("arguments").or_else(|| obj.get("args")) {
        Some(v) => v.clone(),
        // Absent arguments key defaults to an empty argument set (mirrors
        // `arguments_object` in the MCP registry, which tolerates drift).
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    if !args.is_object() {
        return None;
    }
    Some(crate::model::ToolCall {
        id: format!(
            "rescued_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ),
        name: name.to_owned(),
        args,
    })
}

/// Success-execution claim patterns, drawn from REAL false claims observed in
/// `data/arno-transcript.jsonl` (qwen3:4b asserted "has been successfully
/// created/applied" with zero tool call behind it — SPEC §11 decision #16).
/// Lowercase substring match, case-insensitive at the call site.
const FALSE_CLAIM_PATTERNS: &[&str] = &[
    "successfully",
    "has been created",
    "has been applied",
    "has been registered",
    "is now live",
    "has been posted",
    "i've recorded",
    "i have recorded",
    "executed the",
];

/// True when a final answer asserts an action was executed but NO tool call
/// has run this order — the observed qwen3:4b failure mode. Only these
/// replies trigger the corrective nudge; ordinary replies (even ones that
/// list data, which one live false claim mimicked) pass through untouched.
fn claims_execution_without_tool(text: &str, dispatched_tool_calls: u32) -> bool {
    if dispatched_tool_calls > 0 {
        return false;
    }
    let lower = text.to_lowercase();
    FALSE_CLAIM_PATTERNS.iter().any(|p| lower.contains(p))
}

const FALSE_CLAIM_NUDGE: &str = "\
You just stated that an action was completed, but you did not call any tool \
in this turn — nothing was executed. If the user's request requires an \
action, call the appropriate tool right now with the arguments you already \
have (you may reuse values from earlier tool results in this conversation). \
If no tool is needed or information is genuinely missing, reply with what \
you actually know and explicitly say no action was taken.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_error_recognizes_harness_and_common_backend_shapes() {
        assert!(looks_like_error(
            &serde_json::json!({"tool_error": "Tool error: bad uuid"})
        ));
        assert!(looks_like_error(
            &serde_json::json!({"error": "group_id and splits must be provided together"})
        ));
        assert!(looks_like_error(&serde_json::json!({"errors": ["a", "b"]})));
        assert!(looks_like_error(&serde_json::json!("backend_unavailable")));
    }

    #[test]
    fn looks_like_error_does_not_flag_ordinary_results() {
        assert!(!looks_like_error(&serde_json::json!({"total": 8})));
        assert!(!looks_like_error(&serde_json::json!("some text answer")));
        assert!(!looks_like_error(&serde_json::json!(null)));
        assert!(!looks_like_error(
            &serde_json::json!({"description": "grocery run", "amount": 50})
        ));
    }

    // ---- text tool-call rescue (SPEC §11 decision #16) ----

    #[test]
    fn rescue_parses_fenced_json_tool_call() {
        let text = "Sure, let me look that up.\n```json\n{\"tool\": \"securo.list_accounts\", \"arguments\": {}}\n```";
        let call = rescue_text_tool_call(text).expect("rescued");
        assert_eq!(call.name, "securo.list_accounts");
        assert_eq!(call.args, serde_json::json!({}));
        assert!(call.id.starts_with("rescued_"));
    }

    #[test]
    fn rescue_parses_bare_json_object() {
        let call = rescue_text_tool_call(
            "{\"name\": \"securo.list_transactions\", \"args\": {\"limit\": 5}}",
        )
        .expect("rescued");
        assert_eq!(call.name, "securo.list_transactions");
        assert_eq!(call.args["limit"], 5);
    }

    #[test]
    fn rescue_ignores_prose_without_json() {
        assert!(rescue_text_tool_call("Your expense has been recorded.").is_none());
        assert!(rescue_text_tool_call("").is_none());
    }

    #[test]
    fn rescue_ignores_multiple_candidates() {
        let text = "{\"tool\": \"a\", \"arguments\": {}} then {\"tool\": \"b\", \"arguments\": {}}";
        assert!(rescue_text_tool_call(text).is_none(), "ambiguous → None");
    }

    #[test]
    fn rescue_requires_object_arguments() {
        assert!(rescue_text_tool_call("{\"tool\": \"a\", \"arguments\": \"x\"}").is_none());
        // No arguments key at all defaults to an empty object set — allowed.
        let call = rescue_text_tool_call("{\"tool\": \"a\"}").expect("defaults to {}");
        assert_eq!(call.args, serde_json::json!({}));
    }

    #[test]
    fn rescue_ignores_non_tool_json_objects() {
        // An ordinary data object (e.g. rendered results) is not a call.
        assert!(rescue_text_tool_call("{\"total\": 8, \"items\": []}").is_none());
        // Empty tool name is not a call.
        assert!(rescue_text_tool_call("{\"tool\": \"\", \"arguments\": {}}").is_none());
    }

    // ---- success-claim guard (SPEC §11 decision #16) ----

    #[test]
    fn claim_guard_flags_success_language_without_calls() {
        assert!(claims_execution_without_tool(
            "Your expense for the Pixel 10 Pro has been successfully registered in your account.",
            0
        ));
        assert!(claims_execution_without_tool(
            "The transaction is now live in your records.",
            0
        ));
        assert!(claims_execution_without_tool(
            "I have recorded the transaction.",
            0
        ));
    }

    #[test]
    fn claim_guard_allows_success_language_after_calls() {
        assert!(!claims_execution_without_tool(
            "Your expense has been successfully created.",
            2
        ));
    }

    #[test]
    fn claim_guard_allows_ordinary_replies() {
        // This exact false-positive pattern occurred in the live transcript
        // ("Here are the available categories for your expense...") — the
        // word "expense" must not trigger the guard.
        assert!(!claims_execution_without_tool(
            "Here are the available categories for your expense: 1. Donations 2. Education",
            0
        ));
        assert!(!claims_execution_without_tool(
            "Could you please provide the account ID where this transaction should be recorded?",
            0
        ));
        assert!(!claims_execution_without_tool("pong", 0));
    }

    #[test]
    fn claim_guard_is_case_insensitive() {
        assert!(claims_execution_without_tool(
            "The Transaction Has Been Created Successfully.",
            0
        ));
    }
}
