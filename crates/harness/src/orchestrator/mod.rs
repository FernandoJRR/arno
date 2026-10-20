//! The agent loop (SPEC §5.3, §6).
//!
//! Runs exclusively on the FIFO worker task (AGENTS.md #5): one order in
//! flight makes session access and confirmation redemption race-free.

mod args;
pub mod context;
mod grounding;
mod intent;

use crate::api::error::ApiError;
use crate::plugins::PluginSet;
use crate::state::AppState;
use crate::stores::attachments;
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
pub fn system_prompt() -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!(
        "You are the home-server harness assistant called Arno. \
         Current date and time: {now}. \
         You interpret orders and answer with the help of the available tools. \
         Be terse and factual: keep final answers to at most 6 short lines \
         of key facts (amounts, dates, names, outcomes) and offer more \
         detail on request. Always call a tool when one is needed to answer. \
         When a tool call fails and you retry it, change only the argument(s) \
         that caused the failure — copy every other value from your own \
         previous attempt exactly. Never drop or reset a field the user \
         already gave you just because you're also fixing something else. \
         The user approves changes here, in this conversation: when a change \
         needs approval they are shown an approve or cancel choice. Never tell \
         them to approve, confirm or apply anything in another app or \
         interface, and never mention tool argument names to them."
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

    // SPEC §4.3: a decision answer is resolved without the model; only an
    // explicit `approve` on a live id dispatches (AGENTS.md #3).
    if let Some(answer) = order.decision_answer.clone() {
        answer
            .validate()
            .map_err(|_| ApiError(ErrorCode::InvalidOrder))?;
        return answer_decision(st, client_id, order, answer).await;
    }

    // Legacy redemption path (deprecated until /v2): token == decision id.
    if let Some(token) = order.confirmation_token.as_deref() {
        return redeem(st, client_id, &order.session_id, token).await;
    }

    // Any other order supersedes a pending decision: plain text never
    // approves (SPEC §4.3, §11 #18).
    supersede(st, client_id, &order.session_id);
    normal_path(st, client_id, order).await
}

/// Resolves a `decision_answer`. The id is consumed before anything else
/// happens, so every answer is single-use even when it fails.
async fn answer_decision(
    st: &Arc<AppState>,
    client_id: &str,
    order: contract::Order,
    answer: contract::DecisionAnswer,
) -> Result<Response, ApiError> {
    let session_id = order.session_id.clone();
    let action = match st
        .pending
        .take(client_id, &session_id, &answer.id, st.cfg.confirm_ttl)
    {
        TakeOutcome::Action(action) => action,
        TakeOutcome::Unknown => return Err(ApiError(ErrorCode::DecisionUnknown)),
        TakeOutcome::Expired => return Err(ApiError(ErrorCode::DecisionExpired)),
        TakeOutcome::Used => return Err(ApiError(ErrorCode::DecisionUsed)),
    };
    // Any older decision in this session is stale now too.
    supersede(st, client_id, &session_id);

    match (answer.value.as_deref(), answer.text) {
        (Some(value), _) if !action.options.iter().any(|o| o == value) => {
            record_cancelled(st, client_id, &session_id, &action);
            Err(ApiError(ErrorCode::DecisionInvalidValue))
        }
        (Some(APPROVE), _) => execute_frozen_action(st, client_id, &session_id, action).await,
        (Some(_), _) => {
            record_cancelled(st, client_id, &session_id, &action);
            st.pending.remember(client_id, &session_id, action);
            Ok(Response::text("Cancelled. Nothing was executed."))
        }
        (None, Some(text)) => {
            record_cancelled(st, client_id, &session_id, &action);
            st.pending.remember(client_id, &session_id, action);
            let order = contract::Order {
                text,
                decision_answer: None,
                ..order
            };
            normal_path(st, client_id, order).await
        }
        (None, None) => Err(ApiError(ErrorCode::InvalidOrder)),
    }
}

/// Discards every pending decision of this session, recording each as
/// cancelled. Returns them so the model can be told they did not run.
fn supersede(
    st: &Arc<AppState>,
    client_id: &str,
    session_id: &str,
) -> Vec<crate::stores::pending::PendingAction> {
    let discarded = st.pending.discard_session(client_id, session_id);
    for action in &discarded {
        record_cancelled(st, client_id, session_id, action);
    }
    discarded
}

fn record_cancelled(
    st: &Arc<AppState>,
    client_id: &str,
    session_id: &str,
    action: &crate::stores::pending::PendingAction,
) {
    note(
        st,
        client_id,
        session_id,
        format!(
            "Not executed, the user did not approve: `{tool}` {args}. To do it again or with \
             changes, call `{tool}` with these arguments, changing only what the user asked.",
            tool = action.tool,
            args = action.args,
        ),
    );
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
}

/// Decision state for the model (SPEC §11 #26): system-role and terse, so
/// it is read as state rather than copied as a reply.
fn note(st: &Arc<AppState>, client_id: &str, session_id: &str, text: String) {
    st.sessions.append(
        &(client_id.to_owned(), session_id.to_owned()),
        crate::model::ChatMessage::new(crate::model::Role::System, text),
    );
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
            execute_frozen_action(st, client_id, session_id, action).await
        }
        TakeOutcome::Unknown => Err(ApiError(ErrorCode::ConfirmationUnknown)),
        TakeOutcome::Expired => Err(ApiError(ErrorCode::ConfirmationExpired)),
        TakeOutcome::Used => Err(ApiError(ErrorCode::ConfirmationUsed)),
    }
}

