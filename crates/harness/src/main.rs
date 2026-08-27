//! Home Server Harness — Part B core binary (SPEC §5.2).

use harness::{api, config, queue, stores};

use harness::model::ollama::OllamaProvider;
use harness::orchestrator::{NoBackends, ToolExecutor};
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
    let (order_tx, order_rx) = tokio::sync::mpsc::channel::<queue::OrderJob>(64);

    let provider = Arc::new(OllamaProvider::new(
        cfg.ollama_url.clone(),
        cfg.model.clone(),
        cfg.ollama_timeout,
        cfg.ollama_think,
    ));
    let executor: Arc<dyn ToolExecutor> = if cfg.mcp_servers.is_empty() {
        tracing::info!("MCP_SERVERS empty — tool dispatch reports backend_unavailable");
        Arc::new(NoBackends)
    } else {
        // Fail fast: an unreachable configured backend is a config error.
        // Runtime failures stay soft via backend_unavailable (SPEC §5.5).
        match harness::mcp::McpRegistry::connect(&cfg.mcp_servers, cfg.mcp_tool_timeout).await {
            Ok(registry) => Arc::new(registry),
            Err(e) => return Err(anyhow::anyhow!("mcp registry startup failed: {e}")),
        }
    };

    let state: SharedState = Arc::new(AppState {
        sessions: stores::sessions::SessionStore::new(),
        dedup: stores::dedup::DedupStore::new(),
        pending: stores::pending::PendingStore::new(),
        provider,
        executor,
        cfg,
        order_tx: order_tx.clone(),
    });
    drop(order_tx); // router holds its clone; keep only one producer handle alive in state

    tokio::spawn(queue::spawn_worker(state.clone(), order_rx));
    spawn_sweeper(state.clone());

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

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("ctrl_c handler installs");
    tracing::info!("shutdown signal received");
}
