//! Harness API contract tests (SPEC §4.1, STACK.md §8).
//!
//! The model is a wiremock double; the tool executor is a scripted fake.
//! No test here (or anywhere) touches a live Ollama or Telegram.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use harness::config::Config;
use harness::model::{CompletionOutput, CompletionRequest, ModelError, ModelProvider};
use harness::orchestrator::ToolExecutor;
use harness::state::{AppState, SharedState};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A fresh, never-colliding path under the OS temp dir — tests run in
/// parallel and each needs its own policy/audit file.
fn unique_temp_path(tag: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "arno-api-contract-test-{tag}-{}-{n}",
        std::process::id()
    ))
}

fn test_config(ollama_url: String, confirm_ttl: Duration) -> Config {
    let mut client_tokens = HashMap::new();
    client_tokens.insert("cli".to_owned(), "tok-cli".to_owned());
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        client_tokens,
        ollama_url: ollama_url.parse().unwrap(),
        model: "qwen3:8b".into(),
        num_ctx: 32_768,
        ollama_think: harness::model::ollama::ThinkMode::Off,
        ollama_timeout: Duration::from_secs(5),
        mcp_tool_timeout: Duration::from_secs(5),
        order_budget: Duration::from_secs(30),
        max_tool_calls: 8,
        dedup_window: Duration::from_secs(60),
        session_ttl: Duration::from_secs(3600),
        confirm_ttl,
        attach_max_bytes: 64,
        destructive_tools: ["securo.create_transaction".to_owned()]
            .into_iter()
            .collect(),
        mcp_servers: Vec::new(),
        tool_policy_path: unique_temp_path("policy.json"),
        audit_log_path: unique_temp_path("audit.jsonl"),
        tool_policy_retry: Duration::from_secs(300),
    }
}

struct FixedProvider(&'static str);

#[async_trait::async_trait]
impl ModelProvider for FixedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        Ok(CompletionOutput::Final(self.0.to_owned()))
    }
}

#[derive(Clone)]
enum ExecBehavior {
    Ok(serde_json::Value),
    Err(contract::ErrorCode),
}

#[derive(Clone)]
struct RecordingExecutor {
    calls: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    behavior: ExecBehavior,
}

impl RecordingExecutor {
    fn new(behavior: ExecBehavior) -> Self {
        Self {
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            behavior,
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for RecordingExecutor {
    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, contract::ErrorCode> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), args.clone()));
        match &self.behavior {
            ExecBehavior::Ok(v) => Ok(v.clone()),
            ExecBehavior::Err(code) => Err(*code),
        }
    }
}

fn build_state(
    cfg: Config,
    provider: Arc<dyn ModelProvider>,
    executor: Arc<dyn ToolExecutor>,
) -> SharedState {
    let (order_tx, order_rx) = tokio::sync::mpsc::channel(16);
    // Same topology as production main(): a fresh policy/audit pair per test,
    // at the same unique temp paths test_config() already generated.
    let policy = Arc::new(harness::policy::ToolPolicy::load(
        cfg.tool_policy_path.clone(),
        cfg.destructive_tools.clone(),
    ));
    let audit =
        harness::audit::AuditLog::open(&cfg.audit_log_path).expect("audit log opens in tests");
    let state: SharedState = Arc::new(AppState {
        sessions: harness::stores::sessions::SessionStore::new(),
        dedup: harness::stores::dedup::DedupStore::new(),
        pending: harness::stores::pending::PendingStore::new(),
        provider,
        executor,
        cfg,
        order_tx,
        policy,
        audit: Some(audit),
    });
    // Same topology as production main(): a live FIFO worker consumes jobs.
    tokio::spawn(harness::queue::spawn_worker(state.clone(), order_rx));
    state
}

async fn post_order(
    router: axum::Router,
    body: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/orders")
        .header("content-type", "application/json")
        .header(
            "authorization",
            token.map(|t| format!("Bearer {t}")).unwrap_or_default(),
        )
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

const ORDER_BODY: &str = r#"{"session_id":"s1","text":"hello","client_msg_id":"m1"}"#;

// ---- transport & auth ----

#[tokio::test]
async fn health_is_open_and_ok() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let resp = api_router(state)
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

fn api_router(state: SharedState) -> axum::Router {
    harness::api::routes::router(state)
}

#[tokio::test]
async fn missing_token_is_unauthorized_with_contract_body() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let (status, body) = post_order(api_router(state), ORDER_BODY, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error_code"], "unauthorized");
}

#[tokio::test]
async fn wrong_token_is_unauthorized() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("nope")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error_code"], "unauthorized");
}

// ---- normal path ----