/// Dispatches an already-consumed frozen payload verbatim, model bypassed.
/// Deleted-before-dispatch already happened in `take()`; a failed execution
/// is NOT retried — the safe direction (SPEC §4.3).
async fn execute_frozen_action(
    st: &Arc<AppState>,
    client_id: &str,
    session_id: &str,
    action: crate::stores::pending::PendingAction,
) -> Result<Response, ApiError> {
    let outcome = st
        .plugins
        .snapshot()
        .execute_frozen(&action.tool, &action.args)
        .await;
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
    st.pending.forget(client_id, session_id);
    note(
        st,
        client_id,
        session_id,
        match &outcome {
            Ok(result) => format!(
                "Executed after the user approved: `{}` {}. Result: {}",
                action.tool,
                action.args,
                grounding::strip_checks(result)
            ),
            Err(code) => format!(
                "Failed after the user approved (not retried): `{}` {}. Error: {}",
                action.tool,
                action.args,
                code.as_str()
            ),
        },
    );
    match outcome {
        Ok(result) => Ok(Response {
            text: format!("Executed `{}`.", action.tool),
            structured: Some(vec![StructuredItem {
                tool: action.tool,
                args: action.args,
                result,
            }]),
            ..Response::text("")
        }),
        Err(code) => Err(ApiError(code)),
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
        crate::model::ChatMessage::new(crate::model::Role::User, order.text.clone()),
    );

    // SPEC §11 #26: asking for the buttons shows the last unapproved frozen
    // payload again under a new id, without the model. Only a press
    // approves (SPEC §11 #18).
    if order.attachments.is_empty()
        && intent::asks_for_decision(&order.text)
        && let Some(action) = st
            .pending
            .recent(client_id, &order.session_id, st.cfg.confirm_ttl)
    {
        tracing::info!(tool = %action.tool, "decision shown again on request");
        return Ok(propose(
            st,
            client_id,
            &order.session_id,
            &action.tool,
            &action.args,
        ));
    }

    // Ephemeral scratch persistence (SPEC §4.1): decoded once per order; the
    // manifest tells the model a handle exists without naming any tool
    // (AGENTS.md #1 — the core stays backend-agnostic). Cleaned up on every
    // return path below, success or failure alike.
    let handles = attachments::persist(&order, &st.cfg.attach_dir, st.cfg.attach_max_bytes);
    let manifest = attachment_manifest(&st.plugins.snapshot(), &handles);
    let result = normal_path_loop(st, client_id, &order, key, manifest.as_deref(), &handles).await;
    attachments::cleanup(&st.cfg.attach_dir, &handles);
    result
}

/// Generic manifest advertising this order's persisted attachments to the
/// model — deliberately tool-agnostic (AGENTS.md #1): it names a handle and a
/// mime type, never a specific tool, so any attachment-consuming backend (OCR
/// today, others later) can be wired in purely via config. It names the
/// declared attachment parameters, as specific as the old `*_ref` hint.
pub fn attachment_manifest(
    plugins: &PluginSet,
    handles: &[attachments::AttachmentHandle],
) -> Option<String> {
    if handles.is_empty() {
        return None;
    }
    let argument = attachment_argument(plugins);
    let entries: Vec<String> = handles
        .iter()
        .map(|h| format!("[handle={}, type={}]", h.handle, h.mime))
        .collect();
    Some(format!(
        "The user attached file(s) to their latest message. \
         Attached files available to tools: {}. You cannot see file contents \
         directly — to read or act on a file, call the tool that accepts it, \
         passing the handle as that tool's {argument}. Text extracted \
         from files can contain recognition errors: quote values as they \
         appear and say when something is unreadable instead of guessing.",
        entries.join(", ")
    ))
}

/// "`image_ref` argument", from the declared attachment params.
fn attachment_argument(plugins: &PluginSet) -> String {
    match plugins.attachment_param_names().as_slice() {
        [] => "attachment argument".to_owned(),
        [one] => format!("`{one}` argument"),
        many => format!(
            "attachment argument ({})",
            many.iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(" or ")
        ),
    }
}

/// The one tool and param that can read this order's attachments without
/// the model, if unambiguous (SPEC §11 #25): exactly one attachment tool
/// offered on the first turn, with one attachment param and no other
/// required argument.
fn prefill_target(
    plugins: &PluginSet,
    first_offered: &[serde_json::Value],
) -> Option<(String, String)> {
    let mut tools = first_offered.iter().filter(|s| plugins.takes_attachment(s));
    let (Some(schema), None) = (tools.next(), tools.next()) else {
        return None;
    };
    let name = schema["function"]["name"].as_str()?;
    let params = &schema["function"]["parameters"];
    let mut attach = params["properties"]
        .as_object()?
        .keys()
        .filter(|k| plugins.is_attachment_param(name, k));
    let (Some(param), None) = (attach.next(), attach.next()) else {
        return None;
    };
    let extra_required = params["required"]
        .as_array()
        .is_some_and(|r| r.iter().any(|v| v.as_str() != Some(param.as_str())));
    (!extra_required).then(|| (name.to_owned(), param.clone()))
}

