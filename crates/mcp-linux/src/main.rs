//! Home Server Harness — first MCP backend: read-only Linux diagnostics.
//!
//! Contract C server side (SPEC §4.2). Three tools, all read-only, no raw
//! shell anywhere: filesystem stats via statvfs, service state via direct
//! exec of the fixed `systemctl` binary, listening sockets via `/proc/net`.

use anyhow::Context;
use rmcp::{
    ServiceExt,
    transport::stdio,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::sync::Arc;

use mcp_linux::diagnostics::{LinuxDiag, SystemCtl};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Knobs (SPEC §5.7): MCP_LINUX_TRANSPORT=http|stdio, MCP_LINUX_BIND.
    let transport = std::env::var("MCP_LINUX_TRANSPORT").unwrap_or_else(|_| "http".into());
    match transport.as_str() {
        "stdio" => {
            let server = LinuxDiag::new(Arc::new(SystemCtl));
            let running = server.serve(stdio()).await?;
            running.waiting().await?;
            Ok(())
        }
        "http" => {
            let bind: std::net::SocketAddr = std::env::var("MCP_LINUX_BIND")
                .unwrap_or_else(|_| "127.0.0.1:9001".into())
                .parse()
                .context("MCP_LINUX_BIND is not a valid socket address")?;
            // Loopback/compose-internal by default; this backend must never
            // be reachable off-host (SPEC decision #2).
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(%bind, endpoint = "/mcp", "mcp-linux serving");
            // rmcp's default Host allowlist is loopback-only (DNS-rebinding
            // guard); it doesn't know the compose service hostname. The
            // compose network itself is already the trust boundary here
            // (SPEC decision #2 — no ports published to the host), so the
            // Host check is redundant on top of that and safe to disable.
            let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
            let service = StreamableHttpService::new(
                || Ok(LinuxDiag::new(Arc::new(SystemCtl))),
                Arc::new(LocalSessionManager::default()),
                config,
            );
            let router = axum::Router::new().nest_service("/mcp", service);
            axum::serve(listener, router).await?;
            Ok(())
        }
        other => Err(anyhow::anyhow!(
            "MCP_LINUX_TRANSPORT={other:?} invalid — expected http|stdio"
        )),
    }
}
