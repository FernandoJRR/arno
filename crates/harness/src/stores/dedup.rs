//! `client_msg_id` dedup window (SPEC §4.1).
//!
//! Memory-only LRU of recently seen ids; repeats inside the window are
//! rejected (`duplicate_order`) and never re-executed. A restart clears the
//! window — acceptable because cross-restart duplicates are already bounded
//! by adapter offset handling.

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

type DedupKey = (String, String); // (client_id, client_msg_id)

/// Capacity is an implementation bound (oldest entries fall out), not a
/// behavioral knob — the window duration is what defines semantics.
const DEDUP_LRU_CAP: usize = 4096;

pub struct DedupStore(Mutex<LruCache<DedupKey, Instant>>);

impl Default for DedupStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DedupStore {
    pub fn new() -> Self {
        Self(Mutex::new(LruCache::new(
            NonZeroUsize::try_from(DEDUP_LRU_CAP).expect("cap nonzero"),
        )))
    }

    /// Returns true when this exact (client, message id) was seen inside
    /// `window` — callers must then reject with `duplicate_order` without
    /// executing. Otherwise records the id now.
    pub fn check_and_record(&self, client_id: &str, msg_id: &str, window: Duration) -> bool {
        let key: DedupKey = (client_id.to_owned(), msg_id.to_owned());
        let mut guard = self.0.lock().expect("dedup lock");
        if guard.peek(&key).is_some_and(|seen| seen.elapsed() < window) {
            return true;
        }
        guard.put(key, Instant::now());
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_within_window_detected_per_client() {
        let store = DedupStore::new();
        let window = Duration::from_secs(60);
        assert!(!store.check_and_record("cli", "42", window));
        assert!(
            store.check_and_record("cli", "42", window),
            "repeat in window"
        );
        // Same native id under a different adapter client is NOT a duplicate:
        // namespaces are per-client (SPEC §4.1).
        assert!(!store.check_and_record("tg", "42", window));
    }

    #[test]
    fn expired_entry_can_be_reused() {
        let store = DedupStore::new();
        assert!(!store.check_and_record("cli", "7", Duration::from_nanos(1)));
        // elapsed >= 1ns by the time we check again → outside window.
        std::thread::sleep(Duration::from_millis(2));
        assert!(!store.check_and_record("cli", "7", Duration::from_nanos(1)));
    }
}
