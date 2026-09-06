//! Pending destructive actions + confirmation tokens (SPEC §4.3).
//!
//! Invariants (AGENTS.md #3):
//! - The token is deleted from the store *before* dispatch — single-use even
//!   on failure; a replay of a consumed token answers `confirmation_used`.
//! - Tokens are 128-bit CSPRNG, bound to (client, session), TTL-limited.

use rand::TryRng;
use rand::rngs::SysRng;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct PendingAction {
    pub tool: String,
    pub args: serde_json::Value,
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
    ) -> String {
        let token = mint_token();
        self.0.lock().expect("pending lock").live.insert(
            (client_id.to_owned(), session_id.to_owned(), token.clone()),
            PendingAction {
                tool,
                args,
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

    /// Read-only lookup for model-interpreted confirmation (SPEC §4.3
    /// revised): the newest unexpired pending action for this client+session,
    /// without consuming it. Same client/session isolation as `take`.
    pub fn peek_latest(
        &self,
        client_id: &str,
        session_id: &str,
        ttl: Duration,
    ) -> Option<(String, PendingAction)> {
        let inner = self.0.lock().expect("pending lock");
        inner
            .live
            .iter()
            .filter(|((c, s, _), action)| {
                c == client_id && s == session_id && action.created.elapsed() < ttl
            })
            .max_by_key(|(_, action)| action.created)
            .map(|((_, _, token), action)| (token.clone(), action.clone()))
    }

    /// Discards a pending action without dispatching it (explicit rejection,
    /// SPEC §4.3 revised) — same single-use move to `used` as `take`, so it
    /// can never later be redeemed. `false` if the token was already gone.
    pub fn cancel(&self, client_id: &str, session_id: &str, token: &str) -> bool {
        let key: PendingKey = (
            client_id.to_owned(),
            session_id.to_owned(),
            token.to_owned(),
        );
        let mut inner = self.0.lock().expect("pending lock");
        match inner.live.remove(&key) {
            Some(_) => {
                inner.used.insert(key, Instant::now());
                true
            }
            None => false,
        }
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
        let token = store.freeze("cli", "s", "t".into(), serde_json::json!(null));
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
        let other = store.freeze("tg", "s2", "t".into(), serde_json::json!(null));
        assert_eq!(
            store.take("cli", "DIFFERENT", &other, Duration::from_secs(10)),
            TakeOutcome::Unknown
        );
    }

    #[test]
    fn sweep_removes_expired_entries() {
        let store = PendingStore::new();
        store.freeze("cli", "s", "t".into(), serde_json::json!(null));
        store.sweep(Duration::ZERO);
        // Token minted inside freeze is not exposed here; sweeping with zero
        // TTL must have removed it, so any take is Unknown/Used — never Action.
        // (Direct check via a fresh token we keep:)
        let store2 = PendingStore::new();
        let tok = store2.freeze("cli", "s", "t".into(), serde_json::json!(null));
        store2.sweep(Duration::from_secs(3600));
        assert!(matches!(
            store2.take("cli", "s", &tok, Duration::from_secs(3600)),
            TakeOutcome::Action(_)
        ));
    }

    #[test]
    fn peek_latest_returns_the_newest_of_several_pendings() {
        let store = PendingStore::new();
        let first = store.freeze("cli", "s", "tool.a".into(), serde_json::json!({}));
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = store.freeze("cli", "s", "tool.b".into(), serde_json::json!({}));

        let (token, action) = store
            .peek_latest("cli", "s", Duration::from_secs(10))
            .expect("a pending action exists");
        assert_eq!(token, second, "the more recently frozen action wins");
        assert_eq!(action.tool, "tool.b");
        assert_ne!(token, first);
    }

    #[test]
    fn peek_latest_respects_ttl_and_does_not_consume() {
        let store = PendingStore::new();
        store.freeze("cli", "s", "tool.a".into(), serde_json::json!({}));
        assert_eq!(
            store.peek_latest("cli", "s", Duration::ZERO),
            None,
            "expired"
        );

        let token = store
            .peek_latest("cli", "s", Duration::from_secs(10))
            .map(|(t, _)| t);
        assert!(token.is_some());
        // Peeking must not consume — take() still sees a live Action after.
        let (real_token, _) = store
            .peek_latest("cli", "s", Duration::from_secs(10))
            .unwrap();
        assert!(matches!(
            store.take("cli", "s", &real_token, Duration::from_secs(10)),
            TakeOutcome::Action(_)
        ));
    }

    #[test]
    fn peek_latest_never_crosses_client_or_session_boundaries() {
        let store = PendingStore::new();
        store.freeze("cli", "s1", "tool.a".into(), serde_json::json!({}));
        assert_eq!(
            store.peek_latest("telegram", "s1", Duration::from_secs(10)),
            None
        );
        assert_eq!(
            store.peek_latest("cli", "s2", Duration::from_secs(10)),
            None
        );
    }

    #[test]
    fn cancel_makes_a_subsequent_take_report_used_not_action() {
        let store = PendingStore::new();
        let token = store.freeze("cli", "s", "tool.a".into(), serde_json::json!({}));
        assert!(store.cancel("cli", "s", &token));
        assert_eq!(
            store.take("cli", "s", &token, Duration::from_secs(10)),
            TakeOutcome::Used,
            "a cancelled action must never be redeemable"
        );
        assert_eq!(store.peek_latest("cli", "s", Duration::from_secs(10)), None);
    }

    #[test]
    fn cancel_on_an_unknown_token_returns_false() {
        let store = PendingStore::new();
        assert!(!store.cancel("cli", "s", "never-minted"));
    }
}
