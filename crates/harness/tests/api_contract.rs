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
        num_ctx: 16_384,
        ollama_think: harness::model::ollama::ThinkMode::Off,
        ollama_seed: 42,
        ollama_top_k: 1,
        ollama_num_predict: 400,
        ollama_timeout: Duration::from_secs(5),
        mcp_tool_timeout: Duration::from_secs(5),
        order_budget: Duration::from_secs(30),
        max_tool_calls: 8,
        dedup_window: Duration::from_secs(60),
        session_ttl: Duration::from_secs(3600),
        confirm_ttl,
        confirm_mode: None,
        attach_max_bytes: 64,
        attach_dir: unique_temp_path("attachments"),
        destructive_tools: ["securo.create_transaction".to_owned()]
            .into_iter()
            .collect(),
        mcp_servers: Vec::new(),
        plugins_dir: unique_temp_path("plugins"),
        plugins_report_path: unique_temp_path("plugins-report.json"),
        router_enabled: false,
        tool_policy_path: unique_temp_path("policy.json"),
        audit_log_path: unique_temp_path("audit.jsonl"),
        transcript_log_path: unique_temp_path("transcript.jsonl"),
        audit_log_rotate_bytes: 0,
        transcript_log_rotate_bytes: 0,
        log_keep_segments: 12,
        log_verify_on_boot: false,
        tool_policy_retry: Duration::from_secs(300),
        number_guard: true,
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
    let audit = harness::audit::AuditLog::open(
        &cfg.audit_log_path,
        cfg.audit_log_rotate_bytes,
        cfg.log_keep_segments,
    )
    .expect("audit log opens in tests");
    let transcript = harness::transcript::TranscriptLog::open(
        &cfg.transcript_log_path,
        cfg.transcript_log_rotate_bytes,
        cfg.log_keep_segments,
    )
    .expect("transcript log opens in tests");
    let state: SharedState = Arc::new(AppState {
        sessions: harness::stores::sessions::SessionStore::new(),
        dedup: harness::stores::dedup::DedupStore::new(),
        pending: harness::stores::pending::PendingStore::new(),
        provider,
        plugins: Arc::new(harness::plugins::PluginRegistry::from_executor(executor)),
        cfg,
        order_tx,
        policy,
        audit: Some(audit),
        transcript: Some(transcript),
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

/// GET with an optional bearer token, returning (status, parsed JSON).
async fn get_json(
    router: axum::Router,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header(
            "authorization",
            token.map(|t| format!("Bearer {t}")).unwrap_or_default(),
        )
        .body(Body::empty())
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

// ---- tool-recognition reliability (SPEC §11 decision #16) ----

/// A provider double that yields scripted outputs in order, then repeats the
/// last one — drives multi-pass recovery-loop tests.
struct SequencedProvider(std::sync::Mutex<Vec<CompletionOutput>>);

#[async_trait::async_trait]
impl ModelProvider for SequencedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        let mut q = self.0.lock().unwrap();
        if q.len() > 1 {
            Ok(q.remove(0))
        } else {
            Ok(q[0].clone())
        }
    }
}

#[tokio::test]
async fn rescued_text_tool_call_is_dispatched_through_normal_path() {
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!({"id": "tx_9"})));
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(SequencedProvider(std::sync::Mutex::new(vec![
            // Pass 1: the model emits the call as text instead of the
            // structured field.
            CompletionOutput::Final(
                "Sure.\n```json\n{\"tool\": \"securo.list_accounts\", \"arguments\": {}}\n```"
                    .into(),
            ),
            // Pass 2 (after the real result lands): honest final.
            CompletionOutput::Final("Here are your accounts.".into()),
        ]))),
        Arc::new(executor.clone()),
    );
    pin_safe(&state, "securo.list_accounts").await;
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let calls = executor.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "the text call was rescued and dispatched");
    assert_eq!(calls[0].0, "securo.list_accounts");
    assert_eq!(body["text"], "Here are your accounts.", "{body}");
}