#[tokio::test]
async fn happy_path_round_trips_through_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "pong"},
            "done": true
        })))
        .mount(&server)
        .await;

    // Real OllamaProvider against the wiremock double exercises the adapter impl.
    let provider = Arc::new(harness::model::ollama::OllamaProvider::new(
        server.uri().parse().unwrap(),
        "qwen3:8b",
        Duration::from_secs(5),
        harness::model::ollama::ThinkMode::Off,
    ));
    let state = build_state(
        test_config(server.uri(), Duration::from_secs(10)),
        provider,
        Arc::new(harness::orchestrator::NoBackends),
    );
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "pong");
    assert_eq!(
        body["needs_confirmation"],
        serde_json::Value::Null,
        "absent fields stay absent"
    );
}

// ---- adaptive classification at the freeze branch (SPEC §4.2, M3) ----
//
// These are the first tests to exercise `normal_path`'s tool-call branch at
// all: every other test either takes the `Final` shortcut or seeds the
// pending store directly (`redemption_rig`), bypassing the classify-then-
// freeze-or-dispatch decision entirely.

fn tool_call_response(name: &str, arguments: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "call_1", "function": {"name": name, "arguments": arguments}}]
        },
        "done": true
    })
}

/// A scripted classifier verdict, reused instead of a real Ollama round trip
/// — the classifier itself is unit-tested in `policy.rs`; here it only needs
/// to seed one known rule the way `main.rs`'s boot sequence would.
struct FixedVerdict(&'static str);
#[async_trait::async_trait]
impl ModelProvider for FixedVerdict {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        Ok(CompletionOutput::Final(self.0.to_owned()))
    }
}

fn conditional_tool_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {"name": name, "description": "creates a thing; apply=true persists it", "parameters": {}}
    })
}

/// Seeds `state.policy` with a `DestructiveWhen{apply,true}` rule for `name`,
/// the same way `main.rs` does at boot — reconcile, then classify against a
/// scripted verdict instead of a live model.
async fn pin_conditional_on_apply(state: &SharedState, name: &str) {
    let schema = conditional_tool_schema(name);
    let pending = state.policy.reconcile(&[schema.clone()]);
    let verdict = FixedVerdict(
        r#"{"class":"conditional","key":"apply","equals":true,"reason":"test-pinned"}"#,
    );
    state
        .policy
        .classify_pending(&verdict, &[schema], &pending)
        .await;
}

#[tokio::test]
async fn tool_call_matching_the_conditional_predicate_freezes_not_dispatches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_response(
            "securo.propose_create_transaction",
            serde_json::json!({"apply": true, "amount": 50}),
        )))
        .mount(&server)
        .await;

    let provider = Arc::new(harness::model::ollama::OllamaProvider::new(
        server.uri().parse().unwrap(),
        "qwen3:8b",
        Duration::from_secs(5),
        harness::model::ollama::ThinkMode::Off,
    ));
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!({"ok": true})));
    let mut cfg = test_config(server.uri(), Duration::from_secs(10));
    cfg.destructive_tools.clear(); // exercise the policy classifier, not the env override
    let state = build_state(cfg, provider, Arc::new(executor.clone()));
    pin_conditional_on_apply(&state, "securo.propose_create_transaction").await;

    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["needs_confirmation"], true);
    assert!(body["confirmation_token"].is_string());
    assert!(
        body["text"].as_str().unwrap().contains("apply=true"),
        "confirmation text renders the proposed args: {body}"
    );
    assert_eq!(
        executor.calls.lock().unwrap().len(),
        0,
        "a matching predicate must freeze, never dispatch"
    );
}

#[tokio::test]
async fn tool_call_not_matching_the_conditional_predicate_dispatches_directly() {
    let server = MockServer::start().await;
    // First completion returns a tool call without `apply` (a harmless
    // preview, per the predicate); the second — reached only if the
    // orchestrator loops back after dispatch — returns a final answer, so
    // the test fails loudly (via the assertion below) rather than looping
    // until `max_tool_calls`.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_response(
            "securo.propose_create_transaction",
            serde_json::json!({"amount": 50}),
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "here's a preview"},
            "done": true
        })))
        .mount(&server)
        .await;

    let provider = Arc::new(harness::model::ollama::OllamaProvider::new(
        server.uri().parse().unwrap(),
        "qwen3:8b",
        Duration::from_secs(5),
        harness::model::ollama::ThinkMode::Off,
    ));
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!({"preview": true})));
    let mut cfg = test_config(server.uri(), Duration::from_secs(10));
    cfg.destructive_tools.clear();
    let state = build_state(cfg, provider, Arc::new(executor.clone()));
    pin_conditional_on_apply(&state, "securo.propose_create_transaction").await;

    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "here's a preview");
    assert_eq!(
        body["needs_confirmation"],
        serde_json::Value::Null,
        "no apply=true present — dispatches without confirmation"
    );
    assert_eq!(
        executor.calls.lock().unwrap().len(),
        1,
        "dispatched exactly once"
    );
}

