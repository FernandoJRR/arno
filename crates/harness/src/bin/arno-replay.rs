//! Tool-selection replay against the live model (SPEC §11 #23).
//!
//! Runs each case's first completion with the full catalogue and/or through
//! the router, and reports the first tool picked, prompt tokens and latency.
//! Never dispatches a tool. Reads the same env as the harness, so run it
//! inside the harness container:
//!
//! `docker compose exec harness arno-replay [--mode full|routed|both] [--runs N] [--cases FILE] [--only NAME] [--show]`

use harness::config::Config;
use harness::model::ollama::OllamaProvider;
use harness::model::{
    ChatMessage, CompletionOutput, CompletionRequest, CompletionStats, ModelProvider, Role,
};
use harness::orchestrator::{ToolExecutor, attachment_manifest, attachment_tools, system_prompt};
use harness::plugins::{LoadSpec, PluginSet, router};
use harness::stores::attachments::AttachmentHandle;
use serde::Deserialize;
use std::time::Instant;

const DEFAULT_CASES: &str = include_str!("../../replay/cases.toml");

#[derive(Deserialize)]
struct Cases {
    case: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    text: String,
    attachment: Option<String>,
    expect: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Full,
    Routed,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Routed => "routed",
        }
    }
}

struct Args {
    modes: Vec<Mode>,
    runs: usize,
    cases: Option<String>,
    only: Option<String>,
    show: bool,
}

