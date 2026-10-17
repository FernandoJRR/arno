//! Plugin loading, secrets, trust, routing and reload (SPEC §4.5, §11 #21–#24)
//! against the in-process mock MCP backend and a scripted model.

mod common;

use contract::Order;
use harness::config::{BackendAddr, Config};
use harness::model::{CompletionOutput, CompletionRequest, ModelError, ModelProvider, ToolCall};
use harness::orchestrator::ToolExecutor;
use harness::plugins::{LoadError, LoadSpec, PluginRegistry, PluginStatus};
use harness::policy::TrustLevel;
use harness::state::{AppState, SharedState};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn temp_dir(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "arno-plugins-test-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_manifest(dir: &Path, id: &str, url: &str, provenance: &str, extra: &str) {
    std::fs::write(
        dir.join(format!("{id}.toml")),
        format!(
            "arno_manifest = 1\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1\"\n\
             provenance = \"{provenance}\"\n[server]\nkind = \"remote\"\nurl = \"{url}\"\n{extra}"
        ),
    )
    .unwrap();
}

fn spec(dir: &Path) -> LoadSpec {
    LoadSpec {
        dir: dir.to_owned(),
        legacy: Vec::new(),
        timeout: Duration::from_secs(5),
    }
}

fn names(tools: &[serde_json::Value]) -> Vec<String> {
    let mut v: Vec<String> = tools
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn allowlist_hides_and_blocks_disabled_tools() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("allow");
    write_manifest(
        &dir,
        "mock",
        &url,
        "first-party",
        "[tools]\nenabled = [\"echo\", \"b*\"]\n",
    );
    let set = harness::plugins::load(&spec(&dir)).await.unwrap();
    assert_eq!(names(set.tool_schemas()), vec!["mock.boom", "mock.echo"]);
    // A hallucinated call to a disabled tool never reaches the backend.
    let r = set
        .execute(
            "mock.lurch",
            &serde_json::json!({"first_ms": 0, "later_ms": 0}),
        )
        .await
        .unwrap();
    assert_eq!(r["unknown_tool"], "mock.lurch");
}

#[tokio::test]
async fn each_plugin_gets_only_its_own_secret() {
    let (url_a, seen_a) = common::mock_mcp::spawn_capturing_header("authorization")
        .await
        .unwrap();
    let (url_b, seen_b) = common::mock_mcp::spawn_capturing_header("authorization")
        .await
        .unwrap();
    let dir = temp_dir("secrets");
    write_manifest(
        &dir,
        "alpha",
        &url_a,
        "third-party",
        "auth = { kind = \"bearer\", secret = \"ALPHA_TOKEN\" }\n\
         [permissions]\nsecrets = [\"ALPHA_TOKEN\"]\n",
    );
    std::fs::write(dir.join("alpha.env"), "ALPHA_TOKEN=s3cr3t-alpha\n").unwrap();
    write_manifest(&dir, "beta", &url_b, "third-party", "");
    let set = harness::plugins::load(&spec(&dir)).await.unwrap();
    assert!(
        set.plugins()
            .iter()
            .all(|p| p.status == PluginStatus::Ready)
    );
    assert_eq!(
        seen_a.lock().unwrap().as_deref(),
        Some("Bearer s3cr3t-alpha")
    );
    assert_eq!(seen_b.lock().unwrap().as_deref(), None);
    // The value is not in the process env.
    assert!(std::env::var("ALPHA_TOKEN").is_err());
}

#[tokio::test]
async fn missing_secret_fails_that_plugin_only() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("missing");
    write_manifest(
        &dir,
        "locked",
        &url,
        "third-party",
        "auth = { kind = \"bearer\", secret = \"LOCKED_TOKEN\" }\n\
         [permissions]\nsecrets = [\"LOCKED_TOKEN\"]\n",
    );
    write_manifest(&dir, "open", &url, "third-party", "");
    let set = harness::plugins::load(&spec(&dir)).await.unwrap();
    let locked = set.plugin("locked").unwrap();
    assert!(
        matches!(&locked.status, PluginStatus::Failed(r) if r.contains("LOCKED_TOKEN")),
        "{:?}",
        locked.status
    );
    assert_eq!(set.plugin("open").unwrap().status, PluginStatus::Ready);
    assert!(
        names(set.tool_schemas())
            .iter()
            .all(|n| n.starts_with("open."))
    );
}