#[tokio::test]
async fn false_claim_final_triggers_recovery_loop() {
    // Pass 1: the model lies (no tool call). Pass 2 (after the corrective
    // nudge): the model makes the real call. Pass 3: honest final.
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(SequencedProvider(std::sync::Mutex::new(vec![
            CompletionOutput::Final(
                "Your expense for the Pixel 10 Pro has been successfully created.".into(),
            ),
            CompletionOutput::ToolCalls(vec![harness::model::ToolCall {
                id: "call_1".into(),
                name: "securo.list_accounts".into(),
                args: serde_json::json!({}),
            }]),
            CompletionOutput::Final("Done — the account list is in the result above.".into()),
        ]))),
        Arc::new(RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!(
            {"accounts": []}
        )))),
    );
    pin_safe(&state, "securo.list_accounts").await;
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The FIRST (lying) text must NOT be what the user receives.
    assert!(
        !body["text"]
            .as_str()
            .unwrap()
            .contains("successfully created"),
        "{body}"
    );
    assert_eq!(
        body["text"], "Done — the account list is in the result above.",
        "{body}"
    );
}

#[tokio::test]
async fn false_claim_delivered_when_budget_exhausts_recovery() {
    // The model lies on EVERY pass: after the budget guard window closes,
    // the text must be delivered rather than silently swallowed.
    let mut cfg = test_config("http://127.0.0.1:1".into(), Duration::from_millis(50));
    cfg.order_budget = Duration::from_millis(50); // tiny budget: first guard pass consumes it
    let state = build_state(
        cfg,
        Arc::new(FixedProvider(
            "The transaction has been applied to your account.",
        )),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["text"].as_str().unwrap().contains("has been applied"),
        "budget-exhausted lie is delivered, not swallowed: {body}"
    );
}

/// Scripted like `SequencedProvider`, and keeps every prompt it was sent.
struct PromptLogProvider {
    script: std::sync::Mutex<Vec<CompletionOutput>>,
    prompts: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

impl PromptLogProvider {
    fn new(script: Vec<CompletionOutput>) -> (Self, Arc<std::sync::Mutex<Vec<Vec<String>>>>) {
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                script: std::sync::Mutex::new(script),
                prompts: prompts.clone(),
            },
            prompts,
        )
    }
}

#[async_trait::async_trait]
impl ModelProvider for PromptLogProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        self.prompts
            .lock()
            .unwrap()
            .push(req.messages.iter().map(|m| m.content.clone()).collect());
        let mut q = self.script.lock().unwrap();
        Ok(if q.len() > 1 {
            q.remove(0)
        } else {
            q[0].clone()
        })
    }
}

fn propose_call(id: &str, args: serde_json::Value) -> CompletionOutput {
    CompletionOutput::ToolCalls(vec![harness::model::ToolCall {
        id: id.into(),
        name: "securo.propose_create_transaction".into(),
        args,
    }])
}

fn preview_nudges(prompts: &[Vec<String>]) -> usize {
    prompts
        .last()
        .map(|p| {
            p.iter()
                .filter(|m| m.contains("only prepared a preview"))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn success_claim_after_preview_nudges_toward_a_decision() {
    // Preview → false "created" → nudge → the acting call → decision.
    let (provider, prompts) = PromptLogProvider::new(vec![
        propose_call("call_1", serde_json::json!({"amount": 250})),
        CompletionOutput::Final("Expense created successfully.".into()),
        propose_call("call_2", serde_json::json!({"amount": 250, "apply": true})),
    ]);
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!(
        {"proposed": {"amount": 250}}
    )));
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(provider),
        Arc::new(executor.clone()),
    );
    pin_conditional_on_apply(&state, "securo.propose_create_transaction").await;
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["decision"].is_object(), "{body}");
    let calls = executor.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "only the preview ran");
    assert!(calls[0].1.get("apply").is_none());
    let prompts = prompts.lock().unwrap();
    assert_eq!(preview_nudges(&prompts), 1);
    assert!(
        prompts
            .last()
            .unwrap()
            .iter()
            .any(|m| m.contains("apply=true")),
        "nudge names the acting argument from the policy rule"
    );
}

