//! Orders sent with `respond: stream` whose answer has not arrived yet
//! (SPEC §11 #32), keyed by `client_msg_id`: it is known before the POST,
//! so an answer that beats the `202` is still matched.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use teloxide::types::ChatId;

#[derive(Default)]
pub struct Pending(Mutex<HashMap<String, Entry>>);

struct Entry {
    chat: ChatId,
    sent: Instant,
}

impl Pending {
    pub fn insert(&self, key: String, chat: ChatId) {
        self.0.lock().expect("pending lock").insert(
            key,
            Entry {
                chat,
                sent: Instant::now(),
            },
        );
    }

    /// The chat waiting for `key`, once.
    pub fn take(&self, key: &str) -> Option<ChatId> {
        self.0
            .lock()
            .expect("pending lock")
            .remove(key)
            .map(|e| e.chat)
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().expect("pending lock").is_empty()
    }

    /// Removes and returns the chats waiting longer than `deadline`.
    pub fn expire(&self, deadline: Duration) -> Vec<ChatId> {
        let mut map = self.0.lock().expect("pending lock");
        let late: Vec<String> = map
            .iter()
            .filter(|(_, e)| e.sent.elapsed() > deadline)
            .map(|(k, _)| k.clone())
            .collect();
        late.iter()
            .filter_map(|k| map.remove(k))
            .map(|e| e.chat)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_taken_once_and_late_ones_expire() {
        let p = Pending::default();
        assert!(p.is_empty());
        p.insert("1".into(), ChatId(7));
        p.insert("2".into(), ChatId(8));
        assert_eq!(p.take("1"), Some(ChatId(7)));
        assert_eq!(p.take("1"), None);
        assert_eq!(p.take("unknown"), None);
        assert!(p.expire(Duration::from_secs(60)).is_empty());
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(p.expire(Duration::ZERO), [ChatId(8)]);
        assert!(p.is_empty());
    }
}