#[tokio::test]
async fn unreachable_manifest_plugin_is_skipped_not_fatal() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("unreachable");
    write_manifest(&dir, "gone", "http://127.0.0.1:1/mcp", "third-party", "");
    write_manifest(&dir, "here", &url, "first-party", "");
    let set = harness::plugins::load(&spec(&dir)).await.unwrap();
    assert!(matches!(
        set.plugin("gone").unwrap().status,
        PluginStatus::Failed(_)
    ));
    assert!(!set.tool_schemas().is_empty());
}

#[tokio::test]
async fn legacy_servers_still_load_and_conflicts_fail_loudly() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("legacy");
    let legacy = vec![(
        "mock".to_owned(),
        BackendAddr::Http {
            url: url.parse().unwrap(),
            headers: Vec::new(),
        },
    )];
    let set = harness::plugins::load(&LoadSpec {
        legacy: legacy.clone(),
        ..spec(&dir)
    })
    .await
    .unwrap();
    assert!(set.plugin("mock").unwrap().is_legacy());
    assert!(names(set.tool_schemas()).contains(&"mock.echo".to_owned()));

    write_manifest(&dir, "mock", &url, "first-party", "");
    let err = harness::plugins::load(&LoadSpec {
        legacy,
        ..spec(&dir)
    })
    .await
    .err()
    .unwrap();
    assert!(matches!(err, LoadError::LegacyConflict(id) if id == "mock"));
}

#[tokio::test]
async fn bad_manifest_aborts_the_load() {
    let dir = temp_dir("bad");
    write_manifest(
        &dir,
        "x",
        "http://127.0.0.1:1/mcp",
        "first-party",
        "hooks = []\n",
    );
    assert!(matches!(
        harness::plugins::load(&spec(&dir)).await,
        Err(LoadError::Manifest(_))
    ));
}

// ---- orders through plugins ----

const SAFE: &str = r#"{"class":"safe","key":null,"equals":null,"reason":"reads"}"#;

/// Answers router requests (those with `format`) with `route`, classifier
/// prompts with `SAFE`, and main-loop requests from `script`, recording the
/// tools each main-loop request offered.
struct Model {
    route: &'static str,
    script: Mutex<Vec<CompletionOutput>>,
    offered: Mutex<Vec<Vec<String>>>,
    /// Last user message of each main-loop request.
    last: Mutex<Vec<String>>,
    routed_calls: Mutex<usize>,
}

impl Model {
    fn new(route: &'static str, script: Vec<CompletionOutput>) -> Arc<Self> {
        Arc::new(Self {
            route,
            script: Mutex::new(script),
            offered: Mutex::new(Vec::new()),
            last: Mutex::new(Vec::new()),
            routed_calls: Mutex::new(0),
        })
    }
}

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        if req.format.is_some() {
            *self.routed_calls.lock().unwrap() += 1;
            return Ok(CompletionOutput::Final(self.route.to_owned()));
        }
        if req.tools.is_empty() && req.messages[0].content.contains("safety classifier") {
            return Ok(CompletionOutput::Final(SAFE.to_owned()));
        }
        self.offered.lock().unwrap().push(names(&req.tools));
        self.last.lock().unwrap().push(
            req.messages
                .iter()
                .rev()
                .find(|m| m.role == harness::model::Role::User)
                .map(|m| m.content.clone())
                .unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut q = self.script.lock().unwrap();
        Ok(if q.len() > 1 {
            q.remove(0)
        } else {
            q[0].clone()
        })
    }
}

