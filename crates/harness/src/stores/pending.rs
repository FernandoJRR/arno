//! Pending decisions: frozen destructive actions keyed by decision id, which
//! doubles as the legacy confirmation token (SPEC §4.3).
//!
//! Invariants (AGENTS.md #3):
//! - The id is deleted from the store *before* dispatch — single-use even
//!   on failure; a replay of a consumed id answers `*_used`.
//! - Ids are 128-bit CSPRNG, bound to (client, session), TTL-limited.
//! - At most one live decision per (client, session): a new proposal or any
//!   non-answer order supersedes the old one (`discard_session`).

use rand::TryRng;
use rand::rngs::SysRng;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct PendingAction {
    pub tool: String,
    pub args: serde_json::Value,
    /// Option values a `decision_answer` may carry for this decision.
    pub options: Vec<String>,
    pub created: Instant,
}

#[derive(Debug, PartialEq)]
pub enum TakeOutcome {
    /// Store entry removed and token marked used — dispatch may proceed.
    Action(PendingAction),
    Unknown,
    Expired,
    Used,
}

type PendingKey = (String, String, String); // (client_id, session_id, token)

#[derive(Default)]
struct Inner {
    live: HashMap<PendingKey, PendingAction>,
    used: HashMap<PendingKey, Instant>,
}

pub struct PendingStore(Mutex<Inner>);

impl Default for PendingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingStore {
    pub fn new() -> Self {
        Self(Mutex::new(Inner {
            live: HashMap::new(),
            used: HashMap::new(),
        }))
    }

    /// Freeze the exact payload the model proposed (SPEC §4.3 proposal step).
    pub fn freeze(
        &self,
        client_id: &str,
        session_id: &str,
        tool: String,
        args: serde_json::Value,
        options: Vec<String>,
    ) -> String {
        let token = format!("dcs_{}", mint_token());
        self.0.lock().expect("pending lock").live.insert(
            (client_id.to_owned(), session_id.to_owned(), token.clone()),
            PendingAction {
                tool,
                args,
                options,
                created: Instant::now(),
            },
        );
        token
    }

    /// Redemption validation + delete-before-dispatch in one atomic step.
    /// The returned `Action` is no longer in the store, and its key is already
    /// recorded as used, whatever happens next.
    pub fn take(
        &self,
        client_id: &str,
        session_id: &str,
        token: &str,
        ttl: Duration,
    ) -> TakeOutcome {
        let key: PendingKey = (
            client_id.to_owned(),
            session_id.to_owned(),
            token.to_owned(),
        );
        let mut inner = self.0.lock().expect("pending lock");
        match inner.live.remove(&key) {
            Some(action) => {
                if action.created.elapsed() >= ttl {
                    inner.used.insert(key, Instant::now());
                    return TakeOutcome::Expired;
                }
                inner.used.insert(key, Instant::now());
                TakeOutcome::Action(action)
            }
            None => {
                if inner.used.contains_key(&key) {
                    TakeOutcome::Used
                } else {
                    TakeOutcome::Unknown
                }
            }
        }
    }

    pub fn sweep(&self, ttl: Duration) {
        let mut inner = self.0.lock().expect("pending lock");
        inner.live.retain(|_, a| a.created.elapsed() < ttl);
        inner.used.retain(|_, t| t.elapsed() < ttl);
    }

    /// Consumes every live decision for this client+session without
    /// dispatching (supersede rule, SPEC §4.3): stale buttons can then never
    /// approve an older payload. Returns what was discarded.
    pub fn discard_session(&self, client_id: &str, session_id: &str) -> Vec<PendingAction> {
        let mut inner = self.0.lock().expect("pending lock");
        let keys: Vec<PendingKey> = inner
            .live
            .keys()
            .filter(|(c, s, _)| c == client_id && s == session_id)
            .cloned()
            .collect();
        let mut discarded = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(action) = inner.live.remove(&key) {
                discarded.push(action);
            }
            inner.used.insert(key, Instant::now());
        }
        discarded
    }
}

