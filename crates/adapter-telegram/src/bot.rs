//! Long-polling bridge: Telegram `Update` → Harness `Order` → Telegram reply
//! (SPEC §5.1, §6). Single replica only — a second long-poller on the same
//! bot token gets HTTP 409 from Telegram (SPEC §5.1), so this must never be
//! scaled beyond one instance.
//!
//! Confirmation (SPEC §4.3 revised) is now the harness's job, not this
//! adapter's — every message is just forwarded as-is; no per-chat token
//! store, no `[CONFIRM]`-only matching.

use futures::StreamExt;
use teloxide::prelude::*;
use teloxide::types::UpdateKind;
use teloxide::update_listeners::{AsUpdateStream, polling_default};

use crate::config::Config;
use crate::harness_client::{HarnessClient, OrderOutcome};
use crate::order::{build_order, render_error};

pub async fn run(bot: Bot, cfg: Config, client: HarnessClient) -> anyhow::Result<()> {
    let mut listener = polling_default(bot.clone()).await;
    let mut stream = std::pin::pin!(listener.as_stream());

    while let Some(update) = stream.next().await {
        let update = match update {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "update listener error");
                continue;
            }
        };

        let msg = match update.kind {
            UpdateKind::Message(msg) => msg,
            _ => continue,
        };

        // Echo-loop prevention: never act on a message authored by a bot
        // (including this one) — SPEC §5.1.
        if msg.from.as_ref().is_some_and(|u| u.is_bot) {
            continue;
        }

        let chat_id = msg.chat.id.0;
        if !cfg.allowed_chat_ids.contains(&chat_id) {
            tracing::warn!(
                chat_id,
                "dropped message from a chat outside ALLOWED_CHAT_IDS"
            );
            continue;
        }

        let Some(text) = msg.text() else { continue };
        let text = text.to_owned();
        let session_id = chat_id.to_string();
        let client_msg_id = update.id.0.to_string();

        let order = build_order(session_id, client_msg_id, text);

        let reply = match client.send_order(&order).await {
            Ok(OrderOutcome::Ok(resp)) => resp.text,
            Ok(OrderOutcome::Rejected(code)) => render_error(code),
            Err(e) => {
                tracing::warn!(error = %e, "harness API request failed");
                "Couldn't reach the harness — try again shortly.".to_owned()
            }
        };

        if let Err(e) = bot.send_message(msg.chat.id, reply).await {
            tracing::warn!(error = %e, "failed to send Telegram reply");
        }
    }

    Ok(())
}