fn config(dir: &Path, router_enabled: bool) -> Config {
    let mut client_tokens = std::collections::HashMap::new();
    client_tokens.insert("cli".to_owned(), "tok-cli".to_owned());
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        client_tokens,
        ollama_url: "http://127.0.0.1:1".parse().unwrap(),
        model: "m".into(),
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
        confirm_ttl: Duration::from_secs(600),
        confirm_mode: None,
        attach_max_bytes: 1024,
        attach_dir: dir.join("attachments"),
        destructive_tools: Default::default(),
        mcp_servers: Vec::new(),
        plugins_dir: dir.join("plugins"),
        plugins_report_path: dir.join("report.json"),
        router_enabled,
        tool_policy_path: dir.join("policy.json"),
        audit_log_path: dir.join("audit.jsonl"),
        transcript_log_path: dir.join("transcript.jsonl"),
        audit_log_rotate_bytes: 0,
        transcript_log_rotate_bytes: 0,
        log_keep_segments: 12,
        log_verify_on_boot: false,
        tool_policy_retry: Duration::from_secs(300),
        number_guard: false,
    }
}

/// Production topology: load → activate → registry → FIFO worker.
async fn boot(cfg: Config, model: Arc<Model>) -> SharedState {
    let set = harness::plugins::load(&LoadSpec::from_config(&cfg))
        .await
        .unwrap();
    let policy = Arc::new(harness::policy::ToolPolicy::load(
        cfg.tool_policy_path.clone(),
        cfg.destructive_tools.clone(),
    ));
    harness::plugins::activate(&set, &policy, model.as_ref(), &cfg.plugins_report_path).await;
    let transcript = harness::transcript::TranscriptLog::open(&cfg.transcript_log_path, 0, 12)
        .expect("transcript opens");
    let (order_tx, order_rx) = tokio::sync::mpsc::channel(16);
    let state: SharedState = Arc::new(AppState {
        sessions: harness::stores::sessions::SessionStore::new(),
        dedup: harness::stores::dedup::DedupStore::new(),
        pending: harness::stores::pending::PendingStore::new(),
        provider: model,
        plugins: Arc::new(PluginRegistry::new(set)),
        cfg,
        order_tx,
        policy,
        audit: None,
        transcript: Some(transcript),
    });
    tokio::spawn(harness::queue::spawn_worker(state.clone(), order_rx));
    state
}

async fn send(state: &SharedState, text: &str) -> contract::Response {
    send_order(state, serde_json::json!({"session_id": "s1", "text": text})).await
}

async fn send_with_image(state: &SharedState, text: &str) -> contract::Response {
    send_order(
        state,
        serde_json::json!({"session_id": "s1", "text": text,
            "attachments": [{"mime": "image/png", "data_b64": "aGk="}]}),
    )
    .await
}

async fn send_order(state: &SharedState, order: serde_json::Value) -> contract::Response {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let order: Order = serde_json::from_value(order).unwrap();
    state
        .order_tx
        .send(harness::queue::Job::Order(Box::new(
            harness::queue::OrderJob {
                client_id: "cli".into(),
                order,
                reply: tx,
            },
        )))
        .await
        .unwrap();
    match rx.await.unwrap() {
        harness::queue::Outcome::Ok(r) => *r,
        harness::queue::Outcome::Err(e) => panic!("order failed: {:?}", e.0),
    }
}

fn echo_call() -> CompletionOutput {
    CompletionOutput::ToolCalls(vec![ToolCall {
        id: "c1".into(),
        name: "notes.echo".into(),
        args: serde_json::json!({"text": "hi"}),
    }])
}

