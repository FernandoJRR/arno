//! Part A v1 interface adapter (SPEC §5.1, §9 M1): teloxide long-polling
//! bridge between Telegram and the Harness API. Depends only on `contract` —
//! no `harness` dependency (AGENTS.md workspace map).

mod bot;
mod config;
mod harness_client;
mod order;

use harness_client::HarnessClient;
use teloxide::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::from_env().map_err(|e| {
        // Fail loudly on config problems (AGENTS.md #6).
        anyhow::anyhow!("startup configuration invalid: {e}")
    })?;

    let http = reqwest::Client::builder()
        .timeout(cfg.http_timeout)
        .build()?;
    let client = HarnessClient::new(
        http,
        cfg.harness_api_url.clone(),
        cfg.harness_api_token.clone(),
    );
    let telegram_bot_token = cfg.telegram_bot_token.clone();
    let bot = Bot::new(telegram_bot_token);

    tracing::info!("adapter-telegram starting long-poll loop");
    bot::run(bot, cfg, client).await
}
