//! Per-session conversation history (SPEC §4.1).
//!
//! In-memory only; a harness restart clears everything and adapters start
//! fresh (accepted behavior, not a gap). Idle expiry via `SESSION_TTL_H`.

use crate::model::ChatMessage;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type SessionKey = (String, String); // (client_id, session_id)

struct Session {
    history: Vec<ChatMessage>,
    last_active: Instant,
}

#[derive(Default)]
pub struct SessionStore(Mutex<HashMap<SessionKey, Session>>);

impl SessionStore {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    pub fn append(&self, key: &SessionKey, msg: ChatMessage) {
        let mut guard = self.0.lock().expect("session lock");
        let session = guard.entry(key.clone()).or_insert_with(|| Session {
            history: Vec::new(),
            last_active: Instant::now(),
        });
        session.history.push(msg);
        session.last_active = Instant::now();
    }

    pub fn snapshot(&self, key: &SessionKey) -> Vec<ChatMessage> {
        self.0
            .lock()
            .expect("session lock")
            .get(key)
            .map(|s| s.history.clone())
            .unwrap_or_default()
    }

    /// Drops idle sessions; returns how many were removed.
    pub fn sweep(&self, ttl: Duration) -> usize {
        let mut guard = self.0.lock().expect("session lock");
        let before = guard.len();
        guard.retain(|_, s| s.last_active.elapsed() < ttl);
        before - guard.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> SessionKey {
        ("cli".into(), id.into())
    }

    #[test]
    fn append_and_snapshot_scopes_by_client_and_session() {
        let store = SessionStore::new();
        store.append(&key("a"), ChatMessage::new(crate::model::Role::User, "hi"));
        assert_eq!(store.snapshot(&key("a")).len(), 1);
        // Same session_id under another client is a different namespace.
        let other_client: SessionKey = ("tg".into(), "a".into());
        assert!(store.snapshot(&other_client).is_empty());
    }

    #[test]
    fn sweep_drops_only_idle_sessions() {
        let store = SessionStore::new();
        store.append(&key("old"), ChatMessage::new(crate::model::Role::User, "x"));
        // Backdate by inserting and sweeping with zero TTL.
        assert_eq!(store.sweep(Duration::ZERO), 1);
        store.append(
            &key("fresh"),
            ChatMessage::new(crate::model::Role::User, "y"),
        );
        assert_eq!(store.sweep(Duration::from_secs(3600)), 0);
        assert_eq!(store.snapshot(&key("fresh")).len(), 1);
    }
}
