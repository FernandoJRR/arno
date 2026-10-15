//! Read-only OCR (SPEC §4.2): one tool, `extract_text`, that turns an
//! attachment handle into recognized text.
//!
//! Safety posture: the handle is validated against a fixed charset before
//! ever touching the filesystem (path-traversal guard) — the harness mints
//! handles itself, but a compromised or buggy caller must not be able to walk
//! `attach_dir` with `../`. Recognition execs the fixed `tesseract` binary
//! directly (no shell, arguments passed as argv).

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::preprocess;
use crate::reading::Reading;

/// Words Tesseract is less sure of than this are shown as `[?word]`.
pub const DOUBT_CONF: f32 = 60.0;

/// Exec seam so the tool is testable without a real Tesseract install.
#[async_trait::async_trait]
pub trait OcrEngine: Send + Sync {
    async fn recognize(&self, image_path: &Path, lang: &str, psm: u8) -> Result<Reading, String>;
}

/// Direct exec of the fixed tesseract binary — argv only, never a shell.
/// TSV output carries per-word confidence. One thread per process: reads
/// run in parallel and would otherwise oversubscribe the CPU.
pub struct TesseractCli;

#[async_trait::async_trait]
impl OcrEngine for TesseractCli {
    async fn recognize(&self, image_path: &Path, lang: &str, psm: u8) -> Result<Reading, String> {
        let out = tokio::process::Command::new("tesseract")
            .arg(image_path)
            .arg("stdout")
            .arg("-l")
            .arg(lang)
            .arg("--psm")
            .arg(psm.to_string())
            // Not the `tsv` config file: tessdata_best ships without configs/.
            .arg("-c")
            .arg("tessedit_create_tsv=1")
            .env("OMP_THREAD_LIMIT", "1")
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.starts_with("level\t") {
            return Err("tesseract did not produce TSV output".into());
        }
        Ok(Reading::from_tsv(&stdout))
    }
}

/// One Tesseract pass: which image and which page-segmentation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    pub processed: bool,
    pub psm: u8,
}

impl Variant {
    pub fn label(&self) -> String {
        let src = if self.processed {
            "processed"
        } else {
            "original"
        };
        format!("{src}/psm{}", self.psm)
    }
}

/// Measured on a real bill photo (SPEC §11 #19): the cleaned image wins on
/// most fields, the original as-is still wins on some faint digits, and
/// psm 6 (single block) beats psm 3 on some table rows.
const VARIANTS: [Variant; 3] = [
    Variant {
        processed: false,
        psm: 3,
    },
    Variant {
        processed: true,
        psm: 3,
    },
    Variant {
        processed: true,
        psm: 6,
    },
];

/// Runs every applicable variant in parallel. The cleaned image lives in a
/// private temp file (0600, never the shared attachments volume) that is
/// removed when this returns. If cleanup fails, only the original is read.
pub async fn read_variants(
    engine: &Arc<dyn OcrEngine>,
    image: &Path,
    lang: &str,
    scratch_dir: &Path,
    preprocess_on: bool,
) -> Vec<(Variant, Result<Reading, String>)> {
    let processed = if preprocess_on {
        write_processed(image, scratch_dir).await
    } else {
        None
    };
    let mut set = tokio::task::JoinSet::new();
    for (i, v) in VARIANTS.iter().enumerate() {
        let path = match (&processed, v.processed) {
            (_, false) => image.to_owned(),
            (Some(tmp), true) => tmp.path().to_owned(),
            (None, true) => continue,
        };
        let (engine, lang, v) = (engine.clone(), lang.to_owned(), *v);
        set.spawn(async move { (i, v, engine.recognize(&path, &lang, v.psm).await) });
    }
    let mut results = set.join_all().await;
    results.sort_by_key(|(i, _, _)| *i);
    drop(processed);
    results.into_iter().map(|(_, v, r)| (v, r)).collect()
}

async fn write_processed(image: &Path, scratch_dir: &Path) -> Option<tempfile::NamedTempFile> {
    let (image, scratch_dir) = (image.to_owned(), scratch_dir.to_owned());
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let bytes = std::fs::read(&image).map_err(|e| e.to_string())?;
        let img = preprocess::prepare(&bytes)?;
        let tmp = tempfile::Builder::new()
            .prefix("ocr-")
            .suffix(".png")
            .tempfile_in(&scratch_dir)
            .map_err(|e| e.to_string())?;
        img.save_with_format(tmp.path(), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(tmp)
    })
    .await;
    match result {
        Ok(Ok(tmp)) => Some(tmp),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "preprocessing failed; reading the original only");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "preprocessing task failed; reading the original only");
            None
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct OcrArgs {
    /// The attachment handle from the conversation (SPEC §4.1 attachment
    /// manifest), not a filesystem path — it is resolved against this
    /// server's own scratch directory.
    #[schemars(description = "the attachment handle from the conversation")]
    pub image_ref: String,
    // No per-call language: small models filled it with "eng" on Spanish
    // documents and degraded recognition. `MCP_OCR_LANG` alone decides.
}

