//! Contract tests for the MCP registry (SPEC §5.5, STACK.md §8): discovery,
//! namespacing, dispatch, timeout/retry policy, and startup failure modes —
//! all against an in-process rmcp mock, never a real backend.

mod common;

use common::mock_mcp;
use contract::ErrorCode;
use harness::config::BackendAddr;
use harness::mcp::McpRegistry;
use harness::orchestrator::ToolExecutor;
use std::time::Duration;

fn http(addr: String) -> Vec<(String, BackendAddr)> {
    vec![("mock".into(), BackendAddr::Http(addr.parse().unwrap()))]
}

#[tokio::test]
async fn discovery_namespaces_tools_and_dispatch_round_trips() {
    let url = mock_mcp::spawn().await.expect("mock spawns");
    let reg = McpRegistry::connect(&http(url), Duration::from_secs(5))
        .await
        .expect("connects");

    // Schema is namespaced and provider-neutral (OpenAI function shape).
    let schemas = reg.tool_schemas();
    assert_eq!(schemas.len(), 3, "echo + lurch + boom");
    let echo = schemas
        .iter()
        .find(|s| s["function"]["name"] == "mock.echo")
        .expect("echo schema present");
    assert_eq!(echo["type"], "function");
    assert!(
        echo["function"]["parameters"]["properties"]["text"].is_object(),
        "input schema carried through"
    );

    // Namespaced dispatch routes and flattens the result.
    let out = reg
        .execute("mock.echo", &serde_json::json!({"text": "ping"}))
        .await
        .expect("dispatch ok");
    assert_eq!(out, serde_json::Value::String("ping".into()));
}

#[tokio::test]
async fn unknown_tool_reports_back_instead_of_erroring() {
    let url = mock_mcp::spawn().await.expect("mock spawns");
    let reg = McpRegistry::connect(&http(url), Duration::from_secs(5))
        .await
        .expect("connects");
    let out = reg
        .execute("mock.nope", &serde_json::json!({}))
        .await
        .expect("unknown tools are values, not errors");
    assert_eq!(out["unknown_tool"], "mock.nope");
}

#[tokio::test]
async fn timeout_exhausts_retry_budget_then_backend_unavailable() {
    let url = mock_mcp::spawn().await.expect("mock spawns");
    // Every call sleeps 400ms against a 150ms budget: both attempts time out.
    let reg = McpRegistry::connect(&http(url), Duration::from_millis(150))
        .await
        .expect("connects");
    let err = reg
        .execute(
            "mock.lurch",
            &serde_json::json!({"first_ms": 400, "later_ms": 400}),
        )
        .await
        .expect_err("both attempts time out");
    assert_eq!(err, ErrorCode::BackendUnavailable);
}

#[tokio::test]
async fn retry_once_recovers_a_slow_backend_but_frozen_calls_never_retry() {
    let url = mock_mcp::spawn().await.expect("mock spawns");
    // Call #1 sleeps 300ms against a 120ms budget; call #2 returns instantly.
    let reg = McpRegistry::connect(&http(url), Duration::from_millis(120))
        .await
        .expect("connects");
    let args = serde_json::json!({"first_ms": 300, "later_ms": 0});

    // Read-only path: attempt 1 times out, attempt 2 succeeds.
    let out = reg
        .execute("mock.lurch", &args)
        .await
        .expect("retry recovers");
    assert!(out.as_str().unwrap().contains("done"));

    // Frozen-payload path on fresh state: one attempt only → unavailable.
    let url2 = mock_mcp::spawn().await.expect("second mock spawns");
    let frozen = McpRegistry::connect(&http(url2), Duration::from_millis(120))
        .await
        .expect("second connects");
    let err = frozen
        .execute_frozen("mock.lurch", &args)
        .await
        .expect_err("frozen dispatch never retries");
    assert_eq!(err, ErrorCode::BackendUnavailable);
}

#[tokio::test]
async fn application_errors_pass_through_as_values_without_retry() {
    let url = mock_mcp::spawn().await.expect("mock spawns");
    let reg = McpRegistry::connect(&http(url), Duration::from_secs(5))
        .await
        .expect("connects");
    let out = reg
        .execute("mock.boom", &serde_json::json!({}))
        .await
        .expect("app errors are values for the model");
    assert_eq!(out["tool_error"], "detonated");
}

#[tokio::test]
async fn unreachable_configured_backend_aborts_startup() {
    // Nothing listens here — connect must fail loudly (config error).
    let dead = "http://127.0.0.1:1/mcp".to_owned();
    let Err(err) = McpRegistry::connect(&http(dead), Duration::from_secs(1)).await else {
        panic!("fail-fast on unreachable backend");
    };
    assert!(
        err.to_string().contains("connect failed"),
        "error is a connect failure naming the backend: {err}"
    );
}

#[tokio::test]
async fn exec_spawn_failure_is_a_distinct_startup_error() {
    let servers = vec![(
        "ghost".to_owned(),
        BackendAddr::Exec {
            program: "/nonexistent/arno-mock-bin".into(),
            args: vec![],
            env: vec![],
        },
    )];
    let Err(err) = McpRegistry::connect(&servers, Duration::from_secs(1)).await else {
        panic!("missing binary fails loudly");
    };
    assert!(
        err.to_string().contains("ghost"),
        "error names the backend: {err}"
    );
}
