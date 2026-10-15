//! Stdio round-trip against this crate's own binary — the same transport
//! pair the harness registry uses for `exec:` backends. Exercises the
//! handle-traversal guard only, so it needs no real tesseract install in CI
//! (unit coverage of the fake-engine path lives in `src/ocr.rs`).

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;

#[tokio::test]
async fn stdio_round_trip_lists_tools_and_rejects_traversal_handle() {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-ocr"));
    cmd.env("MCP_OCR_TRANSPORT", "stdio");
    cmd.env("MCP_OCR_ATTACH_DIR", std::env::temp_dir());
    let transport = TokioChildProcess::new(cmd).expect("child spawns");

    let client = ().serve(transport).await.expect("initialize ok");
    let tools = client.list_all_tools().await.expect("lists tools");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, vec!["extract_text"], "the v1 tool list is fixed");

    let args = serde_json::json!({"image_ref": "../etc/passwd"})
        .as_object()
        .unwrap()
        .clone();
    let result = client
        .call_tool(CallToolRequestParams::new("extract_text").with_arguments(args))
        .await
        .expect("call completes");
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["isError"], serde_json::json!(true), "{value:?}");

    let _ = client.cancel().await;
}