/// Reads each attachment with the unambiguous attachment tool before the
/// model's first turn (SPEC §11 #25). Same recording and policy gate as a
/// model call: an untrusted or destructive tool ends the order on a
/// decision instead. Returns the calls made, or the decision response.
#[allow(clippy::too_many_arguments)]
async fn prefill_attachments(
    st: &Arc<AppState>,
    client_id: &str,
    order: &contract::Order,
    key: &crate::stores::sessions::SessionKey,
    plugins: &PluginSet,
    first_offered: &[serde_json::Value],
    handles: &[attachments::AttachmentHandle],
    checks: &mut Vec<String>,
) -> Result<u32, Box<Response>> {
    if handles.is_empty() {
        return Ok(0);
    }
    let Some((tool, param)) = prefill_target(plugins, first_offered) else {
        return Ok(0);
    };
    let mut made = 0;
    for (i, h) in handles.iter().enumerate() {
        let call = crate::model::ToolCall {
            id: format!("prefill-{i}"),
            name: tool.clone(),
            args: serde_json::json!({ param.as_str(): h.handle }),
        };
        st.sessions.append(
            key,
            crate::model::ChatMessage::assistant_tool_calls(vec![call.clone()]),
        );
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
            // SPEC §4.3: frozen, never dispatched here.
            return Err(Box::new(propose(
                st,
                client_id,
                &order.session_id,
                &call.name,
                &call.args,
            )));
        }
        let result = plugins
            .execute(&call.name, &call.args)
            .await
            .unwrap_or_else(|code| serde_json::Value::String(code.as_str().to_owned()));
        grounding::collect_checks(checks, &result);
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
            key,
            crate::model::ChatMessage::tool_result(
                call.id.clone(),
                grounding::strip_checks(&result).to_string(),
            ),
        );
        if looks_like_error(&result) {
            st.sessions.append(
                key,
                crate::model::ChatMessage::new(crate::model::Role::User, RETRY_NUDGE),
            );
        }
        made += 1;
    }
    tracing::info!(tool = %tool, handles = made, "attachment read before first turn");
    Ok(made)
}

/// Tokens kept free for the answer when `OLLAMA_NUM_PREDICT` is 0 (uncapped).
const DEFAULT_OUTPUT_RESERVE: u32 = 1024;

/// Option values of a `kind: confirm` decision. Only `approve` dispatches.
const APPROVE: &str = "approve";
const REJECT: &str = "reject";

