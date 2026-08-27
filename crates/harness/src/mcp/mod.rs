//! MCP connection manager + tool registry (SPEC §5.5, STACK.md §5).
//!
//! One rmcp client per configured backend (Streamable HTTP or stdio spawn),
//! discovery at startup, `<server>.<tool>` namespacing, merged schemas for
//! prompt assembly. Dispatch happens exclusively from the FIFO worker task
//! (AGENTS.md #5); destructive calls never reach [`McpRegistry`] because the
//! frozen-payload gate intercepts them first (SPEC §4.3).

use crate::config::BackendAddr;
use crate::orchestrator::ToolExecutor;
use contract::ErrorCode;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    PaginatedRequestParams,
};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::{ServiceExt, model::Tool};
use std::collections::HashMap;
use std::time::Duration;

/// Startup failures are loud and abort boot (AGENTS.md #6 spirit): a backend
/// listed in `MCP_SERVERS` that cannot be reached is a config error.
/// Runtime failures stay soft — they surface as `backend_unavailable`.
type BoxedError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("mcp backend `{server}` connect failed: {source}")]
    Connect { server: String, source: BoxedError },
    #[error("mcp backend `{server}` process spawn failed: {source}")]
    Spawn {
        server: String,
        source: std::io::Error,
    },
    #[error("mcp backend `{server}` discovery failed: {source}")]
    Discovery { server: String, source: BoxedError },
}

struct Backend {
    service: RunningService<RoleClient, ()>,
}

#[derive(Clone)]
struct Route {
    backend: usize,
    tool: String,
}

pub struct McpRegistry {
    backends: Vec<Backend>,
    /// namespaced tool name → backend index + original tool name. Exact-match
    /// routing avoids any ambiguity when a tool name itself contains a dot.
    routes: HashMap<String, Route>,
    schemas: Vec<serde_json::Value>,
    timeout: Duration,
}

impl McpRegistry {
    /// Connects to every configured backend and discovers its tools. Any
    /// failure aborts startup.
    pub async fn connect(
        servers: &[(String, BackendAddr)],
        timeout: Duration,
    ) -> Result<Self, RegistryError> {
        let mut backends = Vec::with_capacity(servers.len());
        let mut routes = HashMap::new();
        let mut schemas = Vec::new();

        for (name, addr) in servers {
            let service = match connect_backend(addr).await {
                Ok(s) => s,
                Err(ConnectFailure::Io(source)) => {
                    return Err(RegistryError::Spawn {
                        server: name.clone(),
                        source,
                    });
                }
                Err(ConnectFailure::Connect(source)) => {
                    return Err(RegistryError::Connect {
                        server: name.clone(),
                        source,
                    });
                }
            };

            // Discovery walks pagination until exhausted; schemas are static
            // for the process lifetime (compose-managed backends, SPEC §11.6).
            let mut cursor = None;
            let mut count = 0usize;
            loop {
                let params = cursor.map(|c| {
                    let mut p = PaginatedRequestParams::default();
                    p.cursor = Some(c);
                    p
                });
                let page = service.list_tools(params).await.map_err(|source| {
                    RegistryError::Discovery {
                        server: name.clone(),
                        source: Box::new(source),
                    }
                })?;
                for tool in page.tools {
                    let original = tool.name.to_string();
                    let namespaced = format!("{name}.{original}");
                    tracing::debug!(tool = %namespaced, "registered mcp tool");
                    routes.insert(
                        namespaced.clone(),
                        Route {
                            backend: backends.len(),
                            tool: original,
                        },
                    );
                    schemas.push(schema_value(&tool, &namespaced));
                    count += 1;
                }
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            tracing::info!(server = %name, tools = count, "mcp backend ready");
            backends.push(Backend { service });
        }

        Ok(Self {
            backends,
            routes,
            schemas,
            timeout,
        })
    }
}

async fn connect_backend(
    addr: &BackendAddr,
) -> Result<RunningService<RoleClient, ()>, ConnectFailure> {
    match addr {
        // Both transports converge on the same client handle type; everything
        // downstream of this match is transport-agnostic.
        BackendAddr::Http(url) => {
            let transport = rmcp::transport::StreamableHttpClientTransport::from_uri(url.as_str());
            ().serve(transport)
                .await
                .map_err(|e| ConnectFailure::Connect(Box::new(e)))
        }
        BackendAddr::Exec { program, args, env } => {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            // Set directly on the child, not inherited from harness's own
            // env — harness's strict validator (config.rs) would reject a
            // backend's own knobs (e.g. mcp-linux's MCP_LINUX_TRANSPORT) if
            // they had to pass through harness's environment instead.
            cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            // rmcp kills and reaps the child when the transport drops — no
            // zombie backends on harness exit.
            let transport =
                rmcp::transport::TokioChildProcess::new(cmd).map_err(ConnectFailure::Io)?;
            ().serve(transport)
                .await
                .map_err(|e| ConnectFailure::Connect(Box::new(e)))
        }
    }
}

enum ConnectFailure {
    Io(std::io::Error),
    Connect(BoxedError),
}

/// Provider-neutral tool schema (OpenAI function shape — what Ollama's native
/// `/api/chat` consumes; STACK.md §4).
fn schema_value(tool: &Tool, namespaced: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": namespaced,
            "description": tool.description.as_deref().unwrap_or_default(),
            "parameters": serde_json::Value::Object((*tool.input_schema).clone()),
        }
    })
}

