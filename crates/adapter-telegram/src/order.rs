//! Pure Order/Response mapping (SPEC §4.1) — no Telegram/HTTP types here so
//! error rendering stays unit-testable.
//!
//! Decisions (SPEC §4.3) are answered with `decision_answer`; the adapter
//! never sends `confirmation_token`.

use contract::{Attachment, DecisionAnswer, ErrorCode, Order};

/// Builds the next `Order` for a chat. `decision_answer` is set only when the
/// user tapped a decision button or typed the free-text answer.
pub fn build_order(
    session_id: String,
    client_msg_id: String,
    text: String,
    attachments: Vec<Attachment>,
    decision_answer: Option<DecisionAnswer>,
) -> Order {
    Order {
        session_id,
        text,
        client_msg_id: Some(client_msg_id),
        confirmation_token: None,
        attachments,
        decision_answer,
    }
}

/// Base64 encode without pulling in a base64 crate here — mirrors
/// `contract`'s own hand-rolled `decode` (same rationale: this is the only
/// place in the adapter that needs it). Standard alphabet, padded.
pub fn encode_base64(bytes: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(match b1 {
            Some(b1) => B64[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => B64[(b2 & 0x3f) as usize] as char,
            None => '=',
        });
    }
    out
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
        ErrorCode::DecisionUnknown => "This request is no longer known — ask again.".into(),
        ErrorCode::DecisionExpired => "This request expired — ask again.".into(),
        ErrorCode::DecisionUsed => "This request was already answered.".into(),
        ErrorCode::DecisionInvalidValue => "That option isn't valid for this request.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_carries_text_with_no_confirmation_token() {
        let order = build_order(
            "chat:1".into(),
            "42".into(),
            "hello".into(),
            Vec::new(),
            None,
        );
        assert_eq!(order.text, "hello");
        assert_eq!(order.session_id, "chat:1");
        assert_eq!(order.client_msg_id.as_deref(), Some("42"));
        assert_eq!(order.confirmation_token, None);
        assert!(order.attachments.is_empty());
    }

    #[test]
    fn order_carries_attachments_through_unchanged() {
        let att = Attachment {
            mime: "image/jpeg".into(),
            data_b64: Some(encode_base64(b"hello")),
            attachment_ref: None,
        };
        let order = build_order(
            "chat:1".into(),
            "42".into(),
            "".into(),
            vec![att.clone()],
            None,
        );
        assert_eq!(order.attachments, vec![att]);
    }

    #[test]
    fn base64_encode_round_trips_through_contract_decode() {
        for input in [&b""[..], b"h", b"he", b"hel", b"hello", b"hello world!"] {
            let att = Attachment {
                mime: "image/jpeg".into(),
                data_b64: Some(encode_base64(input)),
                attachment_ref: None,
            };
            assert_eq!(att.decoded_bytes().unwrap(), Some(input.to_vec()));
        }
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
            ErrorCode::DecisionUnknown,
            ErrorCode::DecisionExpired,
            ErrorCode::DecisionUsed,
            ErrorCode::DecisionInvalidValue,
        ];
        let mut seen = std::collections::HashSet::new();
        for code in codes {
            let text = render_error(code);
            assert!(!text.is_empty());
            assert!(seen.insert(text), "duplicate rendering for {code:?}");
        }
    }
}
