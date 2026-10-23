//! Long-polling bridge: Telegram `Update` → Harness `Order` → Telegram reply
//! (SPEC §5.1, §6). Single replica only — a second long-poller on the same
//! bot token gets HTTP 409 from Telegram (SPEC §5.1), so this must never be
//! scaled beyond one instance.
//!
//! A `Response.decision` (SPEC §4.3) is rendered as an inline keyboard; a
//! tap comes back as a callback query and is sent as `decision_answer`. The
//! harness owns the options; this adapter only renders them.

use contract::{Attachment, DecisionAnswer, Response};
use futures::StreamExt;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    AllowedUpdate, CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, UpdateKind,
};
use teloxide::update_listeners::{AsUpdateStream, Polling};

use crate::config::Config;
use crate::decision::{DecisionKeys, Resolved, render_decision};
use crate::harness_client::{HarnessClient, OrderOutcome};
use crate::order::{build_order, encode_base64, explains_outage, outage_reason, render_error};
use crate::status::Status;

/// Telegram always re-encodes `photo`-type sends as JPEG. Must match
/// `ALLOWED_IMAGE_MIMES` in `crates/harness/src/stores/attachments.rs`
/// exactly, or the harness silently drops the attachment.
const PHOTO_MIME: &str = "image/jpeg";

/// Fetches a Telegram file's path (`get_file`) then streams its bytes
/// (`download_file`) into memory — the two-step dance teloxide requires for
/// any attachment download.
async fn download_photo(bot: &Bot, file_id: teloxide::types::FileId) -> anyhow::Result<Vec<u8>> {
    let file = bot.get_file(file_id).await?;
    let mut bytes = Vec::new();
    bot.download_file(&file.path, &mut bytes).await?;
    Ok(bytes)
}

pub async fn run(bot: Bot, cfg: Config, client: HarnessClient) -> anyhow::Result<()> {
    let client = std::sync::Arc::new(client);
    let status = std::sync::Arc::new(Status::default());
    tokio::spawn(crate::events::listen(
        bot.clone(),
        client.clone(),
        status.clone(),
        cfg.events_reconnect_max,
    ));
    // Explicit list: an omitted `allowed_updates` reuses whatever the token
    // was last polled with, which might exclude callback queries.
    let mut listener = Polling::builder(bot.clone())
        .timeout(std::time::Duration::from_secs(10))
        .allowed_updates(vec![AllowedUpdate::Message, AllowedUpdate::CallbackQuery])
        .delete_webhook()
        .await
        .build();
    let mut stream = std::pin::pin!(listener.as_stream());
    let mut keys = DecisionKeys::default();

    while let Some(update) = stream.next().await {
        let update = match update {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "update listener error");
                continue;
            }
        };
        let client_msg_id = update.id.0.to_string();
        match update.kind {
            UpdateKind::Message(msg) => {
                on_message(&bot, &cfg, &client, &status, &mut keys, msg, client_msg_id).await
            }
            UpdateKind::CallbackQuery(q) => {
                on_callback(&bot, &cfg, &client, &status, &mut keys, q, client_msg_id).await
            }
            _ => {}
        }
    }

    Ok(())
}

async fn on_message(
    bot: &Bot,
    cfg: &Config,
    client: &HarnessClient,
    status: &Status,
    keys: &mut DecisionKeys,
    msg: Message,
    client_msg_id: String,
) {
    // Echo-loop prevention: never act on a message authored by a bot
    // (including this one) — SPEC §5.1.
    if msg.from.as_ref().is_some_and(|u| u.is_bot) {
        return;
    }

    let chat_id = msg.chat.id.0;
    if !cfg.allowed_chat_ids.contains(&chat_id) {
        tracing::warn!(
            chat_id,
            "dropped message from a chat outside ALLOWED_CHAT_IDS"
        );
        return;
    }

    let (text, attachments) = if let Some(sizes) = msg.photo() {
        // Telegram sends multiple resolutions per photo; take the
        // largest. Caption (if any) becomes the order text — contract
        // allows an empty `text` when it exists only to carry a photo.
        let photo = sizes.last().expect("photo() is never an empty slice");
        match download_photo(bot, photo.file.id.clone()).await {
            Ok(bytes) => (
                msg.caption().unwrap_or("").to_owned(),
                vec![Attachment {
                    mime: PHOTO_MIME.into(),
                    data_b64: Some(encode_base64(&bytes)),
                    attachment_ref: None,
                }],
            ),
            Err(e) => {
                tracing::warn!(error = %e, "failed to download telegram photo");
                send(
                    bot,
                    msg.chat.id,
                    "Couldn't download that photo — try sending it again.".into(),
                    None,
                )
                .await;
                return;
            }
        }
    } else if let Some(text) = msg.text() {
        (text.to_owned(), Vec::new())
    } else {
        return;
    };

    // After "Something else", the next text answers that decision; the
    // harness treats it as free text, which never approves.
    let decision_answer = if attachments.is_empty() {
        keys.take_awaiting_text(chat_id).map(|id| DecisionAnswer {
            id,
            value: None,
            text: Some(text.clone()),
        })
    } else {
        None
    };

    let order = build_order(
        chat_id.to_string(),
        client_msg_id,
        text,
        attachments,
        decision_answer,
    );
    status.begin(msg.chat.id).await;
    let outcome = client.send_order(&order).await;
    reply(bot, client, status, keys, msg.chat.id, outcome).await;
}

