//! Contract 1 wire shapes pinned to fixtures (SPEC §4.1, AGENTS.md #2).
//! Each fixture must equal what the types serialize and must parse back.

use contract::*;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;

const ADDITIVE: &str = "Contract 1 is additive-only (SPEC §4.1): add new fields to the \
fixture; never rename or remove one";

fn fixture(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn pinned<T: Serialize + DeserializeOwned + PartialEq + Debug>(name: &str, value: &T) {
    let golden = fixture(name);
    let actual = serde_json::to_value(value).unwrap();
    assert_eq!(
        actual,
        golden,
        "{name}: {ADDITIVE}\nactual: {}",
        serde_json::to_string_pretty(&actual).unwrap()
    );
    let parsed: T = serde_json::from_value(golden).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(&parsed, value, "{name}: the fixture no longer parses back");
}

#[test]
fn order_minimal() {
    pinned(
        "order_minimal",
        &Order {
            session_id: "s1".into(),
            text: "hi".into(),
            client_msg_id: None,
            confirmation_token: None,
            attachments: Vec::new(),
            decision_answer: None,
        },
    );
}

#[test]
fn order_full() {
    pinned(
        "order_full",
        &Order {
            session_id: "chat:1".into(),
            text: "add this bill".into(),
            client_msg_id: Some("42".into()),
            confirmation_token: Some("dcs_legacy".into()),
            attachments: vec![
                Attachment {
                    mime: "image/png".into(),
                    data_b64: Some("aGVsbG8=".into()),
                    attachment_ref: None,
                },
                Attachment {
                    mime: "image/jpeg".into(),
                    data_b64: None,
                    attachment_ref: Some("file_1".into()),
                },
            ],
            decision_answer: Some(DecisionAnswer {
                id: "dcs_1".into(),
                value: Some("approve".into()),
                text: None,
            }),
        },
    );
}

#[test]
fn response_text() {
    pinned("response_text", &Response::text("hello"));
}

#[test]
fn response_decision() {
    pinned(
        "response_decision",
        &Response {
            text: "This would run `x.create`: Amount: 110. Approve or cancel it.".into(),
            needs_confirmation: Some(true),
            confirmation_token: Some("dcs_1".into()),
            structured: Some(vec![StructuredItem {
                tool: "x.list".into(),
                args: serde_json::json!({"limit": 1}),
                result: serde_json::json!({"items": []}),
            }]),
            decision: Some(Decision {
                id: "dcs_1".into(),
                kind: DecisionKind::Confirm,
                prompt: "This would run `x.create`.".into(),
                details: vec![DecisionDetail {
                    label: "Amount".into(),
                    value: "110".into(),
                }],
                options: vec![
                    DecisionOption {
                        value: "approve".into(),
                        title: "Approve".into(),
                        style: OptionStyle::Primary,
                    },
                    DecisionOption {
                        value: "reject".into(),
                        title: "Cancel".into(),
                        style: OptionStyle::Danger,
                    },
                ],
                free_text: true,
                expires_at: "2026-10-23T04:00:00Z".into(),
            }),
            order_id: None,
        },
    );
}

#[test]
fn response_order_id() {
    let mut response = Response::text("hello");
    response.order_id = Some("ord_1".into());
    pinned("response_order_id", &response);
}

#[test]
fn event_progress() {
    pinned(
        "event_progress",
        &Event {
            order_id: "ord_1".into(),
            session_id: "chat:1".into(),
            client_msg_id: Some("42".into()),
            kind: EventKind::Progress {
                step: "Reading the image…".into(),
            },
        },
    );
}

#[test]
fn event_done() {
    pinned(
        "event_done",
        &Event {
            order_id: "ord_1".into(),
            session_id: "chat:1".into(),
            client_msg_id: None,
            kind: EventKind::Done,
        },
    );
}

#[test]
fn an_unknown_event_type_is_ignored_not_rejected() {
    let event: Event = serde_json::from_value(serde_json::json!({
        "order_id": "ord_1", "session_id": "s1", "type": "reminder", "at": "later"}))
    .unwrap();
    assert_eq!(event.kind, EventKind::Unknown);
}

/// Every code: a new variant does not compile until it is listed here.
fn all_codes() -> Vec<ErrorCode> {
    use ErrorCode::*;
    let all = [
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
    ];
    for code in all {
        match code {
            Unauthorized | InvalidOrder | UnknownSession | DuplicateOrder | ConfirmationUnknown
            | ConfirmationExpired | ConfirmationUsed | OrderBudgetExceeded | BackendUnavailable
            | DecisionUnknown | DecisionExpired | DecisionUsed | DecisionInvalidValue => {}
        }
    }
    all.to_vec()
}

#[test]
fn error_codes_and_their_status() {
    let actual: Vec<serde_json::Value> = all_codes()
        .into_iter()
        .map(|code| {
            let mut body = serde_json::to_value(ErrorBody { error_code: code }).unwrap();
            assert_eq!(body["error_code"], code.as_str());
            body["status"] = code.status_code().into();
            body
        })
        .collect();
    assert_eq!(
        serde_json::Value::Array(actual.clone()),
        fixture("error_codes"),
        "error_codes: {ADDITIVE}; never repurpose a code\nactual: {}",
        serde_json::to_string_pretty(&actual).unwrap()
    );
    for body in actual {
        let parsed: ErrorBody = serde_json::from_value(body).unwrap();
        assert!(all_codes().contains(&parsed.error_code));
    }
}

#[test]
fn readiness() {
    pinned(
        "readiness",
        &Readiness {
            status: ReadyStatus::Degraded,
            issues: vec![issue::MODEL_ON_CPU.into(), issue::MODEL_TIMED_OUT.into()],
            model: ModelReadiness {
                name: "qwen3:4b".into(),
                reachable: true,
                gpu_share: Some(0.0),
                last_elapsed_ms: Some(180_000),
                last_timed_out: true,
            },
            plugins: vec![PluginReadiness {
                id: "ocr".into(),
                status: "ready".into(),
                trust: Some("trusted".into()),
            }],
        },
    );
}