#[tokio::test]
async fn repeated_success_claim_after_preview_becomes_a_decision() {
    // The model ignores the nudge: the harness asks the user with the
    // previewed args plus the acting argument; nothing more runs.
    let (provider, prompts) = PromptLogProvider::new(vec![
        propose_call("call_1", serde_json::json!({"amount": 250})),
        CompletionOutput::Final("Expense created successfully.".into()),
        propose_call("call_2", serde_json::json!({"amount": 250})),
        CompletionOutput::Final("Created expense: 250.".into()),
    ]);
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!(
        {"proposed": {"amount": 250}}
    )));
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(provider),
        Arc::new(executor.clone()),
    );
    pin_conditional_on_apply(&state, "securo.propose_create_transaction").await;
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body["text"].as_str().unwrap().contains("Created"),
        "{body}"
    );
    let details = body["decision"]["details"].as_array().expect("decision");
    assert!(
        details
            .iter()
            .any(|d| d["label"] == "apply" && d["value"] == "true"),
        "{body}"
    );
    assert!(
        details
            .iter()
            .any(|d| d["label"] == "amount" && d["value"] == "250")
    );
    assert_eq!(
        executor.calls.lock().unwrap().len(),
        2,
        "both previews only"
    );
    assert_eq!(
        preview_nudges(&prompts.lock().unwrap()),
        1,
        "nudged once only"
    );
}

#[tokio::test]
async fn honest_preview_gets_only_the_footer_and_reads_are_untouched() {
    for (tool, conditional, reply) in [
        (
            "securo.propose_create_transaction",
            true,
            "Here is the proposal. Should I apply it?",
        ),
        (
            "securo.list_transactions",
            false,
            "Your payment was successfully processed on Oct 5.",
        ),
    ] {
        let (provider, prompts) = PromptLogProvider::new(vec![
            CompletionOutput::ToolCalls(vec![harness::model::ToolCall {
                id: "call_1".into(),
                name: tool.into(),
                args: serde_json::json!({}),
            }]),
            CompletionOutput::Final(reply.into()),
        ]);
        let state = build_state(
            test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
            Arc::new(provider),
            Arc::new(RecordingExecutor::new(ExecBehavior::Ok(
                serde_json::json!({"ok": 1}),
            ))),
        );
        if conditional {
            pin_conditional_on_apply(&state, tool).await;
        } else {
            pin_safe(&state, tool).await;
        }
        let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let expected = if conditional {
            format!(
                "{reply}\n\nℹ Not saved yet: this is only a preview. Ask me to apply it to make the change."
            )
        } else {
            reply.to_owned()
        };
        assert_eq!(body["text"], expected, "{body}");
        assert_eq!(prompts.lock().unwrap().len(), 2, "no extra turn for {tool}");
    }
}

// ---- /v1/logs/verify (SPEC §11 decision #15) ----

#[tokio::test]
async fn logs_verify_requires_auth() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    let (status, _) = get_json(api_router(state), "/v1/logs/verify", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logs_verify_reports_clean_chain_with_entry_count() {
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        Arc::new(FixedProvider("x")),
        Arc::new(harness::orchestrator::NoBackends),
    );
    // Write two audit entries directly through the opened log.
    let audit = state.audit.as_ref().unwrap();
    audit
        .record(
            "cli",
            "s1",
            "t1",
            &serde_json::json!({}),
            harness::audit::RecordOutcome::Ok(&serde_json::json!(null)),
        )
        .unwrap();
    audit
        .record(
            "cli",
            "s1",
            "t2",
            &serde_json::json!({}),
            harness::audit::RecordOutcome::Err("backend_unavailable"),
        )
        .unwrap();

    let (status, body) = get_json(api_router(state), "/v1/logs/verify", Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["audit"]["ok"], true, "{body}");
    assert_eq!(body["audit"]["entries"], 2, "{body}");
    assert_eq!(body["audit"]["segments"], 1, "{body}");
    assert_eq!(body["transcript"]["ok"], true, "{body}");
}

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
        42,
        1,
        400,
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
    let pending = state.policy.reconcile(std::slice::from_ref(&schema));
    let verdict = FixedVerdict(
        r#"{"class":"conditional","key":"apply","equals":true,"reason":"test-pinned"}"#,
    );
    state
        .policy
        .classify_pending(&verdict, &[schema], &pending)
        .await;
}