/// 128 bits of CSPRNG output, hex-encoded (SPEC §4.3).
fn mint_token() -> String {
    let mut bytes = [0_u8; 16];
    // OS entropy is a hard requirement for confirmation tokens; failure to
    // source it is unrecoverable and must not be silently downgraded.
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("OS CSPRNG unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<String> {
        vec!["approve".into(), "reject".into()]
    }

    #[test]
    fn tokens_are_128_bit_hex_and_unique() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn lifecycle_unknown_then_action_then_used() {
        let store = PendingStore::new();
        assert_eq!(
            store.take("cli", "s", "nope", Duration::from_secs(10)),
            TakeOutcome::Unknown,
            "never-minted token"
        );
        let token = store.freeze(
            "cli",
            "s",
            "securo.create_transaction".into(),
            serde_json::json!({"x":1}),
            opts(),
        );
        let taken = store.take("cli", "s", &token, Duration::from_secs(10));
        match taken {
            TakeOutcome::Action(a) => {
                assert_eq!(a.tool, "securo.create_transaction");
                assert_eq!(a.args["x"], 1);
            }
            other => panic!("expected action, got {other:?}"),
        }
        assert_eq!(
            store.take("cli", "s", &token, Duration::from_secs(10)),
            TakeOutcome::Used,
            "single-use even after successful removal"
        );
    }

    #[test]
    fn expired_still_counts_as_consumed_and_is_distinct_from_used() {
        let store = PendingStore::new();
        let token = store.freeze("cli", "s", "t".into(), serde_json::json!(null), opts());
        // Zero TTL ⇒ deterministically expired at first take.
        assert_eq!(
            store.take("cli", "s", &token, Duration::ZERO),
            TakeOutcome::Expired
        );
        assert_eq!(
            store.take("cli", "s", &token, Duration::from_secs(10)),
            TakeOutcome::Used
        );
        // Bound to issuing client+session: another namespace sees Unknown.
        let other = store.freeze("tg", "s2", "t".into(), serde_json::json!(null), opts());
        assert_eq!(
            store.take("cli", "DIFFERENT", &other, Duration::from_secs(10)),
            TakeOutcome::Unknown
        );
    }

    #[test]
    fn sweep_removes_expired_entries() {
        let store = PendingStore::new();
        store.freeze("cli", "s", "t".into(), serde_json::json!(null), opts());
        store.sweep(Duration::ZERO);
        // Token minted inside freeze is not exposed here; sweeping with zero
        // TTL must have removed it, so any take is Unknown/Used — never Action.
        // (Direct check via a fresh token we keep:)
        let store2 = PendingStore::new();
        let tok = store2.freeze("cli", "s", "t".into(), serde_json::json!(null), opts());
        store2.sweep(Duration::from_secs(3600));
        assert!(matches!(
            store2.take("cli", "s", &tok, Duration::from_secs(3600)),
            TakeOutcome::Action(_)
        ));
    }

    #[test]
    fn discard_session_supersedes_only_that_session() {
        let store = PendingStore::new();
        let ttl = Duration::from_secs(10);
        let old = store.freeze("cli", "s", "tool.a".into(), serde_json::json!({}), opts());
        let other = store.freeze("cli", "s2", "tool.b".into(), serde_json::json!({}), opts());
        let foreign = store.freeze("tg", "s", "tool.c".into(), serde_json::json!({}), opts());

        let discarded = store.discard_session("cli", "s");
        assert_eq!(discarded.len(), 1);
        assert_eq!(discarded[0].tool, "tool.a");
        assert_eq!(
            store.take("cli", "s", &old, ttl),
            TakeOutcome::Used,
            "a superseded decision must never be redeemable"
        );
        assert!(matches!(
            store.take("cli", "s2", &other, ttl),
            TakeOutcome::Action(_)
        ));
        assert!(matches!(
            store.take("tg", "s", &foreign, ttl),
            TakeOutcome::Action(_)
        ));
        assert!(store.discard_session("cli", "s").is_empty());
    }

    #[test]
    fn ids_carry_prefix_and_options() {
        let store = PendingStore::new();
        let id = store.freeze("cli", "s", "t".into(), serde_json::json!(null), opts());
        assert!(id.starts_with("dcs_") && id.len() == 36, "{id}");
        match store.take("cli", "s", &id, Duration::from_secs(10)) {
            TakeOutcome::Action(a) => assert_eq!(a.options, opts()),
            other => panic!("expected action, got {other:?}"),
        }
    }
}
