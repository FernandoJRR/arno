//! Contract 1 wire types (SPEC §4.1).
//!
//! This crate is the shared vocabulary between the harness core and every
//! interface adapter. Rules:
//! - Additive-only under `/v1`: never remove or repurpose a field or error code.
//! - Unknown fields on input MUST be ignored (serde default behavior), so an
//!   older core tolerates a newer adapter. Do NOT add `deny_unknown_fields`.
//! - No logic beyond validation; no transport dependencies (axum lives in the
//!   harness, which maps [`ErrorCode`] to HTTP status codes).

use serde::{Deserialize, Serialize};

/// Inbound order (SPEC §4.1). `text` may be empty when the order exists only
/// to redeem a `confirmation_token`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub session_id: String,
    #[serde(default)]
    pub text: String,
    /// Stable identifier of the native input (e.g. Telegram `update_id`).
    /// Adapters SHOULD send it; the harness dedups within `DEDUP_WINDOW_MIN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
    /// Presence switches the harness to the redemption path (SPEC §4.3):
    /// the stored frozen payload is dispatched verbatim, model bypassed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// Optional binary transport (SPEC §4.1): exactly one of `data_b64` / `ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_b64: Option<String>,
    /// Opaque handle for the future `POST /v1/files` upload endpoint.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub attachment_ref: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("attachment must carry exactly one of data_b64/ref")]
    AttachmentAmbiguous,
    #[error("attachment data_b64 is not valid base64")]
    AttachmentNotBase64,
}

impl Attachment {
    /// Validates the exactly-one-of invariant and decodes inline data to size it.
    /// Returns the raw byte length when `data_b64` is present.
    pub fn validate(&self) -> Result<Option<usize>, ContractError> {
        match (&self.data_b64, &self.attachment_ref) {
            (Some(_), Some(_)) | (None, None) => Err(ContractError::AttachmentAmbiguous),
            (Some(b64), None) => {
                let len = decode_len(b64)?;
                Ok(Some(len))
            }
            (None, Some(_)) => Ok(None),
        }
    }
}

/// Base64 decode without pulling in a base64 crate here: the harness needs the
/// decoded length only, and lenient decoding of the standard alphabet with
/// padding is enough for sizing. Returns decoded byte count.
fn decode_len(b64: &str) -> Result<usize, ContractError> {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let clean: Vec<u8> = b64.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let unpadded = clean.iter().filter(|&&b| b != b'=').count();
    if !clean.len().is_multiple_of(4) || !clean[unpadded..].iter().all(|&b| b == b'=') {
        return Err(ContractError::AttachmentNotBase64);
    }
    let mut acc: u32 = 0;
    let mut bits = 0_u32;
    let mut out = 0_usize;
    for &b in &clean[..unpadded] {
        let v = B64
            .iter()
            .position(|&c| c == b)
            .ok_or(ContractError::AttachmentNotBase64)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out += 1;
            let _ = (acc >> bits) as u8; // value consumed; we only need the count
        }
    }
    Ok(out)
}

/// Seed of the future audit log (SPEC §4.3): what executed, verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructuredItem {
    pub tool: String,
    pub args: serde_json::Value,
    pub result: serde_json::Value,
}

/// Outbound response (SPEC §4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_confirmation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Vec<StructuredItem>>,
}

impl Response {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            needs_confirmation: None,
            confirmation_token: None,
            structured: None,
        }
    }
}