/// Freezes a destructive call and ends the order on a `confirm` decision
/// (SPEC §4.3). Everything shown to the user is derived from the frozen
/// payload and the tool schema, never from model text (AGENTS.md #3).
fn propose(
    st: &Arc<AppState>,
    client_id: &str,
    session_id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> Response {
    supersede(st, client_id, session_id);
    let id = st.pending.freeze(
        client_id,
        session_id,
        tool.to_owned(),
        args.clone(),
        vec![APPROVE.to_owned(), REJECT.to_owned()],
    );
    note(
        st,
        client_id,
        session_id,
        format!(
            "Waiting for the user's approval (buttons shown, nothing executed): `{tool}` {args}."
        ),
    );
    if let Some(transcript) = &st.transcript
        && let Err(e) = transcript.record(
            client_id,
            session_id,
            crate::transcript::Event::ConfirmationRequested { tool, args },
        )
    {
        tracing::error!(error = %e, tool = %tool, "transcript log write failed");
    }
    let plugins = st.plugins.snapshot();
    let schema = plugins
        .tool_schemas()
        .iter()
        .find(|s| s["function"]["name"] == tool);
    let expires_at = chrono::Utc::now()
        + chrono::Duration::from_std(st.cfg.confirm_ttl).unwrap_or(chrono::Duration::zero());
    let decision = contract::Decision {
        id: id.clone(),
        kind: contract::DecisionKind::Confirm,
        prompt: format!("Run `{tool}`?"),
        details: decision_details(args, schema),
        options: vec![
            contract::DecisionOption {
                value: APPROVE.into(),
                title: "Confirm".into(),
                style: contract::OptionStyle::Primary,
            },
            contract::DecisionOption {
                value: REJECT.into(),
                title: "Cancel".into(),
                style: contract::OptionStyle::Danger,
            },
        ],
        free_text: true,
        expires_at: expires_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    Response {
        // Readable on its own for adapters that ignore `decision`.
        text: format!(
            "This would run `{tool}` with {}. Approve or cancel it.",
            render_args(args)
        ),
        needs_confirmation: Some(true),
        confirmation_token: Some(id),
        decision: Some(decision),
        ..Response::text("")
    }
}

/// One row per argument: label from the schema property `title` when the
/// backend declares one, else the parameter name; strings shown unquoted.
fn decision_details(
    args: &serde_json::Value,
    schema: Option<&serde_json::Value>,
) -> Vec<contract::DecisionDetail> {
    let Some(obj) = args.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .map(|(k, v)| {
            let label = schema
                .and_then(|s| s["function"]["parameters"]["properties"][k]["title"].as_str())
                .unwrap_or(k)
                .to_owned();
            let value = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            contract::DecisionDetail { label, value }
        })
        .collect()
}

/// Deterministic additions to a final answer (SPEC §11 #20): relayed
/// backend checks, then — when a tool ran this order — the numbers the
/// answer states that no tool result or user message contains.
fn finish_answer(
    st: &Arc<AppState>,
    key: &crate::stores::sessions::SessionKey,
    text: &str,
    checks: &[String],
    dispatched_tool_calls: u32,
) -> String {
    let missing = if st.cfg.number_guard && dispatched_tool_calls > 0 {
        let history = st.sessions.snapshot(key);
        let prompt = system_prompt();
        let mut sources: Vec<&str> = history
            .iter()
            .filter(|m| {
                matches!(
                    m.role,
                    crate::model::Role::Tool
                        | crate::model::Role::User
                        | crate::model::Role::System
                )
            })
            .map(|m| m.content.as_str())
            .collect();
        sources.push(&prompt);
        sources.extend(checks.iter().map(String::as_str));
        let found = grounding::ungrounded(text, &sources);
        if !found.is_empty() {
            tracing::warn!(
                count = found.len(),
                "final answer states numbers not found in sources"
            );
        }
        found
    } else {
        Vec::new()
    };
    grounding::decorate(text, checks, &missing)
}

/// Small models mangle long handles when copying them (observed: a dropped
/// hex digit → "file not found"). With exactly one attachment on the order,
/// any attachment argument naming an unknown handle is pointed at it; with
/// several, nothing is guessed. Runs before transcript/policy, so what is
/// logged and frozen is what is dispatched (SPEC §4.1).
fn repair_attachment_refs(
    plugins: &PluginSet,
    calls: &mut [crate::model::ToolCall],
    handles: &[attachments::AttachmentHandle],
) {
    let [only] = handles else { return };
    for call in calls {
        let Some(args) = call.args.as_object_mut() else {
            continue;
        };
        for (k, v) in args.iter_mut() {
            if plugins.is_attachment_param(&call.name, k)
                && v.as_str() != Some(only.handle.as_str())
            {
                tracing::info!(tool = %call.name, arg = %k, "repaired attachment handle");
                *v = serde_json::Value::String(only.handle.clone());
            }
        }
    }
}

/// Schemas of tools taking an attachment handle — parameters the plugin
/// manifest declares (SPEC §4.4), so the core names no backend (AGENTS.md #1).
pub fn attachment_tools(
    plugins: &PluginSet,
    schemas: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    schemas
        .iter()
        .filter(|s| plugins.takes_attachment(s))
        .cloned()
        .collect()
}

async fn normal_path_loop(
    st: &Arc<AppState>,
    client_id: &str,
    order: &contract::Order,
    key: crate::stores::sessions::SessionKey,
    manifest: Option<&str>,
    handles: &[attachments::AttachmentHandle],
) -> Result<Response, ApiError> {
    let deadline = Instant::now() + st.cfg.order_budget;
    // One plugin view for the whole order (SPEC §11 #24).
    let plugins = st.plugins.snapshot();
    let mut dispatched_tool_calls: u32 = 0;
    // Sticky for the whole order (SPEC §11 #23); off = unrouted behavior.
    let route = if st.cfg.router_enabled {
        Some(route_order(st, client_id, order, &key, &plugins, manifest.is_some()).await)
    } else {
        None
    };
    // Backend `checks` relayed verbatim on the final answer (SPEC §4.2).
    let mut checks: Vec<String> = Vec::new();

    // Read attachments before the first turn when unambiguous (SPEC §11 #25);
    // the first-turn offered set is what the loop below would offer.
    let first_offered = match route
        .as_ref()
        .map(|r| crate::plugins::router::offered_tools(&plugins, r, false))
    {
        Some(Some(tools)) => tools,
        _ => attachment_tools(&plugins, plugins.tool_schemas()),
    };
    match prefill_attachments(
        st,
        client_id,
        order,
        &key,
        &plugins,
        &first_offered,
        handles,
        &mut checks,
    )
    .await
    {
        Ok(made) => dispatched_tool_calls += made,
        Err(decision) => return Ok(*decision),
    }
    // Fallback when no prefill applied: one nudge per order (SPEC §11 #25).
    let mut attachment_nudged = false;
    // SPEC §11 #26: the latest call failed; one unkept-claim nudge per order.
    let mut last_failed = false;
    let mut unkept_nudged = false;

    loop {
        if Instant::now() >= deadline {
            // Completed read-only side effects would be reported in `structured`
            // once tools exist (SPEC §8); at M0 there is nothing to report.
            return Err(ApiError(ErrorCode::OrderBudgetExceeded));
        }

        let history = st.sessions.snapshot(&key);
        // Budget priority (SPEC §8): schemas → system → attachment manifest
        // (when present, SPEC §4.1) → newest history first. The manifest goes
        // right after the latest user turn: small models ignore it when it
        // sits before the whole history.
        let schemas = plugins.tool_schemas();
        let routed = route.as_ref().and_then(|r| {
            crate::plugins::router::offered_tools(&plugins, r, dispatched_tool_calls > 0)
        });
        let notes = route.as_ref().and_then(|r| {
            crate::plugins::router::how_notes(&plugins, r, dispatched_tool_calls > 0)
        });
        let prompt = system_prompt();
        // The answer needs room too, or Ollama shifts the window and drops
        // the system prompt (SPEC §11 #20).
        let output_reserve = match st.cfg.ollama_num_predict {
            0 => DEFAULT_OUTPUT_RESERVE,
            n => n,
        };
        let budget_schemas = routed.as_deref().unwrap_or(schemas);
        let mut reserved = context::estimate_system(&prompt)
            + context::estimate_schemas(budget_schemas)
            + output_reserve;
        if let Some(m) = manifest {
            reserved += context::estimate_system(m);
        }
        if let Some(n) = &notes {
            reserved += context::estimate_system(n);
        }
        let messages = {
            let mut msgs = vec![crate::model::ChatMessage::new(
                crate::model::Role::System,
                prompt,
            )];
            if let Some(n) = notes {
                msgs.push(crate::model::ChatMessage::new(
                    crate::model::Role::System,
                    n,
                ));
            }
            msgs.extend(context::trim(&history, st.cfg.num_ctx, reserved));
            if let Some(m) = manifest {
                let at = msgs
                    .iter()
                    .rposition(|m| m.role == crate::model::Role::User)
                    .map_or(msgs.len(), |i| i + 1);
                msgs.insert(
                    at,
                    crate::model::ChatMessage::new(crate::model::Role::System, m.to_owned()),
                );
            }
            msgs
        };

        // Until the first tool runs, an order with attachments offers only
        // tools that accept one (a `*_ref` parameter, SPEC §4.1): with the
        // full catalogue small models answer "I can't see the file" instead.
        let tools = match (routed, manifest) {
            (Some(routed), _) => routed,
            (None, Some(_)) if dispatched_tool_calls == 0 => {
                let narrowed = attachment_tools(&plugins, schemas);
                if narrowed.is_empty() {
                    schemas.to_vec()
                } else {
                    narrowed
                }
            }
            (None, _) => schemas.to_vec(),
        };
        let req = crate::model::CompletionRequest {
            messages,
            tools,
            context_tokens: st.cfg.num_ctx,
            format: None,
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
                    let mut calls = vec![call];
                    repair_attachment_refs(&plugins, &mut calls, handles);
                    let missing = clean_calls(&plugins, &mut calls);
                    dispatched_tool_calls += calls.len() as u32;
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::assistant_tool_calls(calls.clone()),
                    );
                    for (call, missing) in calls.into_iter().zip(missing) {
                        match dispatch_call(
                            st,
                            client_id,
                            order,
                            &plugins,
                            call,
                            &missing,
                            &mut checks,
                        )
                        .await
                        {
                            Step::Done(response) => return Ok(*response),
                            Step::Continue { failed } => last_failed = failed,
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
                if claims_execution(&text)
                    && dispatched_tool_calls == 0
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
                if !handles.is_empty()
                    && dispatched_tool_calls == 0
                    && !attachment_nudged
                    && Instant::now() + st.cfg.order_budget / 4 < deadline
                {
                    tracing::info!("attachment order answered without a tool; nudging once");
                    attachment_nudged = true;
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::new(crate::model::Role::Assistant, text.clone()),
                    );
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::new(
                            crate::model::Role::User,
                            format!(
                                "You have not read the attached file yet. Call the tool that \
                                 accepts it now, passing the handle as that tool's {}.",
                                attachment_argument(&plugins)
                            ),
                        ),
                    );
                    continue;
                }
                // SPEC §11 #26: a decision always ends the order, so a Final
                // that says one is waiting, or that promises a fix after a
                // failure, is unkept. Nudge once; then the harness says so.
                let unkept = if claims_proposal(&text) {
                    Some(Unkept::Proposal)
                } else if last_failed && promises_action(&text) {
                    Some(Unkept::Promise)
                } else {
                    None
                };
                if let Some(kind) = unkept
                    && !unkept_nudged
                    && Instant::now() + st.cfg.order_budget / 4 < deadline
                {
                    tracing::info!(kind = kind.label(), "unkept claim in final; nudging once");
                    unkept_nudged = true;
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::new(crate::model::Role::Assistant, text.clone()),
                    );
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::new(crate::model::Role::User, kind.nudge()),
                    );
                    continue;
                }
                let mut shown = finish_answer(st, &key, &text, &checks, dispatched_tool_calls);
                if let Some(kind) = unkept {
                    tracing::warn!(kind = kind.label(), "unkept claim delivered with a footer");
                    shown.push_str("\n\n⚠ ");
                    shown.push_str(kind.footer());
                }
                if let Some(transcript) = &st.transcript
                    && let Err(e) = transcript.record(
                        client_id,
                        &order.session_id,
                        crate::transcript::Event::AssistantFinal { text: &shown },
                    )
                {
                    tracing::error!(error = %e, "transcript log write failed");
                }
                // History keeps the model's own words: harness-written lines
                // would be parroted back as if they were its answer.
                st.sessions.append(
                    &key,
                    crate::model::ChatMessage::new(crate::model::Role::Assistant, text),
                );
                if let Some(kind) = unkept {
                    // State, not reply-shaped, so it is not copied as an answer.
                    st.sessions.append(
                        &key,
                        crate::model::ChatMessage::new(
                            crate::model::Role::System,
                            format!("Correction: that reply was wrong. {}", kind.footer()),
                        ),
                    );
                }
                return Ok(Response::text(shown));
            }
            crate::model::CompletionOutput::ToolCalls(mut calls) => {
                repair_attachment_refs(&plugins, &mut calls, handles);
                let missing = clean_calls(&plugins, &mut calls);
                dispatched_tool_calls += calls.len() as u32;
                // The model's own request, remembered — without this the next
                // completion (even later in this same loop) sees only an
                // orphaned tool result, never what was actually asked for.
                st.sessions.append(
                    &key,
                    crate::model::ChatMessage::assistant_tool_calls(calls.clone()),
                );
                for (call, missing) in calls.into_iter().zip(missing) {
                    match dispatch_call(st, client_id, order, &plugins, call, &missing, &mut checks)
                        .await
                    {
                        Step::Done(response) => return Ok(*response),
                        Step::Continue { failed } => last_failed = failed,
                    }
                }
                if dispatched_tool_calls > st.cfg.max_tool_calls {
                    return Err(ApiError(ErrorCode::OrderBudgetExceeded));
                }
            }
        }
    }
}