/// Seeds `state.policy` with a `Safe` rule for `name` (same mechanism) so a
/// rescue/claim-guard test can dispatch without confirmation.
async fn pin_safe(state: &SharedState, name: &str) {
    let schema = conditional_tool_schema(name);
    let pending = state.policy.reconcile(std::slice::from_ref(&schema));
    let verdict = FixedVerdict(r#"{"class":"safe","reason":"test-pinned"}"#);
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
        42,
        1,
        400,
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
    let decision = &body["decision"];
    assert_eq!(decision["kind"], "confirm");
    assert_eq!(
        decision["id"], body["confirmation_token"],
        "legacy token == decision id"
    );
    assert!(decision["id"].as_str().unwrap().starts_with("dcs_"));
    assert_eq!(decision["options"][0]["value"], "approve");
    assert_eq!(decision["options"][1]["value"], "reject");
    assert_eq!(decision["free_text"], true);
    let details = decision["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d["label"] == "amount" && d["value"] == "50"),
        "details derive from frozen args: {details:?}"
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
        42,
        1,
        400,
    ));
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!({"preview": true})));
    let mut cfg = test_config(server.uri(), Duration::from_secs(10));
    cfg.destructive_tools.clear();
    let state = build_state(cfg, provider, Arc::new(executor.clone()));
    pin_conditional_on_apply(&state, "securo.propose_create_transaction").await;

    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["text"],
        "here's a preview\n\nℹ Not saved yet: this is only a preview. Ask me to apply it to make the change.",
        "a preview always says so (SPEC §11 #16)"
    );
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
async fn second_completion_sees_the_models_own_prior_tool_call() {
    // Regression test for a real conversation-quality bug: the assistant's
    // own tool-call request must be replayed back to it on the next
    // completion, not just the orphaned result (orchestrator/mod.rs,
    // model/mod.rs's `ChatMessage::assistant_tool_calls`).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_response(
            "securo.list_transactions",
            serde_json::json!({}),
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "done"},
            "done": true
        })))
        .mount(&server)
        .await;

    let provider = Arc::new(harness::model::ollama::OllamaProvider::new(
        server.uri().parse().unwrap(),
        "qwen3:8b",
        Duration::from_secs(5),
        harness::model::ollama::ThinkMode::Off,
        42,
        1,
        400,
    ));
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!({"total": 8})));
    let mut cfg = test_config(server.uri(), Duration::from_secs(10));
    cfg.destructive_tools.clear();
    let state = build_state(cfg, provider, Arc::new(executor));
    pin_conditional_on_apply(&state, "securo.list_transactions").await;

    let (status, _) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);

    let received = server.received_requests().await.expect("recording enabled");
    assert_eq!(received.len(), 2, "one call round, one final round");
    let second_body: serde_json::Value = received[1].body_json().expect("valid json body");
    let messages = second_body["messages"].as_array().expect("messages array");
    let assistant_tool_call_msg = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .expect("the model's own prior tool call must be present in the next request");
    assert_eq!(
        assistant_tool_call_msg["tool_calls"][0]["function"]["name"], "securo.list_transactions",
        "not just present, but naming the tool it actually called"
    );
    // And the matching result is still there too, right after it.
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("total")),
        "the result travels alongside the request, not orphaned: {messages:#?}"
    );
}

#[tokio::test]
async fn tool_error_triggers_a_situational_retry_nudge_on_the_next_completion() {
    // Ollama's `tool_choice` is a documented no-op on the runtime this
    // project targets (verified live) — the harness cannot force a retry
    // tool call. Instead, a failed dispatch should inject a fresh,
    // situational instruction (maximally recent for the next completion)
    // rather than relying solely on the static system prompt.
    let server = MockServer::start().await;
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
            "message": {"role": "assistant", "content": "retrying"},
            "done": true
        })))
        .mount(&server)
        .await;

    let provider = Arc::new(harness::model::ollama::OllamaProvider::new(
        server.uri().parse().unwrap(),
        "qwen3:8b",
        Duration::from_secs(5),
        harness::model::ollama::ThinkMode::Off,
        42,
        1,
        400,
    ));
    // The tool "dispatches" (not destructive — no `apply`) but the backend
    // itself reports an application-level failure, same shape as Securo's
    // own `{"error": "..."}` validation responses observed live.
    let executor = RecordingExecutor::new(ExecBehavior::Ok(
        serde_json::json!({"error": "group_id and splits must be provided together"}),
    ));
    let mut cfg = test_config(server.uri(), Duration::from_secs(10));
    cfg.destructive_tools.clear();
    let state = build_state(cfg, provider, Arc::new(executor));
    pin_conditional_on_apply(&state, "securo.propose_create_transaction").await;

    let (status, _) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);

    let received = server.received_requests().await.expect("recording enabled");
    let second_body: serde_json::Value = received[1].body_json().expect("valid json body");
    let messages = second_body["messages"].as_array().expect("messages array");
    let last = messages.last().expect("at least one message");
    assert_eq!(
        last["role"], "user",
        "nudge lands as the most recent message, right after the erroring result"
    );
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .contains("did not succeed"),
        "nudge content present: {last}"
    );
}

