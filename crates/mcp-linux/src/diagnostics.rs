//! Read-only Linux diagnostics (SPEC §4.2): the concrete tool list picked at
//! M1 — `disk_free`, `services_status`, `ports_listening`.
//!
//! Safety posture: every tool is read-only by construction. `services_status`
//! execs the fixed `systemctl` binary directly (no shell, arguments passed as
//! argv); everything else is pure file reads.

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde_json::{Value, json};
use std::sync::Arc;
/// Exec seam so parsers are testable without a real systemd box.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[String]) -> Result<String, String>;
}

/// Direct exec of the fixed systemctl binary — argv only, never a shell.
pub struct SystemCtl;

#[async_trait::async_trait]
impl CommandRunner for SystemCtl {
    async fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MountArgs {
    /// Only report mounts under this path (exact match or prefix).
    #[schemars(description = "optional mount path filter, e.g. /srv")]
    pub mount: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UnitArgs {
    #[schemars(description = "optional systemd unit name or glob pattern")]
    pub unit: Option<String>,
}

#[derive(Clone)]
pub struct LinuxDiag {
    runner: Arc<dyn CommandRunner>,
    tool_router: ToolRouter<Self>,
}

/// Filesystem types that carry no user data; listing them is noise.
const VIRTUAL_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devpts",
    "devtmpfs",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "overlay",
    "squashfs",
    "mqueue",
    "shm",
    "ramfs",
    "securityfs",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "bpf",
    "pstore",
    "efivarfs",
    "autofs",
    "hugetlbfs",
    "binfmt_misc",
    "nsfs",
];

impl LinuxDiag {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl LinuxDiag {
    #[tool(description = "Free and total space for mounted filesystems that hold user data")]
    async fn disk_free(&self, Parameters(args): Parameters<MountArgs>) -> CallToolResult {
        match self.disk_free_inner(args.mount.as_deref()) {
            Ok(v) => CallToolResult::success(vec![
                rmcp::model::ContentBlock::json(v).expect("serializes"),
            ]),
            Err(e) => error_result(e),
        }
    }

    #[tool(description = "systemd service units with their load/active/sub state")]
    async fn services_status(&self, Parameters(args): Parameters<UnitArgs>) -> CallToolResult {
        match self.services_inner(args.unit.as_deref()).await {
            Ok(v) => CallToolResult::success(vec![
                rmcp::model::ContentBlock::json(v).expect("serializes"),
            ]),
            Err(e) => error_result(e),
        }
    }

    #[tool(description = "TCP sockets currently in the listening state")]
    async fn ports_listening(&self) -> CallToolResult {
        match ports_listening_inner() {
            Ok(v) => CallToolResult::success(vec![rmcp::model::ContentBlock::json(v).expect("ok")]),
            Err(e) => error_result(e),
        }
    }
}

fn error_result(problem: String) -> CallToolResult {
    // Environment problems surface as tool errors the model can read and
    // relay; they are application-level failures and never retried (SPEC §8).
    CallToolResult::error(vec![
        rmcp::model::ContentBlock::json(json!({ "error": problem })).expect("serializes"),
    ])
}

impl LinuxDiag {
    fn disk_free_inner(&self, filter: Option<&str>) -> Result<Value, String> {
        let mounts = read_mounts().map_err(|e| format!("cannot list mounts: {e}"))?;
        let mut out = Vec::new();
        for (path, fstype) in mounts {
            if VIRTUAL_FS.contains(&fstype.as_str()) {
                continue;
            }
            if let Some(f) = filter
                && path != f
                && !path.starts_with(&format!("{f}/"))
            {
                continue;
            }
            let stat = nix::sys::statvfs::statvfs(std::path::Path::new(&path))
                .map_err(|e| format!("statvfs {path}: {e}"))?;
            let block = stat.fragment_size();
            out.push(json!({
                "mount": path,
                "fstype": fstype,
                "total_bytes": stat.blocks() as u64 * block as u64,
                "available_bytes": stat.blocks_available() as u64 * block as u64,
            }));
        }
        Ok(Value::Array(out))
    }

    async fn services_inner(&self, pattern: Option<&str>) -> Result<Value, String> {
        let mut args: Vec<String> = [
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--plain",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(p) = pattern {
            // Single argv element — no shell metacharacters involved.
            args.push(p.to_owned());
        }
        let stdout = self
            .runner
            .run("systemctl", &args)
            .await
            .map_err(|e| format!("systemctl failed: {e}"))?;
        Ok(parse_systemctl_units(&stdout))
    }
}

fn ports_listening_inner() -> Result<Value, String> {
    let mut out = Vec::new();
    for (file, proto) in [("/proc/net/tcp", "tcp"), ("/proc/net/tcp6", "tcp6")] {
        let raw = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
        out.extend(parse_proc_net_tcp(&raw, proto));
    }
    Ok(json!({ "listening": out }))
}

/// `/proc/mounts` → (mount point, fs type) pairs.
pub fn read_mounts() -> Result<Vec<(String, String)>, String> {
    let raw = std::fs::read_to_string("/proc/mounts").map_err(|e| e.to_string())?;
    Ok(raw
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let _device = cols.next()?;
            let mount = cols.next()?;
            let fstype = cols.next()?;
            Some((unescape_mount(mount), fstype.to_owned()))
        })
        .collect())
}

/// Octal escapes like `\040` (space) appear in /proc/mounts paths.
pub fn unescape_mount(mount: &str) -> String {
    let mut out = String::with_capacity(mount.len());
    let mut chars = mount.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Kernel uses OCTAL escapes in /proc/mounts (\040 = space).
            let octal: String = (&mut chars).take(3).collect();
            if let Ok(byte) = u8::from_str_radix(&octal, 8) {
                out.push(byte as char);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// `list-units --no-legend --plain` lines: UNIT LOAD ACTIVE SUB DESCRIPTION…
pub fn parse_systemctl_units(stdout: &str) -> Value {
    let units: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 4 {
                return None;
            }
            json!({
                "unit": cols[0],
                "load": cols[1],
                "active": cols[2],
                "sub": cols[3],
                "description": cols[4..].join(" "),
            })
            .into()
        })
        .collect();
    json!(units)
}

/// One `/proc/net/tcp{,6}` entry per socket in listening state (`0A`).
pub fn parse_proc_net_tcp(raw: &str, proto: &str) -> Vec<Value> {
    raw.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // sl local_address rem_address st …
            if cols.len() < 4 || cols[3] != "0A" {
                return None;
            }
            let (ip, port) = cols[1].split_once(':')?;
            Some(json!({
                "proto": proto,
                "addr": decode_hex_ip(ip),
                "port": u16::from_str_radix(port, 16).ok()?,
            }))
        })
        .collect()
}

/// IPv4 words are little-endian inside the hex blob; IPv6 is 4 LE words.
pub fn decode_hex_ip(hex: &str) -> String {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    if bytes.len() == 4 {
        return format!("{}.{}.{}.{}", bytes[3], bytes[2], bytes[1], bytes[0]);
    }
    // IPv6: four 32-bit LE groups rendered as hextets.
    if bytes.len() == 16 {
        let mut groups = Vec::with_capacity(8);
        for word in bytes.chunks(4) {
            let le = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            groups.push(format!("{:x}", (le >> 16) as u16));
            groups.push(format!("{:x}", le as u16));
        }
        return groups.join(":");
    }
    hex.to_owned()
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LinuxDiag {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Read-only Linux diagnostics backend")
    }
}