#[tokio::test]
async fn untrusted_plugin_read_needs_a_decision_and_promotion_lifts_it() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("trust");
    let cfg = config(&dir, false);
    std::fs::create_dir_all(&cfg.plugins_dir).unwrap();
    write_manifest(&cfg.plugins_dir, "notes", &url, "third-party", "");
    let model = Model::new(
        "",
        vec![echo_call(), CompletionOutput::Final("done".into())],
    );
    let state = boot(cfg.clone(), model.clone()).await;
    assert_eq!(
        state.policy.trust("notes").unwrap().level,
        TrustLevel::Untrusted
    );
    // Classified safe, but untrusted: a decision, no dispatch.
    let (safe, ..) = state.policy.counts_for(&["notes.echo"]);
    assert_eq!(safe, 1);
    let r = send(&state, "echo hi").await;
    let decision = r.decision.expect("untrusted read asks first");
    assert!(decision.prompt.contains("notes.echo"));

    // Disclosure was written with the declared shape.
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg.plugins_report_path).unwrap()).unwrap();
    assert_eq!(report[0]["id"], "notes");
    assert_eq!(report[0]["trust"], "untrusted");

    // Operator promotes, then reloads: the same read now runs directly.
    let mut file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg.tool_policy_path).unwrap()).unwrap();
    file["plugins"]["notes"]["level"] = "trusted".into();
    file["plugins"]["notes"]["source"] = "operator".into();
    std::fs::write(&cfg.tool_policy_path, file.to_string()).unwrap();
    reload(&state).await;
    *model.script.lock().unwrap() = vec![echo_call(), CompletionOutput::Final("done".into())];
    let r = send(&state, "echo hi again").await;
    assert!(r.decision.is_none());
    assert_eq!(r.text, "done");

    // Editing the manifest demotes it again.
    write_manifest(
        &cfg.plugins_dir,
        "notes",
        &url,
        "third-party",
        "[permissions]\negress = [\"example:1\"]\n",
    );
    reload(&state).await;
    assert_eq!(
        state.policy.trust("notes").unwrap().level,
        TrustLevel::Untrusted
    );
}

async fn reload(state: &SharedState) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .order_tx
        .send(harness::queue::Job::Reload(Some(tx)))
        .await
        .unwrap();
    rx.await.unwrap().expect("reload ok");
}

#[tokio::test]
async fn reload_lands_between_orders_and_in_flight_order_keeps_its_snapshot() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("reload");
    let cfg = config(&dir, false);
    std::fs::create_dir_all(&cfg.plugins_dir).unwrap();
    write_manifest(
        &cfg.plugins_dir,
        "one",
        &url,
        "first-party",
        "[tools]\nenabled = [\"echo\"]\n",
    );
    let model = Model::new("", vec![CompletionOutput::Final("ok".into())]);
    let state = boot(cfg.clone(), model.clone()).await;

    write_manifest(
        &cfg.plugins_dir,
        "two",
        &url,
        "first-party",
        "[tools]\nenabled = [\"echo\"]\n",
    );
    // Order first, reload right behind it on the same queue.
    let order = {
        let state = state.clone();
        tokio::spawn(async move { send(&state, "hi").await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    reload(&state).await;
    order.await.unwrap();

    assert_eq!(model.offered.lock().unwrap()[0], vec!["one.echo"]);
    assert_eq!(
        names(state.plugins.snapshot().tool_schemas()),
        vec!["one.echo", "two.echo"]
    );
}

#[tokio::test]
async fn router_off_is_unrouted_and_on_restricts_to_the_pick() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let manifest_extra = "[tools]\nenabled = [\"echo\", \"boom\"]\n\
        [[groups]]\nid = \"talk\"\nwhen = \"Echo text back\"\nhow = \"Echo exactly.\"\ntools = [\"echo\"]\n";

    // Off: no router call, full catalogue.
    let dir = temp_dir("router-off");
    let cfg = config(&dir, false);
    std::fs::create_dir_all(&cfg.plugins_dir).unwrap();
    write_manifest(
        &cfg.plugins_dir,
        "mock",
        &url,
        "first-party",
        manifest_extra,
    );
    let model = Model::new(
        r#"{"group":"mock.talk"}"#,
        vec![CompletionOutput::Final("ok".into())],
    );
    let state = boot(cfg, model.clone()).await;
    send(&state, "say hi").await;
    assert_eq!(*model.routed_calls.lock().unwrap(), 0);
    assert_eq!(
        model.offered.lock().unwrap()[0],
        vec!["mock.boom", "mock.echo"]
    );

    // On: one router call, only the picked group's tools, pick recorded.
    let dir = temp_dir("router-on");
    let cfg = config(&dir, true);
    std::fs::create_dir_all(&cfg.plugins_dir).unwrap();
    write_manifest(
        &cfg.plugins_dir,
        "mock",
        &url,
        "first-party",
        manifest_extra,
    );
    let model = Model::new(
        r#"{"group":"mock.talk"}"#,
        vec![CompletionOutput::Final("ok".into())],
    );
    let state = boot(cfg.clone(), model.clone()).await;
    send(&state, "say hi").await;
    assert_eq!(*model.routed_calls.lock().unwrap(), 1);
    assert_eq!(model.offered.lock().unwrap()[0], vec!["mock.echo"]);
    let transcript = std::fs::read_to_string(&cfg.transcript_log_path).unwrap();
    let pick = transcript
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|e| e["kind"] == "router_pick")
        .expect("router_pick recorded");
    assert_eq!(pick["group"], "mock.talk");

    // `none`: no tools at all.
    let model = Model::new(
        r#"{"group":"none"}"#,
        vec![CompletionOutput::Final("hey".into())],
    );
    let dir = temp_dir("router-none");
    let cfg = config(&dir, true);
    std::fs::create_dir_all(&cfg.plugins_dir).unwrap();
    write_manifest(
        &cfg.plugins_dir,
        "mock",
        &url,
        "first-party",
        manifest_extra,
    );
    let state = boot(cfg, model.clone()).await;
    send(&state, "thanks!").await;
    assert!(model.offered.lock().unwrap()[0].is_empty());
}