#[tokio::test]
async fn transcript_records_user_tool_call_result_and_final_answer_in_order() {
    let server = MockServer::start().await;
    // First completion: a dispatchable (no `apply`) tool call. Second: the
    // final answer, reached after the orchestrator loops back with the tool
    // result — same two-mock shape as the sibling dispatch test above.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_response(
            "securo.list_transactions",
            serde_json::json!({}),
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "you have 8 transactions"},
            "done": true
        })))
        .mount(&server)
        .await;

    let provider = Arc::new(harness::model::ollama::OllamaProvider::new(
        server.uri().parse().unwrap(),
        "qwen3:8b",
        Duration::from_secs(5),
        harness::model::ollama::ThinkMode::Off,
        42,
        1,
        400,
    ));
    let executor = RecordingExecutor::new(ExecBehavior::Ok(serde_json::json!({"total": 8})));
    let mut cfg = test_config(server.uri(), Duration::from_secs(10));
    cfg.destructive_tools.clear();
    let transcript_path = cfg.transcript_log_path.clone();
    let state = build_state(cfg, provider, Arc::new(executor));
    pin_safe(&state, "securo.list_transactions").await;

    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "you have 8 transactions");

    let contents = std::fs::read_to_string(&transcript_path).unwrap();
    let lines: Vec<serde_json::Value> = contents
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kinds: Vec<&str> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec![
            "user_message",
            "tool_call",
            "tool_result",
            "assistant_final"
        ],
        "the full conversational event stream, in order: {lines:#?}"
    );
    assert_eq!(lines[1]["tool"], "securo.list_transactions");
    assert_eq!(lines[2]["result"]["total"], 8);
    assert_eq!(lines[3]["text"], "you have 8 transactions");
    // Hash chain integrity, exercised end-to-end through the real dispatch path.
    for w in lines.windows(2) {
        assert_eq!(w[1]["prev_hash"], w[0]["hash"]);
    }
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

// ---- attachment ingestion (SPEC §4.1, attachment→tool wiring) ----

/// Reads the manifest's `[handle=..., type=...]` entry back out of the
/// system message the orchestrator injects — the test double stands in for
/// a real model reading the same text.
fn extract_handle(manifest: &str) -> &str {
    let after = manifest.split("handle=").nth(1).expect("handle= present");
    after.split([',', ']']).next().expect("terminator present")
}

/// First completion: reads the attachment handle out of the manifest system
/// message and calls `ocr.extract_text` with it. Second completion (after
/// the tool result lands): an honest final answer.
struct AttachmentAwareProvider {
    called: std::sync::Mutex<bool>,
}

#[async_trait::async_trait]
impl ModelProvider for AttachmentAwareProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        let mut called = self.called.lock().unwrap();
        if !*called {
            *called = true;
            let manifest = req
                .messages
                .iter()
                .find(|m| m.content.contains("Attached files available to tools"))
                .expect("manifest system message present");
            let handle = extract_handle(&manifest.content);
            Ok(CompletionOutput::ToolCalls(vec![
                harness::model::ToolCall {
                    id: "call_1".into(),
                    name: "ocr.extract_text".into(),
                    args: serde_json::json!({"image_ref": handle}),
                },
            ]))
        } else {
            Ok(CompletionOutput::Final("The bill total is $42.".into()))
        }
    }
}

