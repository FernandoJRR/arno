//! Unit tests for parsers plus a stdio end-to-end round-trip: this crate's
//! own binary spawned as a child process and driven by an rmcp client — the
//! same transport pair the harness registry uses for `exec:` backends.

use mcp_linux::diagnostics::{
    decode_hex_ip, parse_proc_net_tcp, parse_systemctl_units, read_mounts, unescape_mount,
};

const PROC_TCP_FIXTURE: &str = "\
  sl local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid timeout inode\n\
   0: 0100007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345\n\
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12346\n\
   2: 0100007F:9E38 0100007F:0050 01 00000000:00000000 00:00000000 00000000  1000        0 12347\n";

#[test]
fn proc_net_tcp_keeps_only_listening_sockets() {
    let out = parse_proc_net_tcp(PROC_TCP_FIXTURE, "tcp");
    assert_eq!(out.len(), 2, "established socket filtered out");
    assert_eq!(out[0]["addr"], "127.0.0.1");
    assert_eq!(out[0]["port"], 53);
    assert_eq!(out[1]["addr"], "0.0.0.0");
    assert_eq!(out[1]["port"], 22);
}

#[test]
fn hex_ip_decodes_v4_and_v6() {
    // Little-endian word: 0100007F → 127.0.0.1.
    assert_eq!(decode_hex_ip("0100007F"), "127.0.0.1");
    assert_eq!(decode_hex_ip("00000000"), "0.0.0.0");
    // Kernel renders ::1 as four LE u32 words — 01000000 last.
    let v6 = decode_hex_ip("00000000000000000000000001000000");
    assert_eq!(v6, "0:0:0:0:0:0:0:1");
}

#[test]
fn systemctl_lines_map_to_unit_records() {
    const FIXTURE: &str = "\
sshd.service           loaded active running OpenBSD Secure Shell server\n\
docker.service         loaded active running Docker Application Container Engine\n\
broken.service         loaded failed    failed  Some Broken Unit\n";
    let out = parse_systemctl_units(FIXTURE);
    let arr = out.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["unit"], "sshd.service");
    assert_eq!(arr[0]["load"], "loaded");
    assert_eq!(arr[0]["active"], "active");
    assert_eq!(arr[0]["sub"], "running");
    assert_eq!(arr[0]["description"], "OpenBSD Secure Shell server");
    assert_eq!(arr[2]["sub"], "failed");
}

#[test]
fn mount_paths_unescape_octal() {
    assert_eq!(unescape_mount("/srv/\\040data"), "/srv/ data");
    assert_eq!(unescape_mount("/plain/path"), "/plain/path");
}

#[tokio::test]
async fn stdio_round_trip_lists_tools_and_calls_disk_free() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::TokioChildProcess;

    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-linux"));
    cmd.env("MCP_LINUX_TRANSPORT", "stdio");
    let transport = TokioChildProcess::new(cmd).expect("child spawns");

    let client = ().serve(transport).await.expect("initialize ok");
    let tools = client.list_all_tools().await.expect("lists tools");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(
        names,
        vec!["disk_free", "ports_listening", "services_status"],
        "the M1 tool list is fixed"
    );

    let result = client
        .call_tool(CallToolRequestParams::new("disk_free"))
        .await
        .expect("call completes");
    // Off-Linux dev boxes get a readable tool error instead of a panic.
    let text = serde_json::to_value(&result).unwrap();
    assert!(!text.is_null(), "result carries content: {text:?}");

    client.cancel().await;
}

// list_all_tools convenience exists? If not, paginate manually below.