#[test]
fn shipped_manifests_parse() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/plugins");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|x| x == "toml") {
            let text = std::fs::read_to_string(&path).unwrap();
            let m = harness::plugins::manifest::Manifest::parse(&text, &path.display().to_string())
                .unwrap();
            assert_eq!(path.file_stem().unwrap().to_str(), Some(m.id.as_str()));
            seen += 1;
        }
    }
    assert!(seen >= 3);
}

// ---- attachments read before the first turn (SPEC §11 #25) ----

/// `echo`'s `text` param stands in for an attachment param.
const ATTACH_ECHO: &str = "[tools]\nenabled = [\"echo\"]\nattachment_params = [\"text\"]\n";

/// Boots, then promotes `mock` and reloads, as an operator would.
async fn boot_trusted(cfg: Config, url: &str, extra: &str, model: Arc<Model>) -> SharedState {
    std::fs::create_dir_all(&cfg.plugins_dir).unwrap();
    write_manifest(&cfg.plugins_dir, "mock", url, "third-party", extra);
    let state = boot(cfg.clone(), model).await;
    let mut file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg.tool_policy_path).unwrap()).unwrap();
    file["plugins"]["mock"]["level"] = "trusted".into();
    file["plugins"]["mock"]["source"] = "operator".into();
    std::fs::write(&cfg.tool_policy_path, file.to_string()).unwrap();
    reload(&state).await;
    state
}