#[derive(Clone)]
pub struct OcrServer {
    engine: Arc<dyn OcrEngine>,
    attach_dir: PathBuf,
    lang: String,
    scratch_dir: PathBuf,
    preprocess: bool,
    tool_router: ToolRouter<Self>,
}

impl OcrServer {
    pub fn new(
        engine: Arc<dyn OcrEngine>,
        attach_dir: PathBuf,
        lang: String,
        scratch_dir: PathBuf,
        preprocess: bool,
    ) -> Self {
        Self {
            engine,
            attach_dir,
            lang,
            scratch_dir,
            preprocess,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl OcrServer {
    #[tool(
        description = "Extract text (OCR) from an attached image. Pass the attachment handle as image_ref. Words written as [?word] were read with low confidence and may be wrong."
    )]
    async fn extract_text(&self, Parameters(args): Parameters<OcrArgs>) -> CallToolResult {
        match self.extract_text_inner(&args).await {
            Ok(v) => CallToolResult::success(vec![rmcp::model::ContentBlock::json(v).expect("ok")]),
            Err(e) => error_result(e),
        }
    }
}

impl OcrServer {
    async fn extract_text_inner(&self, args: &OcrArgs) -> Result<serde_json::Value, String> {
        let path = resolve_handle(&self.attach_dir, &args.image_ref)?;
        let results = read_variants(
            &self.engine,
            &path,
            &self.lang,
            &self.scratch_dir,
            self.preprocess,
        )
        .await;
        let mut first_err = None;
        let mut best: Option<(Variant, Reading)> = None;
        for (v, r) in results {
            match r {
                Ok(r) => {
                    let better = best.as_ref().is_none_or(|(_, b)| {
                        (r.score(), r.mean_conf()) > (b.score(), b.mean_conf())
                    });
                    if better {
                        best = Some((v, r));
                    }
                }
                Err(e) => {
                    tracing::warn!(variant = %v.label(), error = %e, "ocr read failed");
                    first_err.get_or_insert(e);
                }
            }
        }
        let Some((variant, reading)) = best else {
            return Err(format!("ocr failed: {}", first_err.unwrap_or_default()));
        };
        let (trimmed, dropped_lines) = reading.trimmed(DOUBT_CONF);
        // Strong misreads are corrected in place; all are reported in
        // `checks`, which the harness relays to the user (SPEC §4.2, §4.4).
        let (text, checks, corrections) = crate::checks::correct(&trimmed.to_text(DOUBT_CONF));
        tracing::info!(variant = %variant.label(), score = reading.score(), "ocr read selected");
        Ok(json!({
            "text": text,
            "chars": text.chars().count(),
            "confidence": reading.mean_conf().round(),
            "doubtful_words": trimmed.doubtful(DOUBT_CONF),
            "dropped_lines": dropped_lines,
            "checks": checks,
            "corrections": corrections,
            "variant": variant.label(),
        }))
    }
}

/// Handles are minted by the harness as `[A-Za-z0-9_-]+` — reject anything
/// else outright rather than let a `/` or `..` reach `Path::join`.
fn resolve_handle(attach_dir: &Path, handle: &str) -> Result<PathBuf, String> {
    if handle.is_empty()
        || !handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("invalid attachment handle: {handle:?}"));
    }
    Ok(attach_dir.join(handle))
}

