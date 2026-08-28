//! In-process rmcp mock backend (STACK.md §8): exercises registry discovery,
//! namespacing, dispatch, timeout → retry, and application-error passthrough
//! without any real MCP server.
//!
//! Tools:
//! - `echo`  — happy path, returns its input
//! - `lurch` — sleeps `first_ms` on call #1, `later_ms` afterwards; models a
//!   backend that recovers after a slow first call (retry-policy probe)
//! - `boom`  — returns an `is_error` result (application-level failure)

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EchoArgs {
    #[schemars(description = "text to echo back")]
    pub text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LurchArgs {
    #[schemars(description = "milliseconds to sleep on the first call")]
    pub first_ms: u64,
    #[schemars(description = "milliseconds to sleep on later calls")]
    pub later_ms: u64,
}

#[derive(Clone)]
pub struct MockMcp {
    calls: Arc<AtomicUsize>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MockMcp {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "echoes its text argument back")]
    fn echo(&self, Parameters(args): Parameters<EchoArgs>) -> String {
        args.text
    }

    #[tool(description = "slow on the first call, fast afterwards")]
    async fn lurch(&self, Parameters(args): Parameters<LurchArgs>) -> String {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let ms = if n == 0 { args.first_ms } else { args.later_ms };
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        format!("call {n} done")
    }

    #[tool(description = "always fails with an application error")]
    fn boom(&self) -> CallToolResult {
        CallToolResult::error(vec![rmcp::model::ContentBlock::text("detonated")])
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MockMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("arno test double")
    }
}

/// Serves the mock over Streamable HTTP on an ephemeral loopback port and
/// returns the endpoint URL for `BackendAddr::Http`.
pub async fn spawn() -> anyhow::Result<String> {
    // Fresh state per spawn so tests never observe each other's call counts.
    let service = StreamableHttpService::new(
        || Ok(MockMcp::new()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let url = format!("http://{addr}/mcp");
    tokio::spawn(async move {
        let router = axum::Router::new().nest_service("/mcp", service);
        axum::serve(listener, router).await.expect("mock mcp serve")
    });
    Ok(url)
}

/// Same mock, but records every inbound request's header value for
/// `header_name` — proves an authenticated `BackendAddr::Http` (e.g. the
/// Securo bearer token, SPEC §11 M2) actually reaches the wire, not just the
/// config parser.
pub async fn spawn_capturing_header(
    header_name: &'static str,
) -> anyhow::Result<(String, Arc<std::sync::Mutex<Option<String>>>)> {
    let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let captured_for_mw = captured.clone();
    let service = StreamableHttpService::new(
        || Ok(MockMcp::new()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let url = format!("http://{addr}/mcp");
    tokio::spawn(async move {
        let router =
            axum::Router::new()
                .nest_service("/mcp", service)
                .layer(axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        let captured = captured_for_mw.clone();
                        async move {
                            if let Some(v) = req.headers().get(header_name)
                                && let Ok(s) = v.to_str()
                            {
                                *captured.lock().unwrap() = Some(s.to_owned());
                            }
                            next.run(req).await
                        }
                    },
                ));
        axum::serve(listener, router).await.expect("mock mcp serve")
    });
    Ok((url, captured))
}
