//! Home Server Harness — Part B core binary (SPEC §5.2).

use harness::{api, config, queue, stores};

use harness::model::ollama::OllamaProvider;
use harness::orchestrator::ToolExecutor;
use harness::plugins::{LoadSpec, PluginRegistry};
use harness::state::{AppState, SharedState};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::from_env().map_err(|e| {
        // Fail loudly on config problems (AGENTS.md #6).
        anyhow::anyhow!("startup configuration invalid: {e}")
    })?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cfg))
}

async fn run(cfg: config::Config) -> anyhow::Result<()> {
    // Scratch, not durable state (AGENTS.md #7): anything left in ATTACH_DIR
    // from a prior process is stale — handles live for one order only.
    if let Err(e) = std::fs::create_dir_all(&cfg.attach_dir) {
        tracing::warn!(error = %e, dir = %cfg.attach_dir.display(), "attachment scratch dir create failed at boot");
    }
    stores::attachments::purge_all(&cfg.attach_dir);

    let (order_tx, order_rx) = tokio::sync::mpsc::channel::<queue::Job>(64);

    let provider = Arc::new(OllamaProvider::new(
        cfg.ollama_url.clone(),
        cfg.model.clone(),
        cfg.ollama_timeout,
        cfg.ollama_think,
        cfg.ollama_seed,
        cfg.ollama_top_k,
        cfg.ollama_num_predict,
    ));
    // Fail fast on manifest errors and unreachable legacy backends; a broken
    // manifest plugin is skipped and reported instead (SPEC §11 #21).
    let plugin_set = harness::plugins::load(&LoadSpec::from_config(&cfg))
        .await
        .map_err(|e| anyhow::anyhow!("plugin load failed: {e}"))?;

    // Tamper-evidence is the point (SPEC §12.1) — a corrupt audit log aborts
    // boot rather than silently starting a fresh chain, same fail-loud
    // precedent as an unreachable configured MCP backend above.
    let audit = harness::audit::AuditLog::open(
        &cfg.audit_log_path,
        cfg.audit_log_rotate_bytes,
        cfg.log_keep_segments,
    )
    .map_err(|e| anyhow::anyhow!("audit log open failed: {e}"))?;

    // Same fail-loud posture for the conversation/tool-call transcript
    // (SPEC §12.2) — a separate file from the audit log by design (§12.2).
    let transcript = harness::transcript::TranscriptLog::open(
        &cfg.transcript_log_path,
        cfg.transcript_log_rotate_bytes,
        cfg.log_keep_segments,
    )
    .map_err(|e| anyhow::anyhow!("transcript log open failed: {e}"))?;

    // Adaptive tool-safety classification (SPEC §4.2, M3) and plugin trust
    // (SPEC §11 #22): reconcile the persisted policy against what's actually
    // discovered, then classify anything unknown/changed right now. A down
    // model provider leaves those tools `Pending` (fail-closed to
    // destructive) rather than blocking boot — the retry task below picks
    // them up once Ollama answers.
    let policy = Arc::new(harness::policy::ToolPolicy::load(
        cfg.tool_policy_path.clone(),
        cfg.destructive_tools.clone(),
    ));
    harness::plugins::activate(
        &plugin_set,
        &policy,
        provider.as_ref(),
        &cfg.plugins_report_path,
    )
    .await;
    if cfg.router_enabled {
        tracing::info!("tool-group router enabled (SPEC §11 #23)");
    }
    if cfg.confirm_mode.is_some() {
        tracing::warn!("CONFIRM_MODE is deprecated and ignored (SPEC §11 #18)");
    }

    let state: SharedState = Arc::new(AppState {
        sessions: stores::sessions::SessionStore::new(),
        dedup: stores::dedup::DedupStore::new(),
        pending: stores::pending::PendingStore::new(),
        provider,
        plugins: Arc::new(PluginRegistry::new(plugin_set)),
        cfg,
        order_tx: order_tx.clone(),
        policy,
        audit: Some(audit),
        transcript: Some(transcript),
    });
    spawn_reload_on_sighup(order_tx.clone());
    drop(order_tx); // router holds its clone; keep only one producer handle alive in state

    tokio::spawn(queue::spawn_worker(state.clone(), order_rx));
    spawn_sweeper(state.clone());
    spawn_policy_retry(state.clone());

    let bind = state.cfg.bind;
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "harness listening (localhost-only per SPEC §11.2)");
    axum::serve(listener, api::routes::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// TTL sweeps for sessions and pending actions — deterministic housekeeping
/// runs on timers, never on the model (SPEC §8).
fn spawn_sweeper(state: SharedState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let dropped_sessions = state.sessions.sweep(state.cfg.session_ttl);
            state.pending.sweep(state.cfg.confirm_ttl);
            if dropped_sessions > 0 {
                tracing::debug!(dropped_sessions, "session TTL sweep");
            }
        }
    });
}

/// Retries tool classification left `Pending` after boot (e.g. Ollama was
/// down) — "whenever Ollama comes back to life" per the design decision this
/// implements. A model call here never sits on the order path: dispatch
/// always consults the already-persisted policy synchronously (SPEC §4.2, via
/// [`harness::policy::ToolPolicy::is_destructive`]); this task only ever
/// narrows an existing fail-closed/destructive default toward a real verdict.
fn spawn_policy_retry(state: SharedState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(state.cfg.tool_policy_retry);
        loop {
            tick.tick().await;
            let pending = state.policy.pending_tools();
            if pending.is_empty() {
                continue;
            }
            let schemas = state.plugins.snapshot().tool_schemas().to_vec();
            let (resolved, still_pending) = state
                .policy
                .classify_pending(state.provider.as_ref(), &schemas, &pending)
                .await;
            if resolved > 0 {
                tracing::info!(
                    resolved,
                    still_pending,
                    "tool policy retry resolved pending tools"
                );
            } else {
                tracing::debug!(still_pending, "tool policy retry: still unresolved");
            }
        }
    });
}

/// SIGHUP queues a plugin reload; it runs between orders (SPEC §11 #24).
#[cfg(unix)]
fn spawn_reload_on_sighup(tx: tokio::sync::mpsc::Sender<queue::Job>) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "SIGHUP handler not installed; reload needs a restart");
            return;
        }
    };
    tokio::spawn(async move {
        while hup.recv().await.is_some() {
            tracing::info!("SIGHUP: plugin reload queued");
            if tx.send(queue::Job::Reload(None)).await.is_err() {
                break;
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_reload_on_sighup(_tx: tokio::sync::mpsc::Sender<queue::Job>) {}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("ctrl_c handler installs");
    tracing::info!("shutdown signal received");
}
