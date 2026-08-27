//! Typed startup configuration (SPEC §5.7, STACK.md §7).
//!
//! Rules (AGENTS.md #6): config arrives exclusively via env vars; unknown
//! component-relevant vars fail loudly; every knob has a documented default.

use crate::model::ollama::ThinkMode;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;

/// A configured MCP backend location (SPEC §4.2). Two transports:
/// Streamable HTTP (`name:http://host/mcp`) or stdio spawn
/// (`name:exec:[KEY=VALUE ...] /path/to/bin --flag value`). Leading
/// `KEY=VALUE` tokens (upper-snake-case keys) become the child's own env
/// vars, set directly on the spawned process rather than inherited from
/// harness's — harness's own env stays under its strict validator
/// (`reject_unknown` below), so a backend's config (e.g. mcp-linux's
/// `MCP_LINUX_TRANSPORT`) can never collide with it. The exec form splits on
/// whitespace — arguments/values containing spaces are not supported in v1.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendAddr {
    Http(reqwest::Url),
    Exec {
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// client_id → bearer token. Sessions, dedup and pendings are namespaced
    /// by client_id (SPEC §4.1) — cross-adapter hijacking impossible.
    pub client_tokens: HashMap<String, String>,
    pub ollama_url: reqwest::Url,
    pub model: String,
    pub num_ctx: u32,
    /// Model reasoning effort (native `/api/chat` `think` field); default off
    /// for deterministic low-latency tool selection.
    pub ollama_think: ThinkMode,
    pub ollama_timeout: Duration,
    pub mcp_tool_timeout: Duration,
    pub order_budget: Duration,
    pub max_tool_calls: u32,
    pub dedup_window: Duration,
    pub session_ttl: Duration,
    pub confirm_ttl: Duration,
    pub attach_max_bytes: usize,
    /// Exact namespaced tool names whose dispatch requires the frozen-payload
    /// gate (SPEC §11.9). Unlisted tools are treated read-only.
    pub destructive_tools: HashSet<String>,
    /// (server_name, endpoint) pairs from `NAME:endpoint,...`.
    pub mcp_servers: Vec<(String, BackendAddr)>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown component env var(s): {0} — see SPEC §5.7 for the recognized set")]
    UnknownVars(String),
    #[error("{var}: {problem}")]
    BadValue { var: &'static str, problem: String },
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        reject_unknown()?;
        Ok(Self {
            bind: socket("HARNESS_API_BIND", "127.0.0.1:8080")?,
            client_tokens: tokens("HARNESS_API_CLIENT_TOKENS")?,
            ollama_url: url_var("OLLAMA_URL", "http://127.0.0.1:11434")?,
            model: string_var("OLLAMA_MODEL", "qwen3:8b"),
            num_ctx: u32_var("OLLAMA_NUM_CTX", 32_768)?,
            ollama_think: parsed("OLLAMA_THINK", Some(ThinkMode::Off))?,
            ollama_timeout: duration_secs("OLLAMA_TIMEOUT_S", 120)?,
            mcp_tool_timeout: duration_secs("MCP_TOOL_TIMEOUT_S", 30)?,
            order_budget: duration_secs("ORDER_BUDGET_S", 180)?,
            max_tool_calls: u32_var("MAX_TOOL_CALLS", 8)?,
            dedup_window: duration_mins("DEDUP_WINDOW_MIN", 60)?,
            session_ttl: duration_hours("SESSION_TTL_H", 24)?,
            confirm_ttl: duration_mins("CONFIRM_TTL_MIN", 10)?,
            attach_max_bytes: usize_var("ATTACH_MAX_BYTES", 10 * 1024 * 1024)?,
            destructive_tools: list("DESTRUCTIVE_TOOLS")?.into_iter().collect(),
            mcp_servers: servers("MCP_SERVERS")?,
        })
    }
}

/// Component-owned variable namespaces. Anything set under these prefixes that
/// is not a recognized knob aborts startup instead of being silently ignored.
const KNOWN_VARS: &[&str] = &[
    "HARNESS_API_BIND",
    "HARNESS_API_CLIENT_TOKENS",
    "OLLAMA_URL",
    "OLLAMA_MODEL",
    "OLLAMA_NUM_CTX",
    "OLLAMA_THINK",
    "OLLAMA_TIMEOUT_S",
    "MCP_TOOL_TIMEOUT_S",
    "MCP_SERVERS",
    "ORDER_BUDGET_S",
    "MAX_TOOL_CALLS",
    "DEDUP_WINDOW_MIN",
    "SESSION_TTL_H",
    "CONFIRM_TTL_MIN",
    "ATTACH_MAX_BYTES",
    "DESTRUCTIVE_TOOLS",
];