#[tokio::test]
async fn duplicate_client_msg_id_rejected_and_never_executed() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("first")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let router = api_router(state);
    let (s1, b1) = post_order(router.clone(), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["text"], "first");
    let (s2, b2) = post_order(router, ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(b2["error_code"], "duplicate_order");
}

#[tokio::test]
async fn malformed_json_maps_to_invalid_order() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let (status, body) = post_order(api_router(state), "{not json", Some("tok-cli")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], "invalid_order");
}

#[tokio::test]
async fn oversized_inline_attachment_rejected() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    // cap is 64 bytes in the test config; 100 'a's decode to ~75 bytes.
    let big = "Y".repeat(140);
    let body = format!(
        r#"{{"session_id":"s","text":"","attachments":[{{"mime":"text/plain","data_b64":"{big}"}}]}}"#
    );
    let (status, err) = post_order(api_router(state), &body, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error_code"], "invalid_order");
}

#[tokio::test]
async fn ambiguous_attachment_rejected() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let body =
        r#"{"session_id":"s","text":"","attachments":[{"mime":"m","data_b64":"aGk=","ref":"h"}]}"#;
    let (status, err) = post_order(api_router(state), body, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error_code"], "invalid_order");
}

// ---- redemption path (frozen-payload replay, SPEC §4.3) ----

struct RedemptionRig {
    state: SharedState,
    executor: RecordingExecutor,
}

fn redemption_rig(confirm_ttl: Duration, behavior: ExecBehavior) -> RedemptionRig {
    let cfg = test_config("http://127.0.0.1:1".into(), confirm_ttl);
    let executor = RecordingExecutor::new(behavior);
    let state = build_state(
        cfg,
        Arc::new(FixedProvider("unused — model bypassed at execution")),
        Arc::new(executor.clone()),
    );
    RedemptionRig { state, executor }
}

#[tokio::test]
async fn redemption_dispatches_frozen_payload_verbatim_then_burns_token() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Ok(serde_json::json!({"id": "tx_1"})),
    );
    let token = rig.state.pending.freeze(
        "cli",
        "s1",
        "securo.create_transaction".into(),
        serde_json::json!({"amount": 42}),
    );

    let body = serde_json::json!({"session_id": "s1", "text": "", "confirmation_token": token});
    let (status, resp) = post_order(
        api_router(rig.state.clone()),
        &body.to_string(),
        Some("tok-cli"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["text"], "Executed `securo.create_transaction`.");
    assert_eq!(resp["structured"][0]["args"]["amount"], 42);
    assert_eq!(resp["structured"][0]["result"]["id"], "tx_1");

    // Single-use: a replay answers confirmation_used, never re-executes.
    let (replay_status, replay) = post_order(
        api_router(rig.state.clone()),
        &body.to_string(),
        Some("tok-cli"),
    )
    .await;
    assert_eq!(replay_status, StatusCode::CONFLICT);
    assert_eq!(replay["error_code"], "confirmation_used");

    assert_eq!(
        rig.executor.calls.lock().unwrap().len(),
        1,
        "exactly one dispatch ever"
    );
}

#[tokio::test]
async fn failed_execution_consumes_token_without_retry() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Err(contract::ErrorCode::BackendUnavailable),
    );
    let token = rig
        .state
        .pending
        .freeze("cli", "s1", "t".into(), serde_json::json!({}));
    let body = serde_json::json!({"session_id": "s1", "text": "", "confirmation_token": token})
        .to_string();

    let (status, err) = post_order(api_router(rig.state.clone()), &body, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err["error_code"], "backend_unavailable");
    // Transient failure still burns the token — fresh confirmation required.
    let (replay_status, replay_err) =
        post_order(api_router(rig.state.clone()), &body, Some("tok-cli")).await;
    assert_eq!(replay_status, StatusCode::CONFLICT);
    assert_eq!(replay_err["error_code"], "confirmation_used");
}

#[tokio::test]
async fn unknown_and_expired_tokens_give_distinct_codes() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Ok(serde_json::json!(null)),
    );
    let (s, e) = post_order(
        api_router(rig.state),
        r#"{"session_id":"s1","text":"","confirmation_token":"ghost"}"#,
        Some("tok-cli"),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(e["error_code"], "confirmation_unknown");

    // TTL zero ⇒ deterministic expiry on first take.
    let expired = redemption_rig(Duration::ZERO, ExecBehavior::Ok(serde_json::json!(null)));
    let token = expired
        .state
        .pending
        .freeze("cli", "s1", "t".into(), serde_json::json!({}));
    let body = serde_json::json!({"session_id": "s1", "text": "", "confirmation_token": token})
        .to_string();
    let (s, e) = post_order(api_router(expired.state), &body, Some("tok-cli")).await;
    assert_eq!(s, StatusCode::GONE);
    assert_eq!(e["error_code"], "confirmation_expired");
}
