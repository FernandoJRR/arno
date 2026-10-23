//! One status message per chat while an order runs (SPEC §11 #32): sent on
//! the first progress step, edited on later ones, then edited into the
//! answer. The lock makes progress and the answer serial, so a late step can
//! never leave a status message behind.

use std::collections::HashMap;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, MessageId};

#[derive(Default)]
pub struct Status(tokio::sync::Mutex<HashMap<i64, Slot>>);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Slot {
    msg: Option<MessageId>,
}

/// What a progress step does to a chat's slot.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// No order of ours is running in that chat.
    Ignore,
    Send,
    Edit(MessageId),
}

fn on_progress(slot: Option<&Slot>) -> Step {
    match slot {
        None => Step::Ignore,
        Some(Slot { msg: None }) => Step::Send,
        Some(Slot { msg: Some(id) }) => Step::Edit(*id),
    }
}

pub fn status_text(step: &str) -> String {
    format!("⏳ {step}")
}

impl Status {
    /// An order for `chat` is about to be sent.
    pub async fn begin(&self, chat: ChatId) {
        self.0.lock().await.insert(chat.0, Slot::default());
    }

    pub async fn progress(&self, bot: &Bot, chat: ChatId, step: &str) {
        let mut slots = self.0.lock().await;
        let action = on_progress(slots.get(&chat.0));
        let text = status_text(step);
        match action {
            Step::Ignore => {}
            Step::Send => match bot.send_message(chat, text).await {
                Ok(msg) => {
                    if let Some(slot) = slots.get_mut(&chat.0) {
                        slot.msg = Some(msg.id);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to send status message"),
            },
            Step::Edit(id) => {
                if let Err(e) = bot.edit_message_text(chat, id, text).await {
                    tracing::debug!(error = %e, "failed to edit status message");
                }
            }
        }
    }

    /// Delivers the answer: into the status message when there is one.
    pub async fn finish(
        &self,
        bot: &Bot,
        chat: ChatId,
        text: String,
        markup: Option<InlineKeyboardMarkup>,
    ) {
        let mut slots = self.0.lock().await;
        if let Some(id) = slots.remove(&chat.0).and_then(|s| s.msg) {
            let mut req = bot.edit_message_text(chat, id, text.clone());
            if let Some(markup) = markup.clone() {
                req = req.reply_markup(markup);
            }
            match req.await {
                Ok(_) => return,
                Err(e) => tracing::warn!(error = %e, "failed to edit status into the answer"),
            }
        }
        crate::bot::send(bot, chat, text, markup).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_step_sends_later_ones_edit_and_a_finished_chat_ignores() {
        assert_eq!(on_progress(None), Step::Ignore);
        assert_eq!(on_progress(Some(&Slot::default())), Step::Send);
        let id = MessageId(9);
        assert_eq!(on_progress(Some(&Slot { msg: Some(id) })), Step::Edit(id));
        assert_eq!(status_text("Reading the image…"), "⏳ Reading the image…");
    }
}