fn transcript_events(cfg: &Config) -> Vec<serde_json::Value> {
    std::fs::read_to_string(&cfg.transcript_log_path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn unambiguous_attachment_tool_runs_before_the_model() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("prefill");
    let cfg = config(&dir, false);
    let model = Model::new("", vec![CompletionOutput::Final("It says hi.".into())]);
    let state = boot_trusted(cfg.clone(), &url, ATTACH_ECHO, model.clone()).await;

    let r = send_with_image(&state, "What this says?").await;
    assert_eq!(r.text, "It says hi.");
    let events = transcript_events(&cfg);
    let call = events
        .iter()
        .find(|e| e["kind"] == "tool_call")
        .expect("prefilled call recorded");
    assert_eq!(call["call_id"], "prefill-0");
    assert_eq!(call["tool"], "mock.echo");
    assert!(call["args"]["text"].as_str().unwrap().starts_with("att_"));
    assert!(events.iter().any(|e| e["kind"] == "tool_result"));
    // One model turn, after the read, with the full catalogue back.
    assert_eq!(model.offered.lock().unwrap().len(), 1);
    assert_eq!(model.offered.lock().unwrap()[0], vec!["mock.echo"]);
}

#[tokio::test]
async fn untrusted_attachment_tool_asks_before_reading() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("prefill-untrusted");
    let cfg = config(&dir, false);
    std::fs::create_dir_all(&cfg.plugins_dir).unwrap();
    write_manifest(&cfg.plugins_dir, "mock", &url, "third-party", ATTACH_ECHO);
    let model = Model::new("", vec![CompletionOutput::Final("unused".into())]);
    let state = boot(cfg.clone(), model.clone()).await;

    let r = send_with_image(&state, "What this says?").await;
    assert!(r.decision.expect("gate holds").prompt.contains("mock.echo"));
    assert!(
        model.offered.lock().unwrap().is_empty(),
        "model never asked"
    );
    assert!(
        !transcript_events(&cfg)
            .iter()
            .any(|e| e["kind"] == "tool_result")
    );
}

#[tokio::test]
async fn ambiguous_attachment_tools_get_one_nudge_instead() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("nudge");
    let cfg = config(&dir, false);
    let extra =
        "[tools]\nenabled = [\"echo\", \"lurch\"]\nattachment_params = [\"text\", \"first_ms\"]\n";
    let model = Model::new(
        "",
        vec![CompletionOutput::Final("I can't see images.".into())],
    );
    let state = boot_trusted(cfg.clone(), &url, extra, model.clone()).await;

    let r = send_with_image(&state, "What this says?").await;
    // Nudged once, then the second plain answer is delivered.
    assert_eq!(r.text, "I can't see images.");
    let last = model.last.lock().unwrap();
    assert_eq!(last.len(), 2);
    assert!(last[1].starts_with("You have not read the attached file yet"));
    assert!(last[1].contains("`first_ms` or `text`"));
    assert!(
        !transcript_events(&cfg)
            .iter()
            .any(|e| e["kind"] == "tool_call")
    );
}

#[tokio::test]
async fn extra_required_args_skip_the_prefill() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("prefill-required");
    let cfg = config(&dir, false);
    // `lurch` also requires `later_ms`, which the harness can't invent.
    let extra = "[tools]\nenabled = [\"lurch\"]\nattachment_params = [\"first_ms\"]\n";
    let model = Model::new("", vec![CompletionOutput::Final("ok".into())]);
    let state = boot_trusted(cfg.clone(), &url, extra, model.clone()).await;

    send_with_image(&state, "read it").await;
    assert!(
        !transcript_events(&cfg)
            .iter()
            .any(|e| e["kind"] == "tool_call")
    );
    assert_eq!(
        model.last.lock().unwrap().len(),
        2,
        "fell back to the nudge"
    );
}

#[tokio::test]
async fn orders_without_attachments_are_not_nudged() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("no-attach");
    let cfg = config(&dir, false);
    let model = Model::new("", vec![CompletionOutput::Final("hello".into())]);
    let state = boot_trusted(cfg, &url, ATTACH_ECHO, model.clone()).await;
    send(&state, "hi").await;
    assert_eq!(model.last.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_plugins_are_listed() {
    let url = common::mock_mcp::spawn().await.unwrap();
    let dir = temp_dir("failed-list");
    write_manifest(&dir, "dead", "http://127.0.0.1:1/mcp", "third-party", "");
    write_manifest(&dir, "live", &url, "first-party", "");
    let set = harness::plugins::load(&spec(&dir)).await.unwrap();
    let failed: Vec<&str> = set.failed().iter().map(|p| p.id.as_str()).collect();
    assert_eq!(failed, vec!["dead"]);
}
