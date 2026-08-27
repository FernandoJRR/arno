//! Context budgeting for prompt assembly (SPEC §8).
//!
//! Fill `num_ctx` in fixed priority: system prompt → merged tool schemas →
//! newest history first. Trimming drops **oldest whole turns**, never
//! splitting an assistant/tool-call↔tool/result pair.

use crate::model::{ChatMessage, Role};

/// Rough token estimate; small-model budgeting is approximate by nature and
/// errs on the conservative side (4 chars ≈ 1 token + fixed per-message cost).
fn estimate(msg: &ChatMessage) -> u32 {
    (msg.content.len() as u32) / 4 + 8
}

pub fn estimate_system(system: &str) -> u32 {
    (system.len() as u32) / 4 + 8
}

/// Token estimate for the merged tool-schema block; reserved before any
/// history trimming (SPEC §8 priority: schemas → system → history).
pub fn estimate_schemas(schemas: &[serde_json::Value]) -> u32 {
    if schemas.is_empty() {
        return 0;
    }
    serde_json::to_string(schemas)
        .map(|s| s.len() as u32 / 4 + 8)
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
        // Budget fits only ~3 of 4 turns.
        let out = trim(&hist, 400, 0);
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
        let mut call = ChatMessage::new(Role::Assistant, "calling");
        call.tool_call_id = Some("c1".into());
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
    fn system_budget_is_reserved() {
        // 4000 chars ≈ 1008 estimated tokens > remaining budget of 200.
        let hist = vec![user(&"x".repeat(4000))];
        assert!(
            trim(&hist, 500, 300).is_empty(),
            "no room after system reservation"
        );
    }

    #[test]
    fn empty_history_is_fine() {
        assert!(trim(&[], 1000, 10).is_empty());
    }
}
