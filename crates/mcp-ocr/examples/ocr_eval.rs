//! Dev-only: runs every OCR variant on one image and reports which expected
//! strings each reading contains, plain or doubt-marked (SPEC §11 #19).
//! `cargo run -p mcp-ocr --example ocr_eval -- <image> [expected…]`
//! Uses the local `tesseract`; point `TESSDATA_PREFIX` at the models to test.

use mcp_ocr::ocr::{DOUBT_CONF, OcrEngine, TesseractCli, read_variants};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (image, expected) = args
        .split_first()
        .expect("usage: ocr_eval <image> [expected…]");
    let lang = std::env::var("MCP_OCR_LANG").unwrap_or_else(|_| "eng+spa".into());
    let engine: Arc<dyn OcrEngine> = Arc::new(TesseractCli);
    let started = std::time::Instant::now();
    let results = read_variants(&engine, image.as_ref(), &lang, &std::env::temp_dir(), true).await;
    println!("all variants: {:?}\n", started.elapsed());
    for (variant, reading) in results {
        let reading = reading.expect("tesseract failed");
        let full = reading.to_text(DOUBT_CONF);
        let (trimmed, dropped) = reading.trimmed(DOUBT_CONF);
        let (text, checks, corrections) = mcp_ocr::checks::correct(&trimmed.to_text(DOUBT_CONF));
        println!(
            "=== {}  score {:.1}  mean {:.0}  doubtful {}  lines {} -> {} (dropped {dropped})  chars {} -> {}",
            variant.label(),
            reading.score(),
            reading.mean_conf(),
            trimmed.doubtful(DOUBT_CONF),
            full.lines().count(),
            text.lines().count(),
            full.chars().count(),
            text.chars().count(),
        );
        for e in expected {
            let mark = if text.contains(e.as_str()) {
                "ok"
            } else if text.contains(&format!("[?{e}"))
                || trimmed.words.iter().any(|w| w.text.contains(e.as_str()))
            {
                "doubt"
            } else {
                "MISS"
            };
            println!("  {mark:5} {e}");
        }
        println!("  corrections: {corrections}");
        for c in checks {
            println!("  check: {c}");
        }
        println!("{text}\n");
    }
}