fn error_result(problem: String) -> CallToolResult {
    // Environment/backend problems surface as tool errors the model can read
    // and relay; they are application-level failures and never retried
    // (SPEC §8).
    CallToolResult::error(vec![
        rmcp::model::ContentBlock::json(json!({ "error": problem })).expect("serializes"),
    ])
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OcrServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Read-only OCR backend: image attachment → text")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reading::Word;

    fn reading(words: &[(&str, f32)]) -> Reading {
        Reading {
            words: words
                .iter()
                .map(|(t, c)| Word {
                    text: (*t).into(),
                    conf: *c,
                    line: (1, 1, 1),
                })
                .collect(),
        }
    }

    /// Returns a different reading per variant: the processed image read
    /// with psm 6 is the most confident one.
    struct FakeEngine;

    #[async_trait::async_trait]
    impl OcrEngine for FakeEngine {
        async fn recognize(&self, path: &Path, _lang: &str, psm: u8) -> Result<Reading, String> {
            let processed = path.extension().is_some_and(|e| e == "png");
            Ok(match (processed, psm) {
                (false, _) => reading(&[("Total", 90.0), ("a3046", 40.0)]),
                (true, 3) => reading(&[("Total", 90.0), ("Q", 80.0), ("30.46", 55.0)]),
                _ => {
                    // Edge debris on its own line: trimmed from the output.
                    let mut r = reading(&[("Total", 95.0), ("Q", 92.0), ("30.46", 70.0)]);
                    r.words.push(Word {
                        text: "Seriio".into(),
                        conf: 20.0,
                        line: (1, 1, 2),
                    });
                    r
                }
            })
        }
    }

    struct FailingEngine;

    #[async_trait::async_trait]
    impl OcrEngine for FailingEngine {
        async fn recognize(&self, _path: &Path, _lang: &str, _psm: u8) -> Result<Reading, String> {
            Err("no such file".into())
        }
    }

    struct Dirs {
        attach: PathBuf,
        scratch: PathBuf,
    }

    fn dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("mcp-ocr-{tag}-{}", std::process::id()));
        let d = Dirs {
            attach: root.join("attach"),
            scratch: root.join("scratch"),
        };
        std::fs::create_dir_all(&d.attach).unwrap();
        std::fs::create_dir_all(&d.scratch).unwrap();
        d
    }

    fn png_bytes() -> Vec<u8> {
        let img = image::GrayImage::from_fn(60, 40, |_, y| {
            image::Luma([if y % 8 < 2 { 0 } else { 255 }])
        });
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn server(engine: Arc<dyn OcrEngine>, d: &Dirs, preprocess: bool) -> OcrServer {
        OcrServer::new(
            engine,
            d.attach.clone(),
            "eng".into(),
            d.scratch.clone(),
            preprocess,
        )
    }

    fn args(image_ref: &str) -> OcrArgs {
        OcrArgs {
            image_ref: image_ref.into(),
        }
    }

    #[test]
    fn resolve_handle_accepts_charset_safe_handles() {
        let dir = PathBuf::from("/attachments");
        let p = resolve_handle(&dir, "att_ab12-CD").unwrap();
        assert_eq!(p, dir.join("att_ab12-CD"));
    }

    #[test]
    fn resolve_handle_rejects_traversal_and_separators() {
        let dir = PathBuf::from("/attachments");
        assert!(resolve_handle(&dir, "../x").is_err());
        assert!(resolve_handle(&dir, "a/b").is_err());
        assert!(resolve_handle(&dir, "").is_err());
        assert!(resolve_handle(&dir, "..").is_err());
    }

    #[tokio::test]
    async fn picks_the_most_confident_variant_and_cleans_up_scratch() {
        let d = dirs("pick");
        std::fs::write(d.attach.join("att_x"), png_bytes()).unwrap();
        let result = server(Arc::new(FakeEngine), &d, true)
            .extract_text_inner(&args("att_x"))
            .await
            .unwrap();
        assert_eq!(result["variant"], "processed/psm6");
        assert_eq!(result["text"], "Total Q 30.46");
        assert_eq!(result["doubtful_words"], 0);
        assert_eq!(result["dropped_lines"], 1);
        assert_eq!(result["checks"], serde_json::json!([]));
        assert_eq!(result["corrections"], 0);
        assert_eq!(
            std::fs::read_dir(&d.scratch).unwrap().count(),
            0,
            "temp file removed"
        );
    }

    #[tokio::test]
    async fn unreadable_image_falls_back_to_the_original_with_doubt_marks() {
        let d = dirs("fallback");
        std::fs::write(d.attach.join("att_x"), b"not an image").unwrap();
        let result = server(Arc::new(FakeEngine), &d, true)
            .extract_text_inner(&args("att_x"))
            .await
            .unwrap();
        assert_eq!(result["variant"], "original/psm3");
        assert_eq!(result["text"], "Total [?a3046]");
        assert_eq!(result["doubtful_words"], 1);
    }

    #[tokio::test]
    async fn preprocess_off_reads_only_the_original() {
        let d = dirs("off");
        std::fs::write(d.attach.join("att_x"), png_bytes()).unwrap();
        let result = server(Arc::new(FakeEngine), &d, false)
            .extract_text_inner(&args("att_x"))
            .await
            .unwrap();
        assert_eq!(result["variant"], "original/psm3");
    }

    #[tokio::test]
    async fn extract_text_rejects_traversal_handle_without_touching_engine() {
        let d = dirs("traversal");
        let err = server(Arc::new(FailingEngine), &d, true)
            .extract_text_inner(&args("../etc/passwd"))
            .await
            .unwrap_err();
        assert!(err.contains("invalid attachment handle"), "{err}");
    }

    #[tokio::test]
    async fn extract_text_surfaces_engine_failure() {
        let d = dirs("failing");
        std::fs::write(d.attach.join("att_x"), png_bytes()).unwrap();
        let err = server(Arc::new(FailingEngine), &d, true)
            .extract_text_inner(&args("att_x"))
            .await
            .unwrap_err();
        assert!(err.contains("ocr failed: no such file"), "{err}");
    }
}
