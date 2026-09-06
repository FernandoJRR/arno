//! Pure Order/Response mapping (SPEC §4.1) — no Telegram/HTTP types here so
//! error rendering stays unit-testable.
//!
//! Confirmation (SPEC §4.3 revised) is now the harness's job — every order
//! here just carries the message text with `confirmation_token: None`.

use contract::{ErrorCode, Order};

/// Builds the next `Order` for a chat — the harness decides on its own
/// whether it confirms anything pending.
pub fn build_order(session_id: String, client_msg_id: String, text: String) -> Order {
    Order {
        session_id,
        text,
        client_msg_id: Some(client_msg_id),
        confirmation_token: None,
        attachments: Vec::new(),
    }
}

/// Terse, code-derived text only — never the underlying payload (AGENTS.md
/// logging rule, SPEC backlog #2: tool results/model context carry finance
/// data).
pub fn render_error(code: ErrorCode) -> String {
    match code {
        ErrorCode::Unauthorized => "Not authorized to reach the harness.".into(),
        ErrorCode::InvalidOrder => "That message couldn't be sent as a valid order.".into(),
        ErrorCode::UnknownSession => "Session not found — try again.".into(),
        ErrorCode::DuplicateOrder => "Already processing that — please wait.".into(),
        ErrorCode::ConfirmationUnknown => "That confirmation wasn't recognized.".into(),
        ErrorCode::ConfirmationExpired => "That confirmation expired — resend your request.".into(),
        ErrorCode::ConfirmationUsed => "That confirmation was already used.".into(),
        ErrorCode::OrderBudgetExceeded => "That took too long and was stopped.".into(),
        ErrorCode::BackendUnavailable => "A required backend is unavailable right now.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_carries_text_with_no_confirmation_token() {
        let order = build_order("chat:1".into(), "42".into(), "hello".into());
        assert_eq!(order.text, "hello");
        assert_eq!(order.session_id, "chat:1");
        assert_eq!(order.client_msg_id.as_deref(), Some("42"));
        assert_eq!(order.confirmation_token, None);
    }

    #[test]
    fn every_error_code_renders_distinct_terse_text() {
        let codes = [
            ErrorCode::Unauthorized,
            ErrorCode::InvalidOrder,
            ErrorCode::UnknownSession,
            ErrorCode::DuplicateOrder,
            ErrorCode::ConfirmationUnknown,
            ErrorCode::ConfirmationExpired,
            ErrorCode::ConfirmationUsed,
            ErrorCode::OrderBudgetExceeded,
            ErrorCode::BackendUnavailable,
        ];
        let mut seen = std::collections::HashSet::new();
        for code in codes {
            let text = render_error(code);
            assert!(!text.is_empty());
            assert!(seen.insert(text), "duplicate rendering for {code:?}");
        }
    }
}