const OWNED_PREFIXES: &[&str] = &[
    "HARNESS_",
    "OLLAMA_",
    "MCP_",
    "ORDER_BUDGET",
    "MAX_TOOL_CALLS",
    "DEDUP_WINDOW",
    "SESSION_TTL",
    "CONFIRM_TTL",
    "ATTACH_MAX",
    "DESTRUCTIVE_",
];

fn reject_unknown() -> Result<(), ConfigError> {
    let offenders: Vec<String> = std::env::vars()
        .filter(|(k, _)| {
            OWNED_PREFIXES.iter().any(|p| k.starts_with(p)) && !KNOWN_VARS.contains(&k.as_str())
        })
        .map(|(k, _)| k)
        .collect();
    if offenders.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::UnknownVars(offenders.join(", ")))
    }
}

fn raw(var: &'static str) -> Result<Option<String>, ConfigError> {
    match std::env::var(var) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::BadValue {
            var,
            problem: e.to_string(),
        }),
    }
}

fn string_var(var: &'static str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_owned())
}

fn parsed<T: std::str::FromStr>(var: &'static str, default: Option<T>) -> Result<T, ConfigError> {
    let text = match raw(var)? {
        Some(t) => t,
        None => {
            return default.ok_or(ConfigError::BadValue {
                var,
                problem: "required but unset".into(),
            });
        }
    };
    text.trim().parse().map_err(|_| ConfigError::BadValue {
        var,
        problem: format!("cannot parse {text:?}"),
    })
}

fn u32_var(var: &'static str, default: u32) -> Result<u32, ConfigError> {
    parsed(var, Some(default))
}

fn usize_var(var: &'static str, default: usize) -> Result<usize, ConfigError> {
    parsed(var, Some(default))
}

fn duration_secs(var: &'static str, default_s: u64) -> Result<Duration, ConfigError> {
    parsed::<u64>(var, Some(default_s)).map(Duration::from_secs)
}

fn duration_mins(var: &'static str, default_min: u64) -> Result<Duration, ConfigError> {
    parsed::<u64>(var, Some(default_min)).map(|m| Duration::from_secs(m.saturating_mul(60)))
}

fn duration_hours(var: &'static str, default_h: u64) -> Result<Duration, ConfigError> {
    parsed::<u64>(var, Some(default_h)).map(|h| Duration::from_secs(h.saturating_mul(3600)))
}

fn socket(var: &'static str, default: &str) -> Result<SocketAddr, ConfigError> {
    parsed(var, Some(default.to_owned()))?
        .parse()
        .map_err(|_| ConfigError::BadValue {
            var,
            problem: "not a valid socket address".into(),
        })
}

fn url_var(var: &'static str, default: &str) -> Result<reqwest::Url, ConfigError> {
    let text = string_var(var, default);
    reqwest::Url::parse(text.trim()).map_err(|_| ConfigError::BadValue {
        var,
        problem: format!("not a valid URL: {text:?}"),
    })
}

fn list(var: &'static str) -> Result<Vec<String>, ConfigError> {
    Ok(match raw(var)? {
        Some(t) => t
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        None => Vec::new(),
    })
}

/// Comma-separated `client_id:token` pairs — one per adapter (SPEC §5.7).
fn tokens(var: &'static str) -> Result<HashMap<String, String>, ConfigError> {
    let mut map = HashMap::new();
    for item in list(var)? {
        let (id, token) = item.split_once(':').ok_or(ConfigError::BadValue {
            var,
            problem: format!("expected client_id:token, got {item:?}"),
        })?;
        if id.is_empty() || token.is_empty() {
            return Err(ConfigError::BadValue {
                var,
                problem: format!("empty part in {item:?}"),
            });
        }
        if map.insert(id.to_owned(), token.to_owned()).is_some() {
            return Err(ConfigError::BadValue {
                var,
                problem: format!("duplicate client_id {id:?}"),
            });
        }
    }
    if map.is_empty() {
        return Err(ConfigError::BadValue {
            var,
            problem: "at least one client token required".into(),
        });
    }
    Ok(map)
}

