//! Adapter-side rendering of `Response.decision` (SPEC §4.3) as an inline
//! keyboard. Telegram caps `callback_data` at 64 bytes, so buttons carry a
//! short key mapped back to `(decision id, value)` here. In-memory only:
//! after a restart old buttons resolve to nothing and degrade gracefully.

use contract::Decision;
use std::collections::{BTreeMap, HashMap};

/// Telegram's hard `callback_data` limit.
pub const MAX_CALLBACK_DATA: usize = 64;
/// Bound on live keys so a long-running adapter can't grow without limit.
const MAX_KEYS: usize = 512;
const OTHER_TITLE: &str = "✏️ Something else";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Send `decision_answer { id, value }`.
    Answer { id: String, value: String },
    /// The next text message in this chat answers `id` as free text.
    AwaitText { id: String },
}

#[derive(Debug, Clone)]
struct Entry {
    chat_id: i64,
    decision_id: String,
    /// `None` for the free-text button.
    value: Option<String>,
}

#[derive(Debug, Default)]
pub struct DecisionKeys {
    next: u64,
    keys: BTreeMap<u64, Entry>,
    awaiting_text: HashMap<i64, String>,
}

impl DecisionKeys {
    /// Registers one key per option (plus free text when allowed) and returns
    /// keyboard rows of `(title, callback_data)`.
    pub fn register(&mut self, chat_id: i64, d: &Decision) -> Vec<Vec<(String, String)>> {
        let mut options = Vec::new();
        for opt in &d.options {
            let key = self.insert(chat_id, &d.id, Some(opt.value.clone()));
            options.push((opt.title.clone(), key));
        }
        let mut rows = vec![options];
        if d.free_text {
            let key = self.insert(chat_id, &d.id, None);
            rows.push(vec![(OTHER_TITLE.to_owned(), key)]);
        }
        rows
    }

    /// Resolves a tapped button. Keys are bound to their chat, and all keys
    /// of the same decision are dropped so a second tap resolves to `None`.
    pub fn resolve(&mut self, chat_id: i64, data: &str) -> Option<Resolved> {
        let n: u64 = data.strip_prefix('k')?.parse().ok()?;
        let entry = self.keys.get(&n)?.clone();
        if entry.chat_id != chat_id {
            return None;
        }
        self.keys.retain(|_, e| e.decision_id != entry.decision_id);
        Some(match entry.value {
            Some(value) => Resolved::Answer {
                id: entry.decision_id,
                value,
            },
            None => {
                self.awaiting_text
                    .insert(chat_id, entry.decision_id.clone());
                Resolved::AwaitText {
                    id: entry.decision_id,
                }
            }
        })
    }

    /// Consumes the pending free-text answer for this chat, if any.
    pub fn take_awaiting_text(&mut self, chat_id: i64) -> Option<String> {
        self.awaiting_text.remove(&chat_id)
    }

    fn insert(&mut self, chat_id: i64, decision_id: &str, value: Option<String>) -> String {
        if self.keys.len() >= MAX_KEYS
            && let Some(oldest) = self.keys.keys().next().copied()
        {
            self.keys.remove(&oldest);
        }
        let n = self.next;
        self.next = self.next.wrapping_add(1);
        self.keys.insert(
            n,
            Entry {
                chat_id,
                decision_id: decision_id.to_owned(),
                value,
            },
        );
        let key = format!("k{n}");
        debug_assert!(key.len() <= MAX_CALLBACK_DATA);
        key
    }
}

/// Message body for a decision: the prompt plus one line per detail.
pub fn render_decision(d: &Decision) -> String {
    let mut out = d.prompt.clone();
    if !d.details.is_empty() {
        out.push('\n');
        for detail in &d.details {
            out.push_str(&format!("\n• {}: {}", detail.label, detail.value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::{DecisionDetail, DecisionKind, DecisionOption, OptionStyle};

    fn decision(id: &str, free_text: bool) -> Decision {
        Decision {
            id: id.into(),
            kind: DecisionKind::Confirm,
            prompt: "Run `x.create`?".into(),
            details: vec![DecisionDetail {
                label: "amount".into(),
                value: "42".into(),
            }],
            options: vec![
                DecisionOption {
                    value: "approve".into(),
                    title: "Confirm".into(),
                    style: OptionStyle::Primary,
                },
                DecisionOption {
                    value: "reject".into(),
                    title: "Cancel".into(),
                    style: OptionStyle::Danger,
                },
            ],
            free_text,
            expires_at: "2026-10-14T00:00:00Z".into(),
        }
    }

    #[test]
    fn rows_carry_short_callback_data_and_resolve_to_answers() {
        let mut keys = DecisionKeys::default();
        let id = format!("dcs_{}", "f".repeat(32));
        let rows = keys.register(7, &decision(&id, true));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].0, "Confirm");
        assert_eq!(rows[1][0].0, OTHER_TITLE);
        for (_, data) in rows.iter().flatten() {
            assert!(data.len() <= MAX_CALLBACK_DATA, "{data}");
        }
        assert_eq!(
            keys.resolve(7, &rows[0][0].1),
            Some(Resolved::Answer {
                id: id.clone(),
                value: "approve".into()
            })
        );
        assert_eq!(
            keys.resolve(7, &rows[0][1].1),
            None,
            "a decision's other buttons die with the first tap"
        );
    }

    #[test]
    fn keys_are_bound_to_their_chat_and_garbage_is_ignored() {
        let mut keys = DecisionKeys::default();
        let rows = keys.register(7, &decision("dcs_a", false));
        assert_eq!(keys.resolve(8, &rows[0][0].1), None);
        assert_eq!(keys.resolve(7, "nonsense"), None);
        assert_eq!(keys.resolve(7, "k999"), None);
        assert!(keys.resolve(7, &rows[0][0].1).is_some());
    }

    #[test]
    fn free_text_button_arms_the_next_message_once() {
        let mut keys = DecisionKeys::default();
        let rows = keys.register(7, &decision("dcs_a", true));
        assert_eq!(
            keys.resolve(7, &rows[1][0].1),
            Some(Resolved::AwaitText { id: "dcs_a".into() })
        );
        assert_eq!(keys.take_awaiting_text(8), None);
        assert_eq!(keys.take_awaiting_text(7), Some("dcs_a".into()));
        assert_eq!(keys.take_awaiting_text(7), None);
    }

    #[test]
    fn key_count_stays_bounded() {
        let mut keys = DecisionKeys::default();
        for i in 0..(MAX_KEYS * 2) {
            keys.register(7, &decision(&format!("dcs_{i}"), true));
        }
        assert!(keys.keys.len() <= MAX_KEYS);
    }

    #[test]
    fn render_lists_prompt_and_details() {
        let text = render_decision(&decision("dcs_a", false));
        assert_eq!(text, "Run `x.create`?\n\n• amount: 42");
    }
}