/// Coerces model-produced arguments into the object MCP requires. Non-object
/// output becomes an empty argument set rather than a hard failure — the
/// schema told the model what to send; tolerate drift, let the tool respond.
fn arguments_object(args: &serde_json::Value) -> Option<JsonObject> {
    match args {
        serde_json::Value::Object(map) => Some(map.clone()),
        serde_json::Value::Null => Some(JsonObject::new()),
        _ => None,
    }
}

/// Flattens a CallToolResult to one JSON value for the session transcript:
/// structured content wins, then concatenated text blocks. `is_error` results
/// are returned as values on purpose — the model reads the error text and can
/// adapt (application-level failures are never retried, SPEC §8).
fn result_to_value(result: &CallToolResult) -> serde_json::Value {
    if let Some(structured) = &result.structured_content {
        return structured.clone();
    }
    let mut text = String::new();
    for block in &result.content {
        if let ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }
    if result.is_error == Some(true) && !text.is_empty() {
        return serde_json::json!({ "tool_error": text });
    }
    if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(text)
    }
}

/// Transport/timeout-classified dispatch with the spec's retry policy
/// (SPEC §8): read-only calls retry at most once; application-level errors
/// (`is_error`, JSON-RPC error) never retry.
enum Attempt {
    Done(Result<serde_json::Value, ErrorCode>),
    Retry(String),
}

impl McpRegistry {
    async fn dispatch(
        &self,
        tool: &str,
        args: &serde_json::Value,
        allow_retry: bool,
    ) -> Result<serde_json::Value, ErrorCode> {
        let Some(route) = self.routes.get(tool) else {
            // Hallucinated tool name: report back so the model self-corrects.
            return Ok(serde_json::json!({ "unknown_tool": tool }));
        };
        let Some(arguments) = arguments_object(args) else {
            return Ok(serde_json::json!({ "bad_arguments": "expected a JSON object" }));
        };

        let attempts = if allow_retry { 2 } else { 1 };
        for attempt in 0..attempts {
            let params =
                CallToolRequestParams::new(route.tool.clone()).with_arguments(arguments.clone());
            let backend = &self.backends[route.backend].service;
            let outcome = tokio::time::timeout(self.timeout, backend.call_tool_once(params)).await;
            match classify(outcome) {
                Attempt::Done(r) => return r,
                Attempt::Retry(problem) => {
                    tracing::warn!(
                        tool,
                        attempt,
                        problem = %problem,
                        "mcp call failed, considering retry"
                    );
                }
            }
        }
        tracing::warn!(tool, "mcp call failed after retry budget");
        Err(ErrorCode::BackendUnavailable)
    }
}

fn classify(
    outcome: Result<Result<CallToolResponse, ServiceError>, tokio::time::error::Elapsed>,
) -> Attempt {
    match outcome {
        Err(_) => Attempt::Retry("timeout".into()),
        Ok(Err(e)) => match e {
            // Application-level: the backend spoke, said no. Final answer.
            ServiceError::McpError(err) => {
                Attempt::Done(Ok(serde_json::Value::String(format!("tool error: {err}"))))
            }
            other => Attempt::Retry(other.to_string()),
        },
        Ok(Ok(CallToolResponse::Complete(result))) => Attempt::Done(Ok(result_to_value(&result))),
        // Tasks / input-required rounds are out of scope for v1 tools.
        Ok(Ok(other)) => Attempt::Done(Ok(serde_json::json!({
            "unsupported_response": format!("{other:?}")
        }))),
    }
}

#[async_trait::async_trait]
impl ToolExecutor for McpRegistry {
    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ErrorCode> {
        // Everything routed here passed the classifier: read-only by
        // definition, so the single transport retry applies (SPEC §8).
        self.dispatch(tool, args, true).await
    }

    async fn execute_frozen(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ErrorCode> {
        // Redemption path: frozen payload dispatched verbatim, never retried
        // — the confirmation gate IS the retry mechanism (SPEC §4.3, AGENTS.md #3).
        self.dispatch(tool, args, false).await
    }

    fn tool_schemas(&self) -> &[serde_json::Value] {
        &self.schemas
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> Tool {
        Tool::new(
            name.to_owned(),
            description.to_owned(),
            serde_json::json!({"type": "object", "properties": {}})
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    #[test]
    fn schema_uses_namespaced_openai_function_shape() {
        let v = schema_value(&tool("disk_free", "free space"), "linux.disk_free");
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "linux.disk_free");
        assert_eq!(v["function"]["description"], "free space");
        assert_eq!(v["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn result_flattening_prefers_structured_then_text() {
        let mut structured = CallToolResult::default();
        structured.structured_content = Some(serde_json::json!({"bytes": 42}));
        assert_eq!(
            result_to_value(&structured),
            serde_json::json!({"bytes": 42})
        );

        structured.structured_content = None;
        structured.content = vec![ContentBlock::text("a"), ContentBlock::text("b")];
        assert_eq!(
            result_to_value(&structured),
            serde_json::Value::String("a\nb".into())
        );

        // is_error surfaces as a tagged value the model can read.
        structured.is_error = Some(true);
        assert_eq!(
            result_to_value(&structured)["tool_error"],
            serde_json::Value::String("a\nb".into())
        );

        let empty = CallToolResult::default();
        assert_eq!(result_to_value(&empty), serde_json::Value::Null);
    }

    #[test]
    fn arguments_must_be_objects_or_absent() {
        assert!(arguments_object(&serde_json::json!({"path": "/"})).is_some());
        assert!(arguments_object(&serde_json::Value::Null).is_some());
        assert!(arguments_object(&serde_json::json!("nope")).is_none());
        assert!(arguments_object(&serde_json::json!(1)).is_none());
    }
}
