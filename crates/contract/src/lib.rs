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
    /// Answer to a `Response.decision` (SPEC §4.3). Takes precedence over
    /// `confirmation_token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_answer: Option<DecisionAnswer>,
}

/// Human choice requested by the harness (SPEC §4.3). The core owns the
/// options; adapters only choose how to render them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub kind: DecisionKind,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<DecisionDetail>,
    pub options: Vec<DecisionOption>,
    /// Whether a free-text answer is accepted. Free text never approves.
    #[serde(default)]
    pub free_text: bool,
    /// RFC 3339.
    pub expires_at: String,
}

/// `form` is reserved for MCP elicitation passthrough; only `confirm` is
/// emitted today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Confirm,
    Choose,
    Form,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionDetail {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub value: String,
    pub title: String,
    #[serde(default)]
    pub style: OptionStyle,
}

/// Rendering hint only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionStyle {
    Primary,
    Danger,
    #[default]
    Default,
}

/// Exactly one of `value` (an offered option) / `text` (free text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionAnswer {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl DecisionAnswer {
    pub fn validate(&self) -> Result<(), ContractError> {
        match (&self.value, &self.text) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            _ => Err(ContractError::DecisionAnswerAmbiguous),
        }
    }
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
    #[error("decision_answer must carry exactly one of value/text")]
    DecisionAnswerAmbiguous,
}

impl Attachment {
    /// Validates the exactly-one-of invariant and decodes inline data to size it.
    /// Returns the raw byte length when `data_b64` is present.
    pub fn validate(&self) -> Result<Option<usize>, ContractError> {
        match (&self.data_b64, &self.attachment_ref) {
            (Some(_), Some(_)) | (None, None) => Err(ContractError::AttachmentAmbiguous),
            (Some(b64), None) => Ok(Some(decode(b64)?.len())),
            (None, Some(_)) => Ok(None),
        }
    }

    /// Decodes `data_b64` to raw bytes; `None` for `ref`-form attachments
    /// (nothing inline to decode). Used by the harness to persist inbound
    /// images to the attachment scratch store (SPEC §4.1).
    pub fn decoded_bytes(&self) -> Result<Option<Vec<u8>>, ContractError> {
        match &self.data_b64 {
            Some(b64) => decode(b64).map(Some),
            None => Ok(None),
        }
    }
}

