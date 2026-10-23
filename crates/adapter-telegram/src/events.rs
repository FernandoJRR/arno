//! Follows the harness's `GET /v1/events` stream and turns progress steps
//! into the chat's status message (SPEC §11 #32). Reconnects with backoff,
//! resuming after the last event id it saw.

use crate::harness_client::HarnessClient;
use crate::sse::Parser;
use crate::status::Status;
use contract::{Event, EventKind};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;

pub async fn listen(
    bot: Bot,
    client: Arc<HarnessClient>,
    status: Arc<Status>,
    max_backoff: Duration,
) {
    let first = Duration::from_secs(1).min(max_backoff);
    let mut backoff = first;
    let mut last: Option<u64> = None;
    loop {
        match client.events(last).await {
            Ok(resp) if resp.status().is_success() => {
                backoff = first;
                tracing::info!("event stream connected");
                let mut parser = Parser::default();
                let mut body = resp.bytes_stream();
                while let Some(Ok(chunk)) = body.next().await {
                    for frame in parser.push(&chunk) {
                        if frame.id.is_some() {
                            last = frame.id;
                        }
                        if let Ok(event) = serde_json::from_str::<Event>(&frame.data) {
                            on_event(&bot, &status, event).await;
                        }
                    }
                }
                tracing::info!("event stream ended; reconnecting");
            }
            Ok(resp) => tracing::warn!(status = %resp.status(), "event stream refused"),
            Err(e) => tracing::warn!(error = %e, "event stream unreachable"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn on_event(bot: &Bot, status: &Status, event: Event) {
    let EventKind::Progress { step } = &event.kind else {
        return;
    };
    // Sessions are chat ids (`build_order`).
    if let Ok(chat) = event.session_id.parse::<i64>() {
        status.progress(bot, ChatId(chat), step).await;
    }
}