/// Whether a token before `=` reads as an env var key (upper-snake-case) —
/// distinguishes a leading `KEY=VALUE` from the program path in an exec
/// endpoint. Must be non-empty and start with a letter or `_`, since a bare
/// `=VALUE` or a digit-led token is never a valid env var name.
fn is_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Comma-separated `name:endpoint` pairs (SPEC §4.2), e.g.
/// `linux-mcp:http://127.0.0.1:9001/mcp` or
/// `linux-mcp:exec:MCP_LINUX_TRANSPORT=stdio /usr/local/bin/mcp-linux`.
/// Empty allowed until M1.
fn servers(var: &'static str) -> Result<Vec<(String, BackendAddr)>, ConfigError> {
    list(var)?
        .into_iter()
        .map(|item| {
            // Split at the FIRST ':' — everything after it is the endpoint,
            // which may itself contain ':' (port).
            let (name, endpoint) = item.split_once(':').ok_or(ConfigError::BadValue {
                var,
                problem: format!("expected name:endpoint, got {item:?}"),
            })?;
            if name.is_empty() {
                return Err(ConfigError::BadValue {
                    var,
                    problem: format!("empty server name in {item:?}"),
                });
            }
            let addr = if let Some(cmdline) = endpoint.strip_prefix("exec:") {
                let mut parts = cmdline.split_whitespace().peekable();
                let mut env = Vec::new();
                while let Some(tok) = parts.peek() {
                    match tok.split_once('=') {
                        Some((k, v)) if is_env_key(k) => {
                            env.push((k.to_owned(), v.to_owned()));
                            parts.next();
                        }
                        _ => break,
                    }
                }
                let program = parts.next().ok_or(ConfigError::BadValue {
                    var,
                    problem: format!("exec endpoint without program in {item:?}"),
                })?;
                BackendAddr::Exec {
                    program: program.to_owned(),
                    args: parts.map(str::to_owned).collect(),
                    env,
                }
            } else {
                if !endpoint.contains("://") {
                    return Err(ConfigError::BadValue {
                        var,
                        problem: format!(
                            "expected name:scheme://… endpoint or name:exec:program, got {item:?}"
                        ),
                    });
                }
                let url = reqwest::Url::parse(endpoint).map_err(|_| ConfigError::BadValue {
                    var,
                    problem: format!("bad endpoint URL in {item:?}"),
                })?;
                BackendAddr::Http(url)
            };
            Ok((name.to_owned(), addr))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn set(k: &str, v: &str) {
        // SAFETY: single-threaded under env_lock(); test-only.
        unsafe { std::env::set_var(k, v) };
    }

    fn unset(k: &str) {
        unsafe { std::env::remove_var(k) };
    }

    struct CleanEnv(Vec<&'static str>);
    impl CleanEnv {
        fn new(vars: &[&'static str]) -> Self {
            for v in vars {
                unset(v);
            }
            Self(vars.to_vec())
        }
    }
    impl Drop for CleanEnv {
        fn drop(&mut self) {
            for v in &self.0 {
                unset(v);
            }
        }
    }

    fn base_vars() -> Vec<&'static str> {
        vec![
            "HARNESS_API_BIND",
            "HARNESS_API_CLIENT_TOKENS",
            "OLLAMA_URL",
            "OLLAMA_MODEL",
            "OLLAMA_NUM_CTX",
            "OLLAMA_THINK",
            "OLLAMA_TIMEOUT_S",
            "MCP_TOOL_TIMEOUT_S",
            "MCP_SERVERS",
            "ORDER_BUDGET_S",
            "MAX_TOOL_CALLS",
            "DEDUP_WINDOW_MIN",
            "SESSION_TTL_H",
            "CONFIRM_TTL_MIN",
            "ATTACH_MAX_BYTES",
            "DESTRUCTIVE_TOOLS",
        ]
    }

    #[test]
    fn defaults_and_required_tokens() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set("HARNESS_API_CLIENT_TOKENS", "cli:secret");
        let cfg = Config::from_env().expect("defaults parse");
        assert_eq!(cfg.bind.to_string(), "127.0.0.1:8080");
        assert_eq!(cfg.num_ctx, 32_768);
        assert_eq!(cfg.ollama_think, ThinkMode::Off);
        assert_eq!(cfg.order_budget, Duration::from_secs(180));
        assert_eq!(cfg.session_ttl, Duration::from_secs(24 * 3600));
        assert_eq!(cfg.confirm_ttl, Duration::from_secs(600));
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn unknown_component_vars_rejected() {
        let _lock = env_lock();
        // The typo var itself must be cleaned up too, or later tests' scans fail.
        let mut vars = base_vars();
        vars.push("HARNESS_API_TOKNE");
        let _clean = CleanEnv::new(&vars);
        set("HARNESS_API_CLIENT_TOKENS", "cli:s");
        set("HARNESS_API_TOKNE", "typo"); // misspelled knob must fail loudly
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HARNESS_API_TOKNE"), "{err}");
    }

    #[test]
    fn foreign_env_vars_are_not_our_business() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set("HARNESS_API_CLIENT_TOKENS", "cli:s");
        set("PATH", "/usr/bin"); // not ours — must not fail
        Config::from_env().expect("foreign vars tolerated");
    }

    #[test]
    fn knobs_parse_over_defaults() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set("HARNESS_API_CLIENT_TOKENS", "tg:t1,cli:t2");
        set("DESTRUCTIVE_TOOLS", "securo.create_transaction");
        set("MCP_SERVERS", "linux-mcp:http://127.0.0.1:9001/mcp");
        set("OLLAMA_NUM_CTX", "16384");
        set("OLLAMA_THINK", "high");
        let cfg = Config::from_env().expect("parses");
        assert_eq!(cfg.client_tokens.get("tg").map(String::as_str), Some("t1"));
        assert!(cfg.destructive_tools.contains("securo.create_transaction"));
        assert_eq!(cfg.mcp_servers[0].0, "linux-mcp");
        match &cfg.mcp_servers[0].1 {
            BackendAddr::Http(url) => {
                assert_eq!(url.as_str(), "http://127.0.0.1:9001/mcp");
            }
            other => panic!("expected http backend, got {other:?}"),
        }
        assert_eq!(cfg.num_ctx, 16_384);
        assert_eq!(cfg.ollama_think, ThinkMode::High);
    }

    #[test]
    fn exec_and_malformed_server_endpoints() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set("HARNESS_API_CLIENT_TOKENS", "cli:s");
        set(
            "MCP_SERVERS",
            "linux-mcp:exec:/usr/local/bin/mcp-linux --transport stdio, bad:, x:no-slash",
        );
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("bad:"), "{err}");

        let _clean2 = CleanEnv::new(&base_vars());
        set("HARNESS_API_CLIENT_TOKENS", "cli:s");
        set(
            "MCP_SERVERS",
            "linux-mcp:exec:/bin/mcp-linux --transport stdio",
        );
        let cfg = Config::from_env().expect("exec form parses");
        match &cfg.mcp_servers[0].1 {
            BackendAddr::Exec { program, args, env } => {
                assert_eq!(program, "/bin/mcp-linux");
                assert_eq!(
                    args.iter().map(String::as_str).collect::<Vec<_>>(),
                    ["--transport", "stdio"]
                );
                assert!(env.is_empty());
            }
            other => panic!("expected exec backend, got {other:?}"),
        }
    }

    #[test]
    fn exec_leading_key_value_tokens_become_child_env_not_harness_env() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set("HARNESS_API_CLIENT_TOKENS", "cli:s");
        set(
            "MCP_SERVERS",
            "linux-mcp:exec:MCP_LINUX_TRANSPORT=stdio RUST_LOG=info /usr/local/bin/mcp-linux --flag",
        );
        let cfg = Config::from_env().expect("exec form with env vars parses");
        match &cfg.mcp_servers[0].1 {
            BackendAddr::Exec { program, args, env } => {
                assert_eq!(program, "/usr/local/bin/mcp-linux");
                assert_eq!(
                    args.iter().map(String::as_str).collect::<Vec<_>>(),
                    ["--flag"]
                );
                assert_eq!(
                    env.iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect::<Vec<_>>(),
                    [("MCP_LINUX_TRANSPORT", "stdio"), ("RUST_LOG", "info")]
                );
            }
            other => panic!("expected exec backend, got {other:?}"),
        }
    }

    #[test]
    fn exec_env_tokens_stop_at_first_non_key_value_token() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set("HARNESS_API_CLIENT_TOKENS", "cli:s");
        // "a=b" doesn't look like an env key (lowercase) — the whole thing is
        // read as the program path, matching a real relative path.
        set("MCP_SERVERS", "linux-mcp:exec:a=b");
        let cfg = Config::from_env().expect("parses");
        match &cfg.mcp_servers[0].1 {
            BackendAddr::Exec { program, args, env } => {
                assert_eq!(program, "a=b");
                assert!(args.is_empty());
                assert!(env.is_empty());
            }
            other => panic!("expected exec backend, got {other:?}"),
        }
    }

    #[test]
    fn missing_or_malformed_tokens_fail() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        assert!(Config::from_env().is_err(), "tokens required");
        set("HARNESS_API_CLIENT_TOKENS", "no-separator");
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::BadValue {
                var: "HARNESS_API_CLIENT_TOKENS",
                ..
            })
        ));
    }
}