/// Base64 decode without pulling in a base64 crate here: lenient decoding of
/// the standard alphabet with padding, used both to size an attachment and
/// (harness-side) to recover its bytes.
fn decode(b64: &str) -> Result<Vec<u8>, ContractError> {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let clean: Vec<u8> = b64.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let unpadded = clean.iter().filter(|&&b| b != b'=').count();
    if !clean.len().is_multiple_of(4) || !clean[unpadded..].iter().all(|&b| b == b'=') {
        return Err(ContractError::AttachmentNotBase64);
    }
    let mut acc: u32 = 0;
    let mut bits = 0_u32;
    let mut out = Vec::with_capacity(unpadded * 3 / 4);
    for &b in &clean[..unpadded] {
        let v = B64
            .iter()
            .position(|&c| c == b)
            .ok_or(ContractError::AttachmentNotBase64)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
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
    /// When present, the order ended waiting on a human choice (SPEC §4.3).
    /// For `kind: confirm` the legacy `needs_confirmation`/`confirmation_token`
    /// are also set, with `confirmation_token == decision.id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
}

impl Response {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            needs_confirmation: None,
            confirmation_token: None,
            structured: None,
            decision: None,
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
    DecisionUnknown,
    DecisionExpired,
    DecisionUsed,
    DecisionInvalidValue,
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
            ErrorCode::DecisionUnknown => "decision_unknown",
            ErrorCode::DecisionExpired => "decision_expired",
            ErrorCode::DecisionUsed => "decision_used",
            ErrorCode::DecisionInvalidValue => "decision_invalid_value",
        }
    }

    /// HTTP status mapping owned by the harness (STACK.md §5); contract stays
    /// transport-neutral. Documented mapping:
    /// 401 unauthorized · 400 invalid_order / decision_invalid_value ·
    /// 404 unknown_session / confirmation_unknown / decision_unknown ·
    /// 409 duplicate_order / confirmation_used / decision_used ·
    /// 410 confirmation_expired / decision_expired · 504 order_budget_exceeded ·
    /// 503 backend_unavailable.
    pub fn status_code(&self) -> u16 {
        match self {
            ErrorCode::Unauthorized => 401,
            ErrorCode::InvalidOrder | ErrorCode::DecisionInvalidValue => 400,
            ErrorCode::UnknownSession
            | ErrorCode::ConfirmationUnknown
            | ErrorCode::DecisionUnknown => 404,
            ErrorCode::DuplicateOrder | ErrorCode::ConfirmationUsed | ErrorCode::DecisionUsed => {
                409
            }
            ErrorCode::ConfirmationExpired | ErrorCode::DecisionExpired => 410,
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

/// `GET /v1/ready` body (SPEC §11 #31): whether orders can be served now.
/// Names and codes only, never payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Readiness {
    pub status: ReadyStatus,
    /// Codes from [`issue`]; adapters ignore codes they do not know.
    #[serde(default)]
    pub issues: Vec<String>,
    pub model: ModelReadiness,
    #[serde(default)]
    pub plugins: Vec<PluginReadiness>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyStatus {
    Ready,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelReadiness {
    pub name: String,
    pub reachable: bool,
    /// Share of the loaded model held on a GPU; absent when unknown or not
    /// loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_share: Option<f32>,
    /// The last order's model call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_elapsed_ms: Option<u64>,
    #[serde(default)]
    pub last_timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginReadiness {
    pub id: String,
    /// `ready` or `failed`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<String>,
}

/// Readiness issue codes (SPEC §11 #31). Additive, never repurposed.
pub mod issue {
    pub const MODEL_UNREACHABLE: &str = "model_unreachable";
    pub const MODEL_ON_CPU: &str = "model_on_cpu";
    pub const MODEL_PARTLY_ON_CPU: &str = "model_partly_on_cpu";
    pub const MODEL_TIMED_OUT: &str = "model_timed_out";
    pub const PLUGIN_FAILED: &str = "plugin_failed";
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
            decision_answer: None,
        };
        let json = serde_json::to_string(&order).unwrap();
        assert!(!json.contains("confirmation_token"));
        assert!(!json.contains("decision_answer"));
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
    fn decoded_bytes_roundtrips_inline_data() {
        let inline = Attachment {
            mime: "image/png".into(),
            data_b64: Some("aGVsbG8=".into()),
            attachment_ref: None,
        };
        assert_eq!(inline.decoded_bytes().unwrap(), Some(b"hello".to_vec()));

        let by_ref = Attachment {
            mime: "image/png".into(),
            data_b64: None,
            attachment_ref: Some("h1".into()),
        };
        assert_eq!(by_ref.decoded_bytes().unwrap(), None);

        let bad = Attachment {
            mime: "image/png".into(),
            data_b64: Some("not-valid-base64!!".into()),
            attachment_ref: None,
        };
        assert_eq!(bad.decoded_bytes(), Err(ContractError::AttachmentNotBase64));
    }

    #[test]
    fn decision_roundtrips_and_is_absent_when_none() {
        let resp = Response {
            decision: Some(Decision {
                id: "dcs_1".into(),
                kind: DecisionKind::Confirm,
                prompt: "Run `x`?".into(),
                details: vec![DecisionDetail {
                    label: "amount".into(),
                    value: "42".into(),
                }],
                options: vec![
                    DecisionOption {
                        value: "approve".into(),
                        title: "Confirm".into(),
                        style: OptionStyle::Primary,
                    },
                    DecisionOption {
                        value: "reject".into(),
                        title: "Cancel".into(),
                        style: OptionStyle::Danger,
                    },
                ],
                free_text: true,
                expires_at: "2026-10-14T00:00:00Z".into(),
            }),
            ..Response::text("t")
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["decision"]["kind"], "confirm");
        assert_eq!(json["decision"]["options"][1]["style"], "danger");
        let back: Response = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);

        let plain = serde_json::to_string(&Response::text("t")).unwrap();
        assert!(!plain.contains("decision"));
    }

    #[test]
    fn decision_answer_exactly_one_of() {
        let ans = |value: Option<&str>, text: Option<&str>| DecisionAnswer {
            id: "dcs_1".into(),
            value: value.map(Into::into),
            text: text.map(Into::into),
        };
        assert_eq!(ans(Some("approve"), None).validate(), Ok(()));
        assert_eq!(ans(None, Some("make it 200")).validate(), Ok(()));
        assert_eq!(
            ans(Some("approve"), Some("x")).validate(),
            Err(ContractError::DecisionAnswerAmbiguous)
        );
        assert_eq!(
            ans(None, None).validate(),
            Err(ContractError::DecisionAnswerAmbiguous)
        );
        let parsed: Order = serde_json::from_str(
            r#"{"session_id":"s","decision_answer":{"id":"dcs_1","value":"reject"}}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.decision_answer.unwrap().value.as_deref(),
            Some("reject")
        );
    }

    #[test]
    fn decision_error_codes_are_stable() {
        for (code, s, status) in [
            (ErrorCode::DecisionUnknown, "decision_unknown", 404),
            (ErrorCode::DecisionExpired, "decision_expired", 410),
            (ErrorCode::DecisionUsed, "decision_used", 409),
            (
                ErrorCode::DecisionInvalidValue,
                "decision_invalid_value",
                400,
            ),
        ] {
            assert_eq!(code.as_str(), s);
            assert_eq!(code.status_code(), status);
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{s}\""));
        }
    }

    #[test]
    fn redemption_order_can_have_empty_text() {
        let json = r#"{"session_id":"s","text":"","confirmation_token":"abc"}"#;
        let parsed: Order = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.confirmation_token.as_deref(), Some("abc"));
    }
}