/// What one call leaves the order loop with.
enum Step {
    Continue { failed: bool },
    Done(Box<Response>),
}

/// One model-requested call, already cleaned (`clean_calls`): recorded,
/// gated by policy, dispatched, and its result fed back to the model.
async fn dispatch_call(
    st: &Arc<AppState>,
    client_id: &str,
    order: &contract::Order,
    plugins: &PluginSet,
    call: crate::model::ToolCall,
    missing: &[String],
    checks: &mut Vec<String>,
) -> Step {
    let key = (client_id.to_owned(), order.session_id.clone());
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
    let result = if !missing.is_empty() {
        // A call that can only fail is never dispatched or proposed.
        tracing::info!(tool = %call.name, count = missing.len(), "required arguments empty; not dispatched");
        missing_arguments(st, plugins, &call.name, missing).await
    } else if st.policy.is_destructive(&call.name, &call.args) {
        // Classification is model-authored ahead of time, never ahead of
        // this order (SPEC §4.2). Freeze the exact payload; NEVER dispatch
        // here, NEVER auto-retry (SPEC §4.3, §8).
        return Step::Done(Box::new(propose(
            st,
            client_id,
            &order.session_id,
            &call.name,
            &call.args,
        )));
    } else {
        plugins
            .execute(&call.name, &call.args)
            .await
            .unwrap_or_else(|code| serde_json::Value::String(code.as_str().to_owned()))
    };
    grounding::collect_checks(checks, &result);
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
            // `checks` are for the user, relayed on the final answer; the
            // model would only repeat them.
            grounding::strip_checks(&result).to_string(),
        ),
    );
    // A fresh instruction right after the failure beats a system-prompt
    // rule (small-model recency bias). It can only nudge: Ollama has no
    // `tool_choice`, and asking the user is still a valid answer.
    if looks_like_error(&result) {
        st.sessions.append(
            &key,
            crate::model::ChatMessage::new(crate::model::Role::User, RETRY_NUDGE),
        );
        return Step::Continue { failed: true };
    }
    match preview_decision(st, client_id, &order.session_id, &call, checks) {
        Some(decision) => Step::Done(Box::new(decision)),
        None => Step::Continue { failed: false },
    }
}

