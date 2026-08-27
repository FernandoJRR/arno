//! Pure Order/Response mapping (SPEC §4.1, §4.3) — no Telegram/HTTP types here
//! so the confirmation round trip and error rendering stay unit-testable.

use contract::{ErrorCode, Order, Response};

/// The literal reply that redeems a pending `confirmation_token` (SPEC §4.3).
pub fn is_confirm_phrase(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("[confirm]")
}

/// Builds the next `Order` for a chat. `pending_token` is `Some` only when the
/// incoming text is the `[CONFIRM]` phrase and a token is on file for this
/// chat — in that case the order redeems it verbatim with empty `text`
/// (SPEC §4.3); otherwise it's a fresh order carrying the message text.
pub fn build_order(
    session_id: String,
    client_msg_id: String,
    text: String,
    pending_token: Option<String>,
) -> Order {
    match pending_token {
        Some(token) => Order {
            session_id,
            text: String::new(),
            client_msg_id: Some(client_msg_id),
            confirmation_token: Some(token),
            attachments: Vec::new(),
        },
        None => Order {
            session_id,
            text,
            client_msg_id: Some(client_msg_id),
            confirmation_token: None,
            attachments: Vec::new(),
        },
    }
}

/// Renders a successful `Response` to display text, plus a confirmation token
/// to remember for this chat when one was minted (SPEC §4.3).
pub fn render_response(resp: &Response) -> (String, Option<String>) {
    let mut text = resp.text.clone();
    let mut pending = None;
    if resp.needs_confirmation == Some(true)
        && let Some(token) = &resp.confirmation_token
    {
        text.push_str("\n\nReply [CONFIRM] to proceed.");
        pending = Some(token.clone());
    }
    (text, pending)
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
    fn confirm_phrase_is_case_and_whitespace_insensitive() {
        assert!(is_confirm_phrase("[CONFIRM]"));
        assert!(is_confirm_phrase("  [confirm]  "));
        assert!(!is_confirm_phrase("confirm"));
        assert!(!is_confirm_phrase("[confirm] please"));
    }

    #[test]
    fn fresh_order_carries_text_and_no_token() {
        let order = build_order("chat:1".into(), "42".into(), "hello".into(), None);
        assert_eq!(order.text, "hello");
        assert_eq!(order.session_id, "chat:1");
        assert_eq!(order.client_msg_id.as_deref(), Some("42"));
        assert_eq!(order.confirmation_token, None);
    }

    #[test]
    fn redemption_order_has_empty_text_and_carries_token() {
        let order = build_order(
            "chat:1".into(),
            "43".into(),
            "[CONFIRM]".into(),
            Some("tok-abc".into()),
        );
        assert_eq!(order.text, "");
        assert_eq!(order.confirmation_token.as_deref(), Some("tok-abc"));
    }

    #[test]
    fn response_without_confirmation_yields_no_pending_token() {
        let (text, pending) = render_response(&Response::text("done"));
        assert_eq!(text, "done");
        assert_eq!(pending, None);
    }

    #[test]
    fn response_needing_confirmation_appends_prompt_and_returns_token() {
        let resp = Response {
            text: "about to delete something".into(),
            needs_confirmation: Some(true),
            confirmation_token: Some("tok-xyz".into()),
            structured: None,
        };
        let (text, pending) = render_response(&resp);
        assert!(text.starts_with("about to delete something"));
        assert!(text.contains("[CONFIRM]"));
        assert_eq!(pending.as_deref(), Some("tok-xyz"));
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