#[tokio::test]
async fn image_attachment_produces_manifest_ocr_call_and_cleans_up_scratch_file() {
    let mut cfg = test_config("http://127.0.0.1:1".into(), Duration::from_secs(10));
    cfg.attach_max_bytes = 1024;
    let attach_dir = cfg.attach_dir.clone();
    let executor = RecordingExecutor::new(ExecBehavior::Ok(
        serde_json::json!({"text": "Total: $42", "chars": 10}),
    ));
    let state = build_state(
        cfg,
        Arc::new(AttachmentAwareProvider {
            called: std::sync::Mutex::new(false),
        }),
        Arc::new(executor.clone()),
    );
    pin_safe(&state, "ocr.extract_text").await;

    let body = serde_json::json!({
        "session_id": "s1",
        "text": "what do I owe on this bill?",
        "attachments": [{"mime": "image/png", "data_b64": "aGVsbG8="}],
    });
    let (status, resp) = post_order(
        api_router(state.clone()),
        &body.to_string(),
        Some("tok-cli"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["text"], "The bill total is $42.");

    let calls = executor.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ocr.extract_text");
    let handle = calls[0].1["image_ref"].as_str().unwrap().to_owned();
    assert!(handle.starts_with("att_"), "{handle}");
    drop(calls);

    // Cleaned up after the order — the scratch file must not survive it.
    assert!(
        !attach_dir.join(&handle).exists(),
        "attachment scratch file was not cleaned up"
    );

    // Never leaks into the persistent session store (SPEC §4.1): only the
    // in-loop context saw the manifest, not the durable history.
    let history = state
        .sessions
        .snapshot(&("cli".to_owned(), "s1".to_owned()));
    assert!(
        history
            .iter()
            .all(|m| !m.content.contains("Attached files available to tools")),
        "manifest must not be persisted into session history"
    );
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
        confirm_options(),
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
    let token = rig.state.pending.freeze(
        "cli",
        "s1",
        "t".into(),
        serde_json::json!({}),
        confirm_options(),
    );
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
    let token = expired.state.pending.freeze(
        "cli",
        "s1",
        "t".into(),
        serde_json::json!({}),
        confirm_options(),
    );
    let body = serde_json::json!({"session_id": "s1", "text": "", "confirmation_token": token})
        .to_string();
    let (s, e) = post_order(api_router(expired.state), &body, Some("tok-cli")).await;
    assert_eq!(s, StatusCode::GONE);
    assert_eq!(e["error_code"], "confirmation_expired");
}

// ---- decisions (SPEC §4.3, §11 #18) ----

fn confirm_options() -> Vec<String> {
    vec!["approve".into(), "reject".into()]
}

fn freeze_tx(state: &SharedState) -> String {
    state.pending.freeze(
        "cli",
        "s1",
        "securo.create_transaction".into(),
        serde_json::json!({"amount": 42}),
        confirm_options(),
    )
}

async fn answer(
    state: &SharedState,
    msg_id: &str,
    answer: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let body = serde_json::json!({
        "session_id": "s1",
        "client_msg_id": msg_id,
        "decision_answer": answer,
    });
    post_order(
        api_router(state.clone()),
        &body.to_string(),
        Some("tok-cli"),
    )
    .await
}

#[tokio::test]
async fn approve_dispatches_frozen_payload_once_then_reports_used() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Ok(serde_json::json!({"id": "tx_1"})),
    );
    let id = freeze_tx(&rig.state);

    let (status, resp) = answer(
        &rig.state,
        "m1",
        serde_json::json!({"id": id, "value": "approve"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["text"], "Executed `securo.create_transaction`.");
    assert_eq!(resp["structured"][0]["args"]["amount"], 42);

    let (status, err) = answer(
        &rig.state,
        "m2",
        serde_json::json!({"id": id, "value": "approve"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error_code"], "decision_used");
    assert_eq!(
        rig.executor.calls.lock().unwrap().len(),
        1,
        "exactly one dispatch ever"
    );
}

#[tokio::test]
async fn reject_never_dispatches() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Ok(serde_json::json!(null)),
    );
    let id = freeze_tx(&rig.state);
    let (status, resp) = answer(
        &rig.state,
        "m1",
        serde_json::json!({"id": id, "value": "reject"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["text"], "Cancelled. Nothing was executed.");
    assert!(rig.executor.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_expired_and_invalid_answers_give_distinct_codes() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Ok(serde_json::json!(null)),
    );
    let (s, e) = answer(
        &rig.state,
        "m1",
        serde_json::json!({"id": "dcs_ghost", "value": "approve"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(e["error_code"], "decision_unknown");

    let id = freeze_tx(&rig.state);
    let (s, e) = answer(
        &rig.state,
        "m2",
        serde_json::json!({"id": id, "value": "yolo"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e["error_code"], "decision_invalid_value");
    // Single-use even on failure: the id cannot be approved afterwards.
    let (s, e) = answer(
        &rig.state,
        "m3",
        serde_json::json!({"id": id, "value": "approve"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(e["error_code"], "decision_used");

    let (s, e) = answer(
        &rig.state,
        "m4",
        serde_json::json!({"id": "x", "value": "approve", "text": "both"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e["error_code"], "invalid_order");

    let expired = redemption_rig(Duration::ZERO, ExecBehavior::Ok(serde_json::json!(null)));
    let id = freeze_tx(&expired.state);
    let (s, e) = answer(
        &expired.state,
        "m1",
        serde_json::json!({"id": id, "value": "approve"}),
    )
    .await;
    assert_eq!(s, StatusCode::GONE);
    assert_eq!(e["error_code"], "decision_expired");
    assert!(rig.executor.calls.lock().unwrap().is_empty());
    assert!(expired.executor.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn free_text_answer_runs_the_model_and_never_dispatches() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Ok(serde_json::json!(null)),
    );
    let id = freeze_tx(&rig.state);
    let (status, resp) = answer(
        &rig.state,
        "m1",
        serde_json::json!({"id": id, "text": "make it 50"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        resp["text"], "unused — model bypassed at execution",
        "normal path ran"
    );
    assert!(
        rig.executor.calls.lock().unwrap().is_empty(),
        "free text never approves"
    );

    let history = rig
        .state
        .sessions
        .snapshot(&("cli".to_owned(), "s1".to_owned()));
    assert!(history.iter().any(|m| m.content.contains("NOT executed")));
    assert!(history.iter().any(|m| m.content == "make it 50"));
}

#[tokio::test]
async fn plain_yes_never_executes_and_supersedes_the_pending_decision() {
    let rig = redemption_rig(
        Duration::from_secs(600),
        ExecBehavior::Ok(serde_json::json!(null)),
    );
    let id = freeze_tx(&rig.state);
    let body = serde_json::json!({"session_id": "s1", "text": "yes", "client_msg_id": "m1"});
    let (status, _) = post_order(
        api_router(rig.state.clone()),
        &body.to_string(),
        Some("tok-cli"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        rig.executor.calls.lock().unwrap().is_empty(),
        "plain text must never approve"
    );

    // Superseded: neither the decision nor its legacy token can approve now.
    let (s, e) = answer(
        &rig.state,
        "m2",
        serde_json::json!({"id": id, "value": "approve"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(e["error_code"], "decision_used");
    assert!(rig.executor.calls.lock().unwrap().is_empty());
}

// SPEC §11 #20: a backend's `checks` are relayed verbatim, and a number the
// final answer invents is named — both without another model pass.
#[tokio::test]
async fn final_answer_relays_tool_checks_and_names_invented_numbers() {
    let result = serde_json::Value::String(
        serde_json::json!({
            "text": "Item Q 70.46\nFee Q 10\nTotal a Pagar Q 30.46",
            "checks": ["\"30.46\" may be a misread of 80.46 (= 70.46 + 10)"],
        })
        .to_string(),
    );
    let executor = RecordingExecutor::new(ExecBehavior::Ok(result));
    let provider = Arc::new(CapturingProvider {
        script: SequencedProvider(std::sync::Mutex::new(vec![
            CompletionOutput::ToolCalls(vec![harness::model::ToolCall {
                id: "c1".into(),
                name: "ocr.extract_text".into(),
                args: serde_json::json!({"image_ref": "att_x"}),
            }]),
            CompletionOutput::Final("Total Q30.46, or Q38.01 with tax.".into()),
        ])),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let state = build_state(
        test_config("http://127.0.0.1:1".into(), Duration::from_secs(10)),
        provider.clone(),
        Arc::new(executor.clone()),
    );
    pin_safe(&state, "ocr.extract_text").await;
    let (status, body) = post_order(api_router(state), ORDER_BODY, Some("tok-cli")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["text"],
        "Total Q30.46, or Q38.01 with tax.\n\n⚠ Checks:\n- \"30.46\" may be a misread of 80.46 (= 70.46 + 10)\n\n⚠ Not found in the source data: 38.01",
        "{body}"
    );
    // The model reads the tool result without `checks` (SPEC §4.2).
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].contains("Total a Pagar"),
        "tool result reached the model"
    );
    assert!(!seen[1].contains("checks"), "{}", seen[1]);
}

/// Records every message the model is given, then answers from a script.
struct CapturingProvider {
    script: SequencedProvider,
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ModelProvider for CapturingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        let all: Vec<String> = req.messages.iter().map(|m| m.content.clone()).collect();
        self.seen.lock().unwrap().push(all.join("\n"));
        self.script.complete(req).await
    }
}