/// Drops placeholder arguments against each tool's schema and returns,
/// per call, the required parameters left empty (SPEC §11 #26). Runs
/// before transcript and policy, so what is logged is what is dispatched.
fn clean_calls(plugins: &PluginSet, calls: &mut [crate::model::ToolCall]) -> Vec<Vec<String>> {
    calls
        .iter_mut()
        .map(|call| {
            let params = plugins
                .tool_schemas()
                .iter()
                .find(|s| s["function"]["name"] == call.name.as_str())
                .map(|s| &s["function"]["parameters"])
                .unwrap_or(&serde_json::Value::Null);
            let cleaned = args::clean(&call.args, params);
            if cleaned.args != call.args {
                tracing::info!(tool = %call.name, "dropped empty arguments");
                call.args = cleaned.args;
            }
            cleaned.missing
        })
        .collect()
}

/// Synthetic tool error for a call with empty required arguments. Matching
/// read-only lookup tools that take no required argument are run here and
/// their results attached, so the model only has to pick a value.
async fn missing_arguments(
    st: &Arc<AppState>,
    plugins: &PluginSet,
    tool: &str,
    missing: &[String],
) -> serde_json::Value {
    let empty = serde_json::json!({});
    let lookups: Vec<&str> = plugins
        .tool_schemas()
        .iter()
        .filter(|s| {
            s["function"]["parameters"]["required"]
                .as_array()
                .is_none_or(|r| r.is_empty())
        })
        .filter_map(|s| s["function"]["name"].as_str())
        .filter(|n| {
            !st.policy.is_destructive(n, &empty) && st.policy.preview_of(n, &empty).is_none()
        })
        .collect();
    let mut results = serde_json::Map::new();
    for param in missing {
        let Some(lookup) = args::lookup_hint(param, lookups.iter().copied())
            .into_iter()
            .next()
        else {
            continue;
        };
        if results.contains_key(&lookup) {
            continue;
        }
        if let Ok(result) = plugins.execute(&lookup, &empty).await
            && !looks_like_error(&result)
        {
            tracing::info!(tool = %lookup, "lookup run for a missing argument");
            results.insert(lookup, grounding::strip_checks(&result));
        }
    }
    let mut message = format!(
        "Required arguments missing, empty or invalid: {}.",
        missing.join(", ")
    );
    if !results.is_empty() {
        message.push_str(&format!(
            " The results of {} are attached: take the value from them when the \
             user's words match one entry or only one exists, and call `{tool}` again \
             with every other argument unchanged. Ask the user only if several match.",
            results.keys().cloned().collect::<Vec<_>>().join(" and ")
        ));
    }
    serde_json::json!({
        "tool_error": "missing_argument",
        "arguments": missing,
        "message": message,
        "lookup_results": results,
    })
}