fn args() -> anyhow::Result<Args> {
    let mut out = Args {
        modes: vec![Mode::Full, Mode::Routed],
        runs: 3,
        cases: None,
        only: None,
        show: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{a} needs a value"))
        };
        match a.as_str() {
            "--mode" => {
                out.modes = match value()?.as_str() {
                    "full" => vec![Mode::Full],
                    "routed" => vec![Mode::Routed],
                    "both" => vec![Mode::Full, Mode::Routed],
                    other => anyhow::bail!("unknown mode {other:?}"),
                }
            }
            "--runs" => out.runs = value()?.parse()?,
            "--cases" => out.cases = Some(value()?),
            "--only" => out.only = Some(value()?),
            "--show" => out.show = true,
            "-h" | "--help" => {
                println!(
                    "arno-replay [--mode full|routed|both] [--runs N] [--cases FILE] [--only NAME] [--show]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok(out)
}

struct Outcome {
    picked: String,
    correct: bool,
    prompt_tokens: u64,
    router_ms: u64,
    total_ms: u64,
    group: Option<String>,
    /// The plain answer, when the model made no tool call.
    text: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let args = args()?;
    let cfg = Config::from_env().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let cases_text = match &args.cases {
        Some(path) => std::fs::read_to_string(path)?,
        None => DEFAULT_CASES.to_owned(),
    };
    let cases: Cases = toml::from_str(&cases_text)?;
    let provider = OllamaProvider::new(
        cfg.ollama_url.clone(),
        cfg.model.clone(),
        cfg.ollama_timeout,
        cfg.ollama_think,
        cfg.ollama_seed,
        cfg.ollama_top_k,
        cfg.ollama_num_predict,
    );
    let set = harness::plugins::load(&LoadSpec::from_config(&cfg))
        .await
        .map_err(|e| anyhow::anyhow!("plugins: {e}"))?;
    println!(
        "model={} tools={} groups={} runs={}",
        cfg.model,
        set.tool_schemas().len(),
        set.groups().len(),
        args.runs
    );
    println!(
        "{:<24} {:<7} {:<3} {:<40} {:>7} {:>7} {:>8}  group",
        "case", "mode", "ok", "first call", "prompt", "router", "total"
    );

    let mut totals: Vec<(Mode, Vec<Outcome>)> =
        args.modes.iter().map(|m| (*m, Vec::new())).collect();
    for case in cases
        .case
        .iter()
        .filter(|c| args.only.as_ref().is_none_or(|o| &c.name == o))
    {
        for (mode, outcomes) in &mut totals {
            for _ in 0..args.runs {
                let o = run(&provider, &set, &cfg, case, *mode).await;
                println!(
                    "{:<24} {:<7} {:<3} {:<40} {:>7} {:>6}ms {:>6}ms  {}",
                    case.name,
                    mode.label(),
                    if o.correct { "✓" } else { "✗" },
                    o.picked,
                    o.prompt_tokens,
                    o.router_ms,
                    o.total_ms,
                    o.group.as_deref().unwrap_or("-")
                );
                if args.show
                    && let Some(text) = &o.text
                {
                    println!(
                        "    └ {}",
                        text.chars()
                            .take(160)
                            .collect::<String>()
                            .replace('\n', " ")
                    );
                }
                outcomes.push(o);
            }
        }
    }

    println!("\nsummary");
    for (mode, outcomes) in &totals {
        let n = outcomes.len().max(1) as u64;
        let ok = outcomes.iter().filter(|o| o.correct).count();
        let avg = |f: fn(&Outcome) -> u64| outcomes.iter().map(f).sum::<u64>() / n;
        println!(
            "{:<7} accuracy {ok}/{} ({:.0}%)  avg prompt {} tok  avg router {} ms  avg total {} ms",
            mode.label(),
            outcomes.len(),
            100.0 * ok as f64 / n as f64,
            avg(|o| o.prompt_tokens),
            avg(|o| o.router_ms),
            avg(|o| o.total_ms),
        );
    }
    Ok(())
}

/// One first completion, mirroring the orchestrator's prompt assembly for a
/// fresh session (system prompt, user turn, attachment notice after it).
async fn run(
    provider: &OllamaProvider,
    set: &PluginSet,
    cfg: &Config,
    case: &Case,
    mode: Mode,
) -> Outcome {
    let started = Instant::now();
    let handles: Vec<AttachmentHandle> = case
        .attachment
        .iter()
        .map(|mime| AttachmentHandle {
            handle: "att_replay0001".into(),
            mime: mime.clone(),
        })
        .collect();
    let has_attachments = !handles.is_empty();

    let (pick, router_ms) = match mode {
        Mode::Full => (None, 0),
        Mode::Routed => {
            let r =
                router::pick(provider, set, &[], &case.text, has_attachments, cfg.num_ctx).await;
            let ms = r.latency.as_millis() as u64;
            (Some(r.pick), ms)
        }
    };
    let unrouted = || {
        let all = set.tool_schemas();
        if has_attachments {
            let narrowed = attachment_tools(set, all);
            if narrowed.is_empty() {
                all.to_vec()
            } else {
                narrowed
            }
        } else {
            all.to_vec()
        }
    };
    let tools = pick
        .as_ref()
        .and_then(|p| router::offered_tools(set, p, false))
        .unwrap_or_else(unrouted);

    let mut messages = vec![ChatMessage::new(Role::System, system_prompt())];
    if let Some(notes) = pick.as_ref().and_then(|p| router::how_notes(set, p, false)) {
        messages.push(ChatMessage::new(Role::System, notes));
    }
    messages.push(ChatMessage::new(Role::User, case.text.clone()));
    if let Some(m) = attachment_manifest(set, &handles) {
        messages.push(ChatMessage::new(Role::System, m));
    }
    let req = CompletionRequest {
        messages,
        tools,
        context_tokens: cfg.num_ctx,
        format: None,
    };
    let (picked, text, stats) = match provider.complete_with_stats(req).await {
        Ok((CompletionOutput::ToolCalls(calls), stats)) => (calls[0].name.clone(), None, stats),
        Ok((CompletionOutput::Final(t), stats)) => ("none".to_owned(), Some(t), stats),
        Err(e) => (format!("error: {e}"), None, None),
    };
    let stats = stats.unwrap_or(CompletionStats::default());
    Outcome {
        correct: case.expect.contains(&picked),
        picked,
        prompt_tokens: stats.prompt_tokens,
        router_ms,
        total_ms: started.elapsed().as_millis() as u64,
        group: pick.map(|p| p.label().to_owned()),
        text,
    }
}
