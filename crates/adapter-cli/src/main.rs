//! M0 throwaway CLI adapter (SPEC §9): proves the interface contract is real
//! from day one — the cheapest guarantee that Telegram never welds to the core.

use clap::Parser;
use contract::{Decision, DecisionAnswer, ErrorBody, Order, Response};
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

    // The decision shown last; the next line answers it (SPEC §4.3).
    let mut pending: Option<Decision> = None;
    let stdin = std::io::stdin();
    for (n, line) in stdin.lock().lines().enumerate() {
        let text = line?;
        if text.trim().is_empty() || text == ":quit" {
            if text == ":quit" {
                break;
            }
            continue;
        }
        let decision_answer = pending.take().and_then(|d| answer_for(&d, &text));
        let order = Order {
            session_id: args.session.clone(),
            text: if decision_answer.is_some() {
                String::new()
            } else {
                text
            },
            client_msg_id: Some(format!("{boot_id}-{n}")),
            confirmation_token: None,
            attachments: Vec::new(),
            decision_answer,
        };

        let resp = http
            .post(format!("{}/v1/orders", args.url.trim_end_matches('/')))
            .bearer_auth(&args.token)
            .json(&order)
            .send()
            .await?;

        match resp.status().is_success() {
            true => pending = render(resp.json::<Response>().await?),
            false => {
                let code = resp.json::<ErrorBody>().await?.error_code;
                println!("[error] {}", code.as_str());
            }
        }
    }
    Ok(())
}

/// Prints the reply; a decision becomes a numbered menu and is returned so
/// the next input line can answer it.
fn render(r: Response) -> Option<Decision> {
    let Some(d) = r.decision else {
        println!("{}", r.text);
        return None;
    };
    println!("{}", d.prompt);
    for detail in &d.details {
        println!("  {}: {}", detail.label, detail.value);
    }
    for (i, opt) in d.options.iter().enumerate() {
        println!("  [{}] {}", i + 1, opt.title);
    }
    if d.free_text {
        println!("  (or type something else)");
    }
    Some(d)
}

/// A number picks an option; other text is a free-text answer when allowed,
/// otherwise an ordinary order (which supersedes the decision).
fn answer_for(d: &Decision, line: &str) -> Option<DecisionAnswer> {
    let (value, text) = match line.trim().parse::<usize>() {
        Ok(i) if (1..=d.options.len()).contains(&i) => (Some(d.options[i - 1].value.clone()), None),
        _ if d.free_text => (None, Some(line.to_owned())),
        _ => return None,
    };
    Some(DecisionAnswer {
        id: d.id.clone(),
        value,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::{DecisionKind, DecisionOption, OptionStyle};

    fn decision(free_text: bool) -> Decision {
        let opt = |v: &str| DecisionOption {
            value: v.into(),
            title: v.into(),
            style: OptionStyle::Default,
        };
        Decision {
            id: "dcs_a".into(),
            kind: DecisionKind::Confirm,
            prompt: "Run?".into(),
            details: Vec::new(),
            options: vec![opt("approve"), opt("reject")],
            free_text,
            expires_at: String::new(),
        }
    }

    #[test]
    fn numbers_pick_options_and_text_is_free_text_only_when_allowed() {
        let d = decision(true);
        assert_eq!(
            answer_for(&d, "1").unwrap().value.as_deref(),
            Some("approve")
        );
        assert_eq!(
            answer_for(&d, " 2 ").unwrap().value.as_deref(),
            Some("reject")
        );
        let free = answer_for(&d, "make it 50").unwrap();
        assert_eq!(
            (free.value, free.text.as_deref()),
            (None, Some("make it 50"))
        );
        assert_eq!(answer_for(&d, "3").unwrap().text.as_deref(), Some("3"));
        assert!(answer_for(&decision(false), "hello").is_none());
    }
}