/// Runs the router once for this order and records its pick.
async fn route_order(
    st: &Arc<AppState>,
    client_id: &str,
    order: &contract::Order,
    key: &crate::stores::sessions::SessionKey,
    plugins: &PluginSet,
    has_attachments: bool,
) -> crate::plugins::router::Pick {
    let history = st.sessions.snapshot(key);
    // The latest user turn is passed separately.
    let earlier = history.split_last().map_or(&[][..], |(_, rest)| rest);
    let r = crate::plugins::router::pick(
        st.provider.as_ref(),
        plugins,
        earlier,
        &order.text,
        has_attachments,
        st.cfg.num_ctx,
    )
    .await;
    let latency_ms = r.latency.as_millis() as u64;
    tracing::info!(group = r.pick.label(), latency_ms, "router pick");
    if let Some(transcript) = &st.transcript
        && let Err(e) = transcript.record(
            client_id,
            &order.session_id,
            crate::transcript::Event::RouterPick {
                group: r.pick.label(),
                latency_ms,
            },
        )
    {
        tracing::error!(error = %e, "transcript log write failed");
    }
    r.pick
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
    "i created",
    "i've created",
    "i have created",
    "was created",
    "has been added",
    "was added",
    "i've added",
    "i have added",
    "has been saved",
    "was recorded",
];

/// A change described as waiting for approval (SPEC §11 #26). Observed:
/// "I prepared a proposal… Please confirm with \"approve\"" with no call.
const PROPOSAL_CLAIM_PATTERNS: &[&str] = &[
    "prepared a proposal",
    "i prepared",
    "i've prepared",
    "i have prepared",
    "confirm with",
    "reply with \"approve",
    "type \"approve",
    "say \"approve",
    "awaiting approval",
    "awaiting your approval",
    "must approve",
    "approval is required",
];

/// A fix promised instead of made (SPEC §11 #26). Observed: "I will correct
/// the call by removing those fields." with no call after a tool error.
const PROMISE_PATTERNS: &[&str] = &[
    "i will correct",
    "i'll correct",
    "i will retry",
    "i'll retry",
    "i will fix",
    "i'll fix",
    "i will remove",
    "i'll remove",
    "i will update",
    "i'll update",
    "i will call",
    "i'll call",
    "i will try again",
    "i'll try again",
    "let me correct",
    "let me retry",
    "let me fix",
];

fn claims_proposal(text: &str) -> bool {
    let lower = text.to_lowercase();
    PROPOSAL_CLAIM_PATTERNS.iter().any(|p| lower.contains(p))
}

fn promises_action(text: &str) -> bool {
    let lower = text.to_lowercase();
    PROMISE_PATTERNS.iter().any(|p| lower.contains(p))
}

/// A Final that describes something the order did not do (SPEC §11 #26).
#[derive(Clone, Copy)]
enum Unkept {
    Proposal,
    Promise,
}

impl Unkept {
    fn label(self) -> &'static str {
        match self {
            Unkept::Proposal => "proposal",
            Unkept::Promise => "promise",
        }
    }

    fn nudge(self) -> &'static str {
        match self {
            Unkept::Proposal => {
                "Nothing is waiting for approval: none was prepared in this turn. If you \
                 have what you need, call the tool now and the user gets approve/cancel \
                 buttons; otherwise ask for what is missing. Never ask the user to type \
                 approve."
            }
            Unkept::Promise => {
                "You said you would fix the call but made no call. Make the corrected call \
                 now, keeping every other value, or ask the user one question."
            }
        }
    }

    fn footer(self) -> &'static str {
        match self {
            Unkept::Proposal => "Nothing is waiting for your approval.",
            Unkept::Promise => "Nothing was done.",
        }
    }
}

/// True when a final answer asserts an action was executed but NO tool call
/// has run this order — the observed qwen3:4b failure mode. Only these
/// replies trigger the corrective nudge; ordinary replies (even ones that
/// list data, which one live false claim mimicked) pass through untouched.
#[cfg(test)]
fn claims_execution_without_tool(text: &str, dispatched_tool_calls: u32) -> bool {
    dispatched_tool_calls == 0 && claims_execution(text)
}

/// Success-claim language (SPEC §11 #16), case-insensitive.
fn claims_execution(text: &str) -> bool {
    let lower = text.to_lowercase();
    FALSE_CLAIM_PATTERNS.iter().any(|p| lower.contains(p))
}

