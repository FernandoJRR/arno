//! M0 throwaway CLI adapter (SPEC §9): proves the interface contract is real
//! from day one — the cheapest guarantee that Telegram never welds to the core.

use clap::Parser;
use contract::{ErrorBody, Order, Response};
use std::io::BufRead;

#[derive(Parser)]
struct Args {
    /// Harness API base URL.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    url: String,
    #[arg(long, default_value = "default")]
    session: String,
    /// Client token as registered in HARNESS_API_CLIENT_TOKENS.
    #[arg(long, env = "HARNESS_API_TOKEN")]
    token: String,
    /// Must exceed ORDER_BUDGET_S (SPEC §5.7); default 240 > 180.
    #[arg(long, default_value_t = 240)]
    timeout_secs: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(args.timeout_secs))
        .build()?;

    // Per-process nonce, not just the loop index: the harness dedups on
    // (client_id, client_msg_id) for DEDUP_WINDOW_MIN, and a bare
    // enumerate() index restarts at 0 every launch.
    let boot_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let stdin = std::io::stdin();
    for (n, line) in stdin.lock().lines().enumerate() {
        let text = line?;
        if text.trim().is_empty() || text == ":quit" {
            if text == ":quit" {
                break;
            }
            continue;
        }
        let order = Order {
            session_id: args.session.clone(),
            text,
            client_msg_id: Some(format!("{boot_id}-{n}")),
            confirmation_token: None,
            attachments: Vec::new(),
        };

        let resp = http
            .post(format!("{}/v1/orders", args.url.trim_end_matches('/')))
            .bearer_auth(&args.token)
            .json(&order)
            .send()
            .await?;

        match resp.status().is_success() {
            true => render(resp.json::<Response>().await?),
            false => {
                let code = resp.json::<ErrorBody>().await?.error_code;
                println!("[error] {}", code.as_str());
            }
        }
    }
    Ok(())
}

fn render(r: Response) {
    // Confirmation (SPEC §4.3 revised) is harness-interpreted from plain
    // text now — `r.text` alone already carries the instruction.
    println!("{}", r.text);
}
