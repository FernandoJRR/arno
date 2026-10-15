//! Home Server Harness — OCR MCP backend: image attachment → text.
//!
//! Contract C server side (SPEC §4.2). Tesseract runs on CPU with near-zero
//! VRAM cost, the right default for a GPU already saturated by the tool-
//! calling model (STACK.md §3). `MCP_OCR_ENGINE=ollama-vision` is a
//! documented future variant behind the same `OcrEngine` trait; not built.

use anyhow::Context;
use rmcp::{
    ServiceExt,
    transport::stdio,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::path::PathBuf;
use std::sync::Arc;

use mcp_ocr::ocr::{OcrServer, TesseractCli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Knobs (SPEC §5.7): MCP_OCR_TRANSPORT=http|stdio, MCP_OCR_BIND,
    // MCP_OCR_ATTACH_DIR, MCP_OCR_LANG, MCP_OCR_ENGINE, MCP_OCR_PREPROCESS.
    let engine = std::env::var("MCP_OCR_ENGINE").unwrap_or_else(|_| "tesseract".into());
    if engine != "tesseract" {
        anyhow::bail!(
            "MCP_OCR_ENGINE={engine:?} invalid — only \"tesseract\" is built today \
             (ollama-vision is a documented future variant, STACK.md §3)"
        );
    }
    let attach_dir = PathBuf::from(
        std::env::var("MCP_OCR_ATTACH_DIR").unwrap_or_else(|_| "/attachments".into()),
    );
    // Bills may be Spanish as often as English — default covers both.
    let lang = std::env::var("MCP_OCR_LANG").unwrap_or_else(|_| "eng+spa".into());
    let preprocess = match std::env::var("MCP_OCR_PREPROCESS").as_deref() {
        Err(_) | Ok("on") => true,
        Ok("off") => false,
        Ok(other) => anyhow::bail!("MCP_OCR_PREPROCESS={other:?} invalid — expected on|off"),
    };
    // Cleaned images are private per-call temp files, never the shared volume.
    let scratch_dir = std::env::temp_dir();

    let transport = std::env::var("MCP_OCR_TRANSPORT").unwrap_or_else(|_| "http".into());
    match transport.as_str() {
        "stdio" => {
            let server = OcrServer::new(
                Arc::new(TesseractCli),
                attach_dir,
                lang,
                scratch_dir,
                preprocess,
            );
            let running = server.serve(stdio()).await?;
            running.waiting().await?;
            Ok(())
        }
        "http" => {
            let bind: std::net::SocketAddr = std::env::var("MCP_OCR_BIND")
                .unwrap_or_else(|_| "0.0.0.0:9002".into())
                .parse()
                .context("MCP_OCR_BIND is not a valid socket address")?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(%bind, endpoint = "/mcp", "mcp-ocr serving");
            // Same rationale as mcp-linux: the compose network is the trust
            // boundary (no ports published to the host), so rmcp's loopback
            // Host allowlist is redundant on top of it and safe to disable.
            let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
            let attach_dir_for_service = attach_dir.clone();
            let lang_for_service = lang.clone();
            let service = StreamableHttpService::new(
                move || {
                    Ok(OcrServer::new(
                        Arc::new(TesseractCli),
                        attach_dir_for_service.clone(),
                        lang_for_service.clone(),
                        scratch_dir.clone(),
                        preprocess,
                    ))
                },
                Arc::new(LocalSessionManager::default()),
                config,
            );
            let router = axum::Router::new().nest_service("/mcp", service);
            axum::serve(listener, router).await?;
            Ok(())
        }
        other => Err(anyhow::anyhow!(
            "MCP_OCR_TRANSPORT={other:?} invalid — expected http|stdio"
        )),
    }
}
