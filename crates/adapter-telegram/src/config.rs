//! Typed startup configuration (SPEC §5.7, STACK.md §7). Config arrives
//! exclusively via env vars; unknown vars under this adapter's owned
//! prefixes fail loudly (AGENTS.md #6) — a misspelled knob must not be
//! silently ignored.

use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_bot_token: String,
    /// Telegram chat IDs allowed to talk to this bot (SPEC §5.1, §7 layer 1).
    pub allowed_chat_ids: HashSet<i64>,
    pub harness_api_url: String,
    pub harness_api_token: String,
    /// Must exceed the harness's own `ORDER_BUDGET_S` (default 180s) —
    /// SPEC §5.7 "Adapters set their Harness-API HTTP timeout above
    /// ORDER_BUDGET_S."
    pub http_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown component env var(s): {0} — see SPEC §5.7 for the recognized set")]
    UnknownVars(String),
    #[error("{var}: {problem}")]
    BadValue { var: &'static str, problem: String },
}

const KNOWN_VARS: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "ALLOWED_CHAT_IDS",
    "HARNESS_API_URL",
    "HARNESS_API_TOKEN",
    "TELEGRAM_HTTP_TIMEOUT_S",
];

const OWNED_PREFIXES: &[&str] = &["TELEGRAM_", "ALLOWED_CHAT_IDS", "HARNESS_API_"];

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        reject_unknown()?;
        Ok(Self {
            telegram_bot_token: required("TELEGRAM_BOT_TOKEN")?,
            allowed_chat_ids: chat_ids("ALLOWED_CHAT_IDS")?,
            harness_api_url: required("HARNESS_API_URL")?,
            harness_api_token: required("HARNESS_API_TOKEN")?,
            http_timeout: duration_secs("TELEGRAM_HTTP_TIMEOUT_S", 240)?,
        })
    }
}

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

fn raw(var: &'static str) -> Option<String> {
    std::env::var(var).ok()
}

fn required(var: &'static str) -> Result<String, ConfigError> {
    raw(var)
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::BadValue {
            var,
            problem: "required but unset".into(),
        })
}

fn duration_secs(var: &'static str, default_s: u64) -> Result<Duration, ConfigError> {
    match raw(var) {
        None => Ok(Duration::from_secs(default_s)),
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| ConfigError::BadValue {
                var,
                problem: format!("cannot parse {text:?}"),
            }),
    }
}

/// Comma-separated Telegram chat IDs (SPEC §5.7); at least one required or
/// the bot would accept orders from nobody, i.e. never work.
fn chat_ids(var: &'static str) -> Result<HashSet<i64>, ConfigError> {
    let text = required(var)?;
    let mut set = HashSet::new();
    for part in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let id = part.parse::<i64>().map_err(|_| ConfigError::BadValue {
            var,
            problem: format!("not a valid chat id: {part:?}"),
        })?;
        set.insert(id);
    }
    if set.is_empty() {
        return Err(ConfigError::BadValue {
            var,
            problem: "at least one chat id required".into(),
        });
    }
    Ok(set)
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

    fn base_vars() -> Vec<&'static str> {
        vec![
            "TELEGRAM_BOT_TOKEN",
            "ALLOWED_CHAT_IDS",
            "HARNESS_API_URL",
            "HARNESS_API_TOKEN",
            "TELEGRAM_HTTP_TIMEOUT_S",
        ]
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

    fn set_required() {
        set("TELEGRAM_BOT_TOKEN", "bot-tok");
        set("ALLOWED_CHAT_IDS", "123,-456");
        set("HARNESS_API_URL", "http://127.0.0.1:8080");
        set("HARNESS_API_TOKEN", "client-tok");
    }

    #[test]
    fn defaults_and_required_vars() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set_required();
        let cfg = Config::from_env().expect("parses");
        assert_eq!(cfg.telegram_bot_token, "bot-tok");
        assert!(cfg.allowed_chat_ids.contains(&123));
        assert!(cfg.allowed_chat_ids.contains(&-456));
        assert_eq!(cfg.http_timeout, Duration::from_secs(240));
    }

    #[test]
    fn custom_timeout_overrides_default() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set_required();
        set("TELEGRAM_HTTP_TIMEOUT_S", "300");
        let cfg = Config::from_env().expect("parses");
        assert_eq!(cfg.http_timeout, Duration::from_secs(300));
    }

    #[test]
    fn missing_required_var_fails() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn empty_chat_id_list_fails() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set_required();
        set("ALLOWED_CHAT_IDS", "");
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn unknown_component_var_rejected() {
        let _lock = env_lock();
        let mut vars = base_vars();
        vars.push("TELEGRAM_BOT_TOKEM");
        let _clean = CleanEnv::new(&vars);
        set_required();
        set("TELEGRAM_BOT_TOKEM", "typo");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("TELEGRAM_BOT_TOKEM"), "{err}");
    }

    #[test]
    fn foreign_env_vars_are_not_our_business() {
        let _lock = env_lock();
        let _clean = CleanEnv::new(&base_vars());
        set_required();
        set("PATH", "/usr/bin");
        Config::from_env().expect("foreign vars tolerated");
    }
}
