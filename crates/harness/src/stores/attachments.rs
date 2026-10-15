//! Ephemeral scratch store for inbound image attachments (SPEC §4.1).
//!
//! Bytes never reach the model directly: the harness persists them to a
//! shared scratch volume under a minted handle, injects a generic manifest
//! into the model's context (`orchestrator::mod`), and a downstream tool
//! (e.g. `ocr.extract_text`) resolves handle → file on its own side of the
//! same volume. This module knows nothing about OCR or any other consumer —
//! backend-agnostic per AGENTS.md #1. Files are scratch, not durable state
//! (AGENTS.md #7): minted per order, deleted after it, purged wholesale at
//! boot.

use contract::{Attachment, Order};
use rand::TryRng;
use rand::rngs::SysRng;
use std::path::Path;

/// Image mimes the harness will persist. Bills/screenshots/photos, no more —
/// widening this list is a deliberate decision, not a drive-by addition.
const ALLOWED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// One persisted attachment: the minted handle a tool receives, plus the mime
/// so a future manifest/tool could branch on type without re-deriving it.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentHandle {
    pub handle: String,
    pub mime: String,
}

/// Persists every inline, allowed-mime, in-budget attachment on `order` to
/// `dir/<handle>` (0600) and returns the minted handles. `ref`-form
/// attachments (future `POST /v1/files`) and anything over `cap` or outside
/// `ALLOWED_IMAGE_MIMES` are silently skipped — `orchestrator::validate_attachments`
/// already rejected an oversize inline attachment at the API boundary, so the
/// size check here is defense in depth, not the primary gate.
pub fn persist(order: &Order, dir: &Path, cap: usize) -> Vec<AttachmentHandle> {
    if order.attachments.is_empty() {
        return Vec::new();
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::error!(error = %e, dir = %dir.display(), "attachment scratch dir create failed");
        return Vec::new();
    }
    order
        .attachments
        .iter()
        .filter_map(|att| persist_one(att, dir, cap))
        .collect()
}

fn persist_one(att: &Attachment, dir: &Path, cap: usize) -> Option<AttachmentHandle> {
    if !ALLOWED_IMAGE_MIMES.contains(&att.mime.as_str()) {
        return None;
    }
    let bytes = att.decoded_bytes().ok().flatten()?; // Err or ref-form: skip
    if bytes.len() > cap {
        return None;
    }
    let handle = mint_handle();
    let path = dir.join(&handle);
    if let Err(e) = write_private(&path, &bytes) {
        tracing::error!(error = %e, path = %path.display(), "attachment scratch write failed");
        return None;
    }
    Some(AttachmentHandle {
        handle,
        mime: att.mime.clone(),
    })
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Best-effort delete of this order's handles — called on every return path
/// (success, budget-exceeded, model/backend error) so a failed order never
/// leaks a scratch file. Missing files are not an error (already cleaned up,
/// or never written).
pub fn cleanup(dir: &Path, handles: &[AttachmentHandle]) {
    for h in handles {
        let path = dir.join(&h.handle);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, path = %path.display(), "attachment cleanup failed");
        }
    }
}

/// Clears the entire scratch dir at boot: any file surviving a prior process
/// exit is stale by construction (handles live for one order only) rather
/// than durable state worth recovering (AGENTS.md #7).
pub fn purge_all(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(error = %e, dir = %dir.display(), "attachment scratch purge failed");
            return;
        }
    };
    for entry in entries.flatten() {
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::warn!(error = %e, path = %entry.path().display(), "attachment scratch purge failed");
        }
    }
}

/// Handle charset is deliberately restricted to `[A-Za-z0-9_-]` (the
/// consuming tool's traversal guard, e.g. `mcp-ocr`, enforces the same set) —
/// hex output already satisfies it.
fn mint_handle() -> String {
    let mut bytes = [0_u8; 16];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("OS CSPRNG unavailable");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("att_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "arno-attachments-test-{tag}-{}-{n}",
            std::process::id()
        ))
    }

    fn order_with(attachments: Vec<Attachment>) -> Order {
        Order {
            session_id: "s".into(),
            text: "".into(),
            client_msg_id: None,
            confirmation_token: None,
            attachments,
            decision_answer: None,
        }
    }

    fn inline(mime: &str, b64: &str) -> Attachment {
        Attachment {
            mime: mime.into(),
            data_b64: Some(b64.into()),
            attachment_ref: None,
        }
    }

    #[test]
    fn persists_allowed_mime_with_private_perms_and_safe_handle() {
        let dir = temp_dir("ok");
        let order = order_with(vec![inline("image/png", "aGVsbG8=")]); // "hello"
        let handles = persist(&order, &dir, 1024);
        assert_eq!(handles.len(), 1);
        let h = &handles[0];
        assert_eq!(h.mime, "image/png");
        assert!(
            h.handle
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "handle {} is not charset-safe",
            h.handle
        );
        let path = dir.join(&h.handle);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "expected 0600, got {mode:o}");
        }
    }

    #[test]
    fn skips_disallowed_mime_and_oversize_and_ref_form() {
        let dir = temp_dir("skip");
        let order = order_with(vec![
            inline("application/pdf", "aGVsbG8="), // not an image mime
            inline("image/png", "aGVsbG8="),       // 5 bytes, over cap below
            Attachment {
                mime: "image/png".into(),
                data_b64: None,
                attachment_ref: Some("h1".into()),
            }, // ref-form: future upload endpoint
        ]);
        let handles = persist(&order, &dir, 2); // cap smaller than "hello"
        assert!(handles.is_empty(), "{handles:?}");
    }

    #[test]
    fn cleanup_removes_persisted_files_and_tolerates_already_gone() {
        let dir = temp_dir("cleanup");
        let order = order_with(vec![inline("image/webp", "aGVsbG8=")]);
        let handles = persist(&order, &dir, 1024);
        assert_eq!(handles.len(), 1);
        let path = dir.join(&handles[0].handle);
        assert!(path.exists());
        cleanup(&dir, &handles);
        assert!(!path.exists());
        cleanup(&dir, &handles); // second call: already gone, must not panic/log as error
    }

    #[test]
    fn purge_all_clears_stale_files_and_tolerates_missing_dir() {
        let dir = temp_dir("purge");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("att_stale1"), b"x").unwrap();
        std::fs::write(dir.join("att_stale2"), b"y").unwrap();
        purge_all(&dir);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        purge_all(&temp_dir("never-created")); // must not panic
    }

    #[test]
    fn empty_attachments_never_creates_the_dir() {
        let dir = temp_dir("no-touch");
        let handles = persist(&order_with(vec![]), &dir, 1024);
        assert!(handles.is_empty());
        assert!(!dir.exists(), "no attachments: dir must not be created");
    }
}
