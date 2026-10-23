//! Long-polling bridge: Telegram `Update` → Harness `Order` → Telegram reply
//! (SPEC §5.1, §6). Single replica only — a second long-poller on the same
//! bot token gets HTTP 409 from Telegram (SPEC §5.1), so this must never be
//! scaled beyond one instance.
//!
//! A `Response.decision` (SPEC §4.3) is rendered as an inline keyboard; a
//! tap comes back as a callback query and is sent as `decision_answer`. The
//! harness owns the options; this adapter only renders them.

use contract::{Attachment, DecisionAnswer, ErrorCode, Order, Respond, Response};
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
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
use crate::pending::Pending;
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

/// What the update loop and the event listener share (SPEC §11 #32).
pub struct App {
    pub bot: Bot,
    pub client: HarnessClient,
    pub status: Status,
    pub keys: tokio::sync::Mutex<DecisionKeys>,
    pub pending: Pending,
    /// Set by the listener while `/v1/events` is open: only then are orders
    /// sent with `respond: stream`, so no answer depends on a stream that is
    /// not there.
    pub connected: AtomicBool,
    /// How long a stream answer may take before the chat is told it was lost.
    pub answer_deadline: Duration,
}

/// Status shown for an order waiting behind another one.
const QUEUED: &str = "Queued…";
const LOST: &str = "Lost track of that request — ask again.";
/// How often lost stream answers are looked for.
const SWEEP_EVERY: Duration = Duration::from_secs(30);

pub async fn run(bot: Bot, cfg: Config, client: HarnessClient) -> anyhow::Result<()> {
    let app = Arc::new(App {
        bot: bot.clone(),
        client,
        status: Status::default(),
        keys: tokio::sync::Mutex::new(DecisionKeys::default()),
        pending: Pending::default(),
        connected: AtomicBool::new(false),
        answer_deadline: cfg.http_timeout,
    });
    tokio::spawn(crate::events::listen(app.clone(), cfg.events_reconnect_max));
    tokio::spawn(sweep(app.clone()));
    // Explicit list: an omitted `allowed_updates` reuses whatever the token
    // was last polled with, which might exclude callback queries.
    let mut listener = Polling::builder(bot.clone())
        .timeout(std::time::Duration::from_secs(10))
        .allowed_updates(vec![AllowedUpdate::Message, AllowedUpdate::CallbackQuery])
        .delete_webhook()
        .await
        .build();
    let mut stream = std::pin::pin!(listener.as_stream());

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
            UpdateKind::Message(msg) => on_message(&app, &cfg, msg, client_msg_id).await,
            UpdateKind::CallbackQuery(q) => on_callback(&app, &cfg, q, client_msg_id).await,
            _ => {}
        }
    }

    Ok(())
}

/// Tells chats whose stream answer never came (SPEC §11 #32).
async fn sweep(app: Arc<App>) {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    loop {
        tick.tick().await;
        for chat in app.pending.expire(app.answer_deadline) {
            tracing::warn!(chat_id = chat.0, "stream answer never arrived");
            app.status.finish(&app.bot, chat, LOST.into(), None).await;
        }
    }
}

async fn on_message(app: &App, cfg: &Config, msg: Message, client_msg_id: String) {
    let bot = &app.bot;
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
        app.keys
            .lock()
            .await
            .take_awaiting_text(chat_id)
            .map(|id| DecisionAnswer {
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
    dispatch(app, msg.chat.id, order).await;
}

async fn on_callback(app: &App, cfg: &Config, q: CallbackQuery, client_msg_id: String) {
    let bot = &app.bot;
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

    let resolved = match q.data.as_deref() {
        Some(data) => app.keys.lock().await.resolve(chat.0, data),
        None => None,
    };
    match resolved {
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
            dispatch(app, chat, order).await;
        }
    }
}

/// Sends an order: as `respond: stream` while the event stream is up, so
/// the update loop is free at once; blocking otherwise (SPEC §11 #32).
async fn dispatch(app: &App, chat: ChatId, mut order: Order) {
    let key = order.client_msg_id.clone().unwrap_or_default();
    let Mode { stream, queued } = mode(
        app.connected.load(Ordering::Acquire),
        &key,
        app.pending.is_empty(),
    );
    app.status.begin(chat).await;
    if queued {
        app.status.progress(&app.bot, chat, QUEUED).await;
    }
    if !stream {
        let outcome = app.client.send_order(&order).await;
        return reply(app, chat, outcome).await;
    }
    order.respond = Some(Respond::Stream);
    app.pending.insert(key.clone(), chat);
    let outcome = app.client.send_order(&order).await;
    if let Ok(OrderOutcome::Accepted(order_id)) = &outcome {
        tracing::info!(%order_id, "order accepted; the answer comes on the stream");
        return;
    }
    // Answered here (an older harness, or a refusal): unless the stream
    // already delivered it.
    if app.pending.take(&key).is_some() {
        reply(app, chat, outcome).await;
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Mode {
    /// Send with `respond: stream`.
    stream: bool,
    /// Another stream order is still waiting: show "Queued…" at once.
    queued: bool,
}

fn mode(connected: bool, key: &str, nothing_pending: bool) -> Mode {
    let stream = connected && !key.is_empty();
    Mode {
        stream,
        queued: stream && !nothing_pending,
    }
}

/// Renders a harness outcome: a decision gets its keyboard, errors get
/// terse code-derived text, explained by readiness when it can say why.
async fn reply(app: &App, chat: ChatId, outcome: Result<OrderOutcome, reqwest::Error>) {
    match outcome {
        Ok(OrderOutcome::Ok(resp)) => deliver(app, chat, *resp).await,
        Ok(OrderOutcome::Rejected(code)) => fail(app, chat, code).await,
        Ok(OrderOutcome::Accepted(_)) => {}
        Err(e) => {
            tracing::warn!(error = %e, "harness API request failed");
            app.status
                .finish(
                    &app.bot,
                    chat,
                    "Couldn't reach the harness — try again shortly.".to_owned(),
                    None,
                )
                .await;
        }
    }
}

/// Shows an answer, from the order's reply or from the stream.
pub async fn deliver(app: &App, chat: ChatId, resp: Response) {
    let (text, markup) = render_response(&mut *app.keys.lock().await, chat.0, resp);
    app.status.finish(&app.bot, chat, text, markup).await;
}

/// Shows a failure, from the order's reply or from the stream.
pub async fn fail(app: &App, chat: ChatId, code: ErrorCode) {
    let mut text = render_error(code);
    if explains_outage(code)
        && let Some(why) = app
            .client
            .readiness()
            .await
            .as_ref()
            .and_then(outage_reason)
    {
        text.push(' ');
        text.push_str(why);
    }
    app.status.finish(&app.bot, chat, text, None).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_only_while_connected_and_queued_behind_a_waiting_order() {
        let m = |stream, queued| Mode { stream, queued };
        assert_eq!(mode(false, "1", true), m(false, false));
        assert_eq!(mode(false, "1", false), m(false, false));
        assert_eq!(mode(true, "", true), m(false, false));
        assert_eq!(mode(true, "1", true), m(true, false));
        assert_eq!(mode(true, "1", false), m(true, true));
    }
}