/// Machine-readable failure codes (SPEC §4.1). Every non-2xx carries exactly
/// one of these as `{"error_code": ...}` so all adapters render identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    InvalidOrder,
    UnknownSession,
    DuplicateOrder,
    ConfirmationUnknown,
    ConfirmationExpired,
    ConfirmationUsed,
    OrderBudgetExceeded,
    BackendUnavailable,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::InvalidOrder => "invalid_order",
            ErrorCode::UnknownSession => "unknown_session",
            ErrorCode::DuplicateOrder => "duplicate_order",
            ErrorCode::ConfirmationUnknown => "confirmation_unknown",
            ErrorCode::ConfirmationExpired => "confirmation_expired",
            ErrorCode::ConfirmationUsed => "confirmation_used",
            ErrorCode::OrderBudgetExceeded => "order_budget_exceeded",
            ErrorCode::BackendUnavailable => "backend_unavailable",
        }
    }

    /// HTTP status mapping owned by the harness (STACK.md §5); contract stays
    /// transport-neutral. Documented mapping:
    /// 401 unauthorized · 400 invalid_order · 404 unknown_session /
    /// confirmation_unknown · 409 duplicate_order / confirmation_used ·
    /// 410 confirmation_expired · 504 order_budget_exceeded ·
    /// 503 backend_unavailable.
    pub fn status_code(&self) -> u16 {
        match self {
            ErrorCode::Unauthorized => 401,
            ErrorCode::InvalidOrder => 400,
            ErrorCode::UnknownSession | ErrorCode::ConfirmationUnknown => 404,
            ErrorCode::DuplicateOrder | ErrorCode::ConfirmationUsed => 409,
            ErrorCode::ConfirmationExpired => 410,
            ErrorCode::OrderBudgetExceeded => 504,
            ErrorCode::BackendUnavailable => 503,
        }
    }
}

/// Non-2xx body shape: `{ "error_code": "..." }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error_code: ErrorCode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_roundtrip_and_unknown_fields_ignored() {
        let order = Order {
            session_id: "s1".into(),
            text: "hi".into(),
            client_msg_id: Some("42".into()),
            confirmation_token: None,
            attachments: vec![],
        };
        let json = serde_json::to_string(&order).unwrap();
        assert!(!json.contains("confirmation_token"));
        // Newer adapter sends an extra field — older core must tolerate it.
        let tolerant = r#"{"session_id":"s1","text":"x","future_field":123}"#;
        let parsed: Order = serde_json::from_str(tolerant).unwrap();
        assert_eq!(parsed.session_id, "s1");
        assert_eq!(parsed.text, "x");
        let back: Order = serde_json::from_str(&json).unwrap();
        assert_eq!(back, order);
    }

    #[test]
    fn attachment_exactly_one_of() {
        let both = Attachment {
            mime: "image/png".into(),
            data_b64: Some("aGVsbG8=".into()),
            attachment_ref: Some("h1".into()),
        };
        assert_eq!(both.validate(), Err(ContractError::AttachmentAmbiguous));
        let neither = Attachment {
            mime: "image/png".into(),
            data_b64: None,
            attachment_ref: None,
        };
        assert_eq!(neither.validate(), Err(ContractError::AttachmentAmbiguous));
        let inline = Attachment {
            mime: "text/plain".into(),
            data_b64: Some("aGVsbG8=".into()),
            attachment_ref: None,
        };
        assert_eq!(inline.validate().unwrap(), Some(5)); // "hello"
        let by_ref = Attachment {
            mime: "application/pdf".into(),
            data_b64: None,
            attachment_ref: Some("h9".into()),
        };
        assert_eq!(by_ref.validate().unwrap(), None);
    }

    #[test]
    fn error_codes_serialize_stably() {
        assert_eq!(ErrorCode::DuplicateOrder.as_str(), "duplicate_order");
        let body = serde_json::to_string(&ErrorBody {
            error_code: ErrorCode::OrderBudgetExceeded,
        })
        .unwrap();
        assert_eq!(body, r#"{"error_code":"order_budget_exceeded"}"#);
        let parsed: ErrorBody = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.error_code, ErrorCode::OrderBudgetExceeded);
    }

    #[test]
    fn status_mapping_is_documented() {
        assert_eq!(ErrorCode::Unauthorized.status_code(), 401);
        assert_eq!(ErrorCode::ConfirmationExpired.status_code(), 410);
        assert_eq!(ErrorCode::BackendUnavailable.status_code(), 503);
    }

    #[test]
    fn redemption_order_can_have_empty_text() {
        let json = r#"{"session_id":"s","text":"","confirmation_token":"abc"}"#;
        let parsed: Order = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.confirmation_token.as_deref(), Some("abc"));
    }
}