/// A successful preview of a `destructive_when` tool goes straight to a
/// decision: the previewed arguments plus the policy's acting argument,
/// approve-gated like any destructive call (SPEC §11 #16, AGENTS.md #3).
/// The model never has to word the confirmation or repeat the arguments.
fn preview_decision(
    st: &Arc<AppState>,
    client_id: &str,
    session_id: &str,
    call: &crate::model::ToolCall,
    checks: &[String],
) -> Option<Response> {
    let (key, value) = st.policy.preview_of(&call.name, &call.args)?;
    let mut args = call.args.clone();
    args.as_object_mut()?.insert(key, value);
    tracing::info!(tool = %call.name, "preview dispatched; asking the user to apply it");
    let mut response = propose(st, client_id, session_id, &call.name, &args);
    response.text = grounding::decorate(&response.text, checks, &[]);
    Some(response)
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
    fn decision_details_come_from_args_and_schema_titles() {
        let schema = serde_json::json!({"type": "function", "function": {
            "name": "x.create", "parameters": {"type": "object", "properties": {
                "amount": {"type": "number", "title": "Amount"},
                "note": {"type": "string"}}}}});
        let args = serde_json::json!({"amount": 150.5, "note": "coffee", "apply": true});
        let rows: Vec<(String, String)> = decision_details(&args, Some(&schema))
            .into_iter()
            .map(|d| (d.label, d.value))
            .collect();
        assert!(rows.contains(&("Amount".into(), "150.5".into())));
        assert!(
            rows.contains(&("note".into(), "coffee".into())),
            "strings unquoted"
        );
        assert!(rows.contains(&("apply".into(), "true".into())));
        assert!(decision_details(&serde_json::json!(null), None).is_empty());
    }

    struct Schemas(Vec<serde_json::Value>);

    #[async_trait::async_trait]
    impl ToolExecutor for Schemas {
        async fn execute(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<serde_json::Value, ErrorCode> {
            Ok(serde_json::Value::Null)
        }
        fn tool_schemas(&self) -> &[serde_json::Value] {
            &self.0
        }
    }

    /// `ocr` declares `image` as its attachment param (no `_ref` suffix);
    /// `fin` declares none.
    fn plugin_set() -> PluginSet {
        let schema = |name: &str, props: serde_json::Value| {
            serde_json::json!({"type": "function", "function": {
                "name": name, "parameters": {"type": "object", "properties": props}}})
        };
        let manifest = |id: &str, extra: &str| {
            let text = format!(
                "arno_manifest = 1\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1\"\n\
                 provenance = \"first-party\"\n[server]\nkind = \"remote\"\nurl = \"http://x/mcp\"\n{extra}"
            );
            crate::plugins::Plugin {
                id: id.into(),
                manifest: Some(crate::plugins::manifest::Manifest::parse(&text, "t").unwrap()),
                hash: Some("h".into()),
                status: crate::plugins::PluginStatus::Ready,
            }
        };
        let exec = Arc::new(Schemas(vec![
            schema(
                "ocr.extract_text",
                serde_json::json!({"image": {"type": "string"}}),
            ),
            schema(
                "fin.import",
                serde_json::json!({"file_ref": {"type": "string"}}),
            ),
            schema(
                "fin.list_accounts",
                serde_json::json!({"limit": {"type": "integer"}}),
            ),
        ]));
        PluginSet::assemble(
            exec,
            vec![
                manifest("ocr", "[tools]\nattachment_params = [\"image\"]\n"),
                manifest("fin", ""),
            ],
        )
    }

    #[test]
    fn repair_points_mangled_handles_at_the_only_attachment() {
        let plugins = plugin_set();
        let handle = |h: &str| attachments::AttachmentHandle {
            handle: h.into(),
            mime: "image/jpeg".into(),
        };
        let call = |name: &str, args| crate::model::ToolCall {
            id: "c1".into(),
            name: name.into(),
            args,
        };
        let mut calls = vec![call(
            "ocr.extract_text",
            serde_json::json!({"image": "att_12x4", "n": 1}),
        )];
        repair_attachment_refs(&plugins, &mut calls, &[handle("att_1234")]);
        assert_eq!(
            calls[0].args,
            serde_json::json!({"image": "att_1234", "n": 1})
        );

        // Undeclared params are left alone, even with a `_ref` suffix.
        let mut calls = vec![call("fin.import", serde_json::json!({"file_ref": "x"}))];
        repair_attachment_refs(&plugins, &mut calls, &[handle("att_1234")]);
        assert_eq!(calls[0].args["file_ref"], "x");

        // Several attachments: never guess.
        let mut calls = vec![call(
            "ocr.extract_text",
            serde_json::json!({"image": "att_12x4"}),
        )];
        repair_attachment_refs(
            &plugins,
            &mut calls,
            &[handle("att_1234"), handle("att_5678")],
        );
        assert_eq!(calls[0].args["image"], "att_12x4");
    }

    #[test]
    fn attachment_notice_names_the_declared_param_but_no_tool() {
        let plugins = plugin_set();
        let handles = [attachments::AttachmentHandle {
            handle: "att_1".into(),
            mime: "image/jpeg".into(),
        }];
        let text = attachment_manifest(&plugins, &handles).unwrap();
        assert!(text.contains("that tool's `image` argument"), "{text}");
        assert!(!text.contains("ocr"));
        assert!(attachment_manifest(&plugins, &[]).is_none());
    }

    #[test]
    fn attachment_tools_keeps_only_declared_attachment_params() {
        let plugins = plugin_set();
        let names: Vec<_> = attachment_tools(&plugins, plugins.tool_schemas())
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec!["ocr.extract_text"]);
    }

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
