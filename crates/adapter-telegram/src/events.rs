//! Follows the harness's `GET /v1/events` stream (SPEC §11 #32): progress
//! steps drive the chat's status message, and the answers of
//! `respond: stream` orders are delivered from it. Reconnects with backoff,
//! resuming after the last event id it saw.

use crate::bot::{App, deliver, fail};
use crate::sse::Parser;
use contract::{Event, EventKind};
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use teloxide::types::ChatId;

pub async fn listen(app: Arc<App>, max_backoff: Duration) {
    let first = Duration::from_secs(1).min(max_backoff);
    let mut backoff = first;
    let mut last: Option<u64> = None;
    loop {
        match app.client.events(last).await {
            Ok(resp) if resp.status().is_success() => {
                backoff = first;
                app.connected.store(true, Ordering::Release);
                tracing::info!("event stream connected");
                let mut parser = Parser::default();
                let mut body = resp.bytes_stream();
                while let Some(Ok(chunk)) = body.next().await {
                    for frame in parser.push(&chunk) {
                        if frame.id.is_some() {
                            last = frame.id;
                        }
                        if let Ok(event) = serde_json::from_str::<Event>(&frame.data) {
                            on_event(&app, event).await;
                        }
                    }
                }
                app.connected.store(false, Ordering::Release);
                tracing::info!("event stream ended; reconnecting");
            }
            Ok(resp) => tracing::warn!(status = %resp.status(), "event stream refused"),
            Err(e) => tracing::warn!(error = %e, "event stream unreachable"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn on_event(app: &App, event: Event) {
    match event.kind {
        EventKind::Progress { step } => {
            // Sessions are chat ids (`build_order`).
            if let Ok(chat) = event.session_id.parse::<i64>() {
                app.status.progress(&app.bot, ChatId(chat), &step).await;
            }
        }
        // Only answers this adapter is waiting for: a replay or another
        // order's blocking reply is not one.
        EventKind::Response { response } => {
            if let Some(chat) = waiting(app, &event.client_msg_id) {
                deliver(app, chat, *response).await;
            }
        }
        EventKind::Error { error_code } => {
            if let Some(chat) = waiting(app, &event.client_msg_id) {
                fail(app, chat, error_code).await;
            }
        }
        EventKind::Done | EventKind::Unknown => {}
    }
}

fn waiting(app: &App, client_msg_id: &Option<String>) -> Option<ChatId> {
    client_msg_id.as_deref().and_then(|k| app.pending.take(k))
}