async fn on_callback(
    bot: &Bot,
    cfg: &Config,
    client: &HarnessClient,
    status: &Status,
    keys: &mut DecisionKeys,
    q: CallbackQuery,
    client_msg_id: String,
) {
    // Always acknowledge, or Telegram shows a spinner on the button.
    if let Err(e) = bot.answer_callback_query(q.id.clone()).await {
        tracing::warn!(error = %e, "failed to answer callback query");
    }
    let Some(message) = q.message.as_ref() else {
        return;
    };
    let chat = message.chat().id;
    if !cfg.allowed_chat_ids.contains(&chat.0) {
        tracing::warn!(
            chat_id = chat.0,
            "dropped callback from a chat outside ALLOWED_CHAT_IDS"
        );
        return;
    }
    // One tap per decision: clear the keyboard whatever happens next.
    if let Err(e) = bot.edit_message_reply_markup(chat, message.id()).await {
        tracing::warn!(error = %e, "failed to clear decision keyboard");
    }

    match q.data.as_deref().and_then(|d| keys.resolve(chat.0, d)) {
        None => {
            send(bot, chat, "This request expired — ask again.".into(), None).await;
        }
        Some(Resolved::AwaitText { .. }) => {
            send(bot, chat, "Type your answer.".into(), None).await;
        }
        Some(Resolved::Answer { id, value }) => {
            let order = build_order(
                chat.0.to_string(),
                client_msg_id,
                String::new(),
                Vec::new(),
                Some(DecisionAnswer {
                    id,
                    value: Some(value),
                    text: None,
                }),
            );
            status.begin(chat).await;
            let outcome = client.send_order(&order).await;
            reply(bot, client, status, keys, chat, outcome).await;
        }
    }
}

/// Renders a harness outcome: a decision gets its keyboard, errors get
/// terse code-derived text, explained by readiness when it can say why.
async fn reply(
    bot: &Bot,
    client: &HarnessClient,
    status: &Status,
    keys: &mut DecisionKeys,
    chat: ChatId,
    outcome: Result<OrderOutcome, reqwest::Error>,
) {
    let (text, markup) = match outcome {
        Ok(OrderOutcome::Ok(resp)) => render_response(keys, chat.0, *resp),
        Ok(OrderOutcome::Rejected(code)) => {
            let mut text = render_error(code);
            if explains_outage(code)
                && let Some(why) = client.readiness().await.as_ref().and_then(outage_reason)
            {
                text.push(' ');
                text.push_str(why);
            }
            (text, None)
        }
        Err(e) => {
            tracing::warn!(error = %e, "harness API request failed");
            (
                "Couldn't reach the harness — try again shortly.".to_owned(),
                None,
            )
        }
    };
    status.finish(bot, chat, text, markup).await;
}

fn render_response(
    keys: &mut DecisionKeys,
    chat_id: i64,
    resp: Response,
) -> (String, Option<InlineKeyboardMarkup>) {
    let Some(decision) = resp.decision else {
        return (resp.text, None);
    };
    let rows = keys.register(chat_id, &decision).into_iter().map(|row| {
        row.into_iter()
            .map(|(title, data)| InlineKeyboardButton::callback(title, data))
            .collect::<Vec<_>>()
    });
    (
        render_decision(&decision),
        Some(InlineKeyboardMarkup::new(rows)),
    )
}

pub async fn send(bot: &Bot, chat: ChatId, text: String, markup: Option<InlineKeyboardMarkup>) {
    let mut req = bot.send_message(chat, text);
    if let Some(markup) = markup {
        req = req.reply_markup(markup);
    }
    if let Err(e) = req.await {
        tracing::warn!(error = %e, "failed to send Telegram reply");
    }
}
