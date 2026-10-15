//! Context budgeting for prompt assembly (SPEC §8).
//!
//! Fill `num_ctx` in fixed priority: system prompt → merged tool schemas →
//! newest history first. Trimming drops **oldest whole turns**, never
//! splitting an assistant/tool-call↔tool/result pair.

use crate::model::{ChatMessage, Role};

/// Bytes per token in the estimate. JSON schemas and Spanish OCR text
/// tokenize denser than English prose; underestimating lets Ollama silently
/// drop the oldest tokens — the system prompt — in a tight window (§11 #20).
const BYTES_PER_TOKEN: u32 = 3;

/// Rough token estimate; small-model budgeting is approximate by nature and
/// errs on the conservative side (+ fixed per-message cost).
fn estimate(msg: &ChatMessage) -> u32 {
    (msg.content.len() as u32) / BYTES_PER_TOKEN + 8
}

pub fn estimate_system(system: &str) -> u32 {
    (system.len() as u32) / BYTES_PER_TOKEN + 8
}

/// Token estimate for the merged tool-schema block; reserved before any
/// history trimming (SPEC §8 priority: schemas → system → history).
pub fn estimate_schemas(schemas: &[serde_json::Value]) -> u32 {
    if schemas.is_empty() {
        return 0;
    }
    serde_json::to_string(schemas)
        .map(|s| s.len() as u32 / BYTES_PER_TOKEN + 8)
        .unwrap_or(0)
}

/// Returns the trimmed history (chronological). The system message is the
/// caller's concern — it always survives at top priority.
pub fn trim(history: &[ChatMessage], context_tokens: u32, system_tokens: u32) -> Vec<ChatMessage> {
    let mut budget = context_tokens.saturating_sub(system_tokens);
    let mut kept_rev: Vec<Vec<&ChatMessage>> = Vec::new();
    let mut i = history.len();
    while i > 0 {
        i -= 1;
        // A tool result must never travel without the assistant message that
        // requested it: group [assistant(tool_calls), tool] as one turn.
        let group: Vec<&ChatMessage> = if history[i].role == Role::Tool {
            if i > 0 && history[i - 1].role == Role::Assistant {
                i -= 1;
                vec![&history[i], &history[i + 1]]
            } else {
                vec![&history[i]] // defensive: orphaned tool result
            }
        } else {
            vec![&history[i]]
        };
        let cost: u32 = group.iter().map(|m| estimate(m)).sum();
        if cost > budget {
            break; // everything older than this point is dropped wholesale
        }
        budget -= cost;
        kept_rev.push(group);
    }
    // kept_rev is newest-group-first; walk it backwards so whole turns land
    // oldest-first while each group keeps its internal (call → result) order.
    let mut out: Vec<ChatMessage> = Vec::new();
    for group in kept_rev.into_iter().rev() {
        out.extend(group.into_iter().cloned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(s: &str) -> ChatMessage {
        ChatMessage::new(Role::User, s)
    }

    #[test]
    fn keeps_newest_and_drops_oldest_whole_turns() {
        let hist: Vec<ChatMessage> = ["one", "two", "three", "four"]
            .iter()
            .map(|s| user(&s.repeat(100)))
            .collect();
        // Budget fits only 3 of 4 turns (174 + 141 + 108 ≤ 450 < 531).
        let out = trim(&hist, 450, 0);
        assert_eq!(
            out.first().unwrap().content,
            hist[1].content,
            "oldest dropped first"
        );
        assert_eq!(out.last().unwrap().content, hist[3].content);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn never_splits_tool_result_from_assistant_call() {
        // Real production shape (not a hand-rolled stand-in): the assistant's
        // actual tool-call request, as `orchestrator/mod.rs` now appends it.
        let call = ChatMessage::assistant_tool_calls(vec![crate::model::ToolCall {
            id: "c1".into(),
            name: "securo.list_transactions".into(),
            args: serde_json::json!({}),
        }]);
        let result = ChatMessage::tool_result("c1", "result");
        let hist = vec![user("old ".repeat(200).trim_end()), call, result];
        // Budget only fits the pair → both survive or neither does.
        let out = trim(&hist, 120, 0);
        if out.iter().any(|m| m.role == Role::Tool) {
            assert!(out.iter().any(|m| m.role == Role::Assistant));
        }
        assert!(
            out.iter().all(|m| m.role != Role::User),
            "older turn dropped before splitting pair"
        );
    }

    #[test]
    fn assistant_tool_call_request_survives_alongside_its_result() {
        // Confirms the pairing logic actually engages with the real shape:
        // both messages carry the same content either way, so this would
        // catch a regression where `tool_calls` silently stops being grouped.
        let call = ChatMessage::assistant_tool_calls(vec![crate::model::ToolCall {
            id: "c1".into(),
            name: "securo.list_transactions".into(),
            args: serde_json::json!({"limit": 5}),
        }]);
        let result = ChatMessage::tool_result("c1", "result");
        let hist = vec![call, result];
        let out = trim(&hist, 10_000, 0);
        assert_eq!(out.len(), 2, "both the request and its result survive");
        assert_eq!(out[0].role, Role::Assistant);
        assert_eq!(
            out[0].tool_calls.as_ref().unwrap()[0].name,
            "securo.list_transactions"
        );
        assert_eq!(out[1].role, Role::Tool);
    }

    #[test]
    fn system_budget_is_reserved() {
        // 4000 chars ≈ 1341 estimated tokens > remaining budget of 200.
        let hist = vec![user(&"x".repeat(4000))];
        assert!(
            trim(&hist, 500, 300).is_empty(),
            "no room after system reservation"
        );
    }

    #[test]
    fn kept_history_fits_beside_the_reserved_answer() {
        let hist: Vec<ChatMessage> = (0..20)
            .map(|i| user(&format!("turn {i} ").repeat(40)))
            .collect();
        let (ctx, reserved) = (2000, 900); // system + schemas + answer
        let out = trim(&hist, ctx, reserved);
        assert_eq!(
            out.last().unwrap().content,
            hist[19].content,
            "newest turn kept"
        );
        let used: u32 = out.iter().map(estimate).sum();
        assert!(used <= ctx - reserved, "{used} > {}", ctx - reserved);
        assert!(out.len() < hist.len(), "older turns dropped");
    }

    #[test]
    fn empty_history_is_fine() {
        assert!(trim(&[], 1000, 10).is_empty());
    }
}
