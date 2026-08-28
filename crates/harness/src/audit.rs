//! Persistent, append-only, hash-chained audit trail of executed frozen
//! actions (SPEC §4.3, §12.1, M3).
//!
//! `contract::StructuredItem`'s doc comment already calls itself "the seed of
//! the future audit log" — this module is that log. Deliberately **not**
//! under `stores/`: those stay in-memory per AGENTS.md #7, and this is one of
//! the two narrowly-scoped, spec-revised exceptions (the other is
//! [`crate::policy::ToolPolicy`]) — the harness's own record of what it did
//! to real financial data must survive a restart, or "before M3 runs against
//! real books" (SPEC §12 backlog #1) is never actually true.
//!
//! Each line's `hash` covers its own content plus the previous line's hash,
//! so editing, deleting, or reordering any historical line breaks the chain
//! from that point forward — detectable by replaying the file top to bottom.
//! This is why a corrupt file aborts boot (`open` returns `Err`) rather than
//! silently starting fresh: silently restarting the chain would erase the
//! very evidence tamper-evidence exists to preserve.
//!
//! Contains real finance data by design — unlike `tracing::` output, which
//! per AGENTS.md must never carry tool payloads. File permissions are
//! narrowed to the owner on Unix.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
// 64 zero hex chars, trimmed to exactly 64 below (const fmt can't repeat).

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit log open failed: {0}")]
    Open(std::io::Error),
    #[error("audit log write failed: {0}")]
    Write(std::io::Error),
    #[error("audit log corrupt at line {line}: {problem}")]
    Corrupt { line: usize, problem: String },
}

#[derive(Serialize)]
struct Entry<'a> {
    seq: u64,
    ts_unix_ms: u128,
    client_id: &'a str,
    session_id: &'a str,
    tool: &'a str,
    args: &'a serde_json::Value,
    outcome: Outcome<'a>,
    prev_hash: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome<'a> {
    Ok { result: &'a serde_json::Value },
    Err { error_code: &'a str },
}

/// What actually happened to a frozen dispatch, for [`AuditLog::record`].
pub enum RecordOutcome<'a> {
    Ok(&'a serde_json::Value),
    Err(&'a str),
}

struct Inner {
    file: File,
    seq: u64,
    last_hash: String,
}

#[derive(Debug)]
pub struct AuditLog(Mutex<Inner>);

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl AuditLog {
    /// Opens (creating if absent) the audit log at `path`. If the file
    /// already has entries, replays and verifies the entire hash chain first
    /// — any mismatch aborts with `AuditError::Corrupt` rather than silently
    /// starting over, mirroring the "unreachable configured backend is a
    /// config error" fail-loud precedent (`crate::mcp`).
    pub fn open(path: &Path) -> Result<Self, AuditError> {
        let (seq, last_hash) = if path.exists() {
            replay(path)?
        } else {
            (0, genesis())
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(AuditError::Open)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!(path = %path.display(), error = %e, "could not restrict audit log permissions to 0600");
            }
        }
        Ok(Self(Mutex::new(Inner {
            file,
            seq,
            last_hash,
        })))
    }

    /// Appends one entry for a dispatch that has *already happened* — success
    /// or failure, both are audit-worthy (SPEC §4.3: what was attempted,
    /// verbatim). A write failure here is the caller's problem to log loudly
    /// (it must never override the real execution outcome already decided).
    pub fn record(
        &self,
        client_id: &str,
        session_id: &str,
        tool: &str,
        args: &serde_json::Value,
        outcome: RecordOutcome<'_>,
    ) -> Result<(), AuditError> {
        let mut inner = self.0.lock().expect("audit lock");
        let seq = inner.seq + 1;
        let ts_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let entry = Entry {
            seq,
            ts_unix_ms,
            client_id,
            session_id,
            tool,
            args,
            outcome: match outcome {
                RecordOutcome::Ok(result) => Outcome::Ok { result },
                RecordOutcome::Err(error_code) => Outcome::Err { error_code },
            },
            prev_hash: &inner.last_hash,
        };
        let mut value = serde_json::to_value(&entry).expect("audit entry always serializes");
        let hash = hash_entry(&inner.last_hash, &value);
        value["hash"] = serde_json::Value::String(hash.clone());
        let line = format!("{value}\n");

        inner
            .file
            .write_all(line.as_bytes())
            .map_err(AuditError::Write)?;
        inner.file.flush().map_err(AuditError::Write)?;
        inner.seq = seq;
        inner.last_hash = hash;
        Ok(())
    }
}

fn genesis() -> String {
    GENESIS_HASH.to_owned()
}

/// Hashes exactly the entry's own JSON (without a `hash` field yet) chained
/// onto `prev_hash` — the same value verifiers recompute in `replay`.
fn hash_entry(prev_hash: &str, entry_without_hash: &serde_json::Value) -> String {
    let body = serde_json::to_string(entry_without_hash).expect("entry always serializes");
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(body.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads every line, verifying each one's `hash` covers `prev_hash` + its own
/// content, and that its `prev_hash` matches the previous line's `hash` (or
/// genesis for line 1). Returns the final `(seq, hash)` to resume from.
fn replay(path: &Path) -> Result<(u64, String), AuditError> {
    let file = File::open(path).map_err(AuditError::Open)?;
    let reader = BufReader::new(file);
    let mut seq = 0_u64;
    let mut expected_prev = genesis();
    for (i, line) in reader.lines().enumerate() {
        let line_no = i + 1;
        let line = line.map_err(AuditError::Open)?;
        if line.trim().is_empty() {
            continue;
        }
        let mut value: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| AuditError::Corrupt {
                line: line_no,
                problem: format!("invalid JSON: {e}"),
            })?;
        let stored_hash = value
            .get("hash")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AuditError::Corrupt {
                line: line_no,
                problem: "missing hash field".into(),
            })?
            .to_owned();
        let stored_prev = value
            .get("prev_hash")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AuditError::Corrupt {
                line: line_no,
                problem: "missing prev_hash field".into(),
            })?
            .to_owned();
        if stored_prev != expected_prev {
            return Err(AuditError::Corrupt {
                line: line_no,
                problem: "prev_hash does not match preceding entry's hash — chain broken".into(),
            });
        }
        let obj = value.as_object_mut().ok_or_else(|| AuditError::Corrupt {
            line: line_no,
            problem: "entry is not a JSON object".into(),
        })?;
        obj.remove("hash");
        let recomputed = hash_entry(&stored_prev, &value);
        if recomputed != stored_hash {
            return Err(AuditError::Corrupt {
                line: line_no,
                problem: "hash does not match entry content — tampered or truncated".into(),
            });
        }
        let this_seq = value
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| AuditError::Corrupt {
                line: line_no,
                problem: "missing seq field".into(),
            })?;
        seq = this_seq;
        expected_prev = stored_hash;
    }
    Ok((seq, expected_prev))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "arno-audit-test-{tag}-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn first_entry_chains_from_genesis_with_64_hex_hash() {
        let path = tmp_path("first");
        let log = AuditLog::open(&path).unwrap();
        log.record(
            "cli",
            "s1",
            "securo.propose_create_transaction",
            &serde_json::json!({"apply": true, "amount": 50}),
            RecordOutcome::Ok(&serde_json::json!({"id": "tx_1"})),
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(line["seq"], 1);
        assert_eq!(line["prev_hash"], genesis());
        assert_eq!(line["hash"].as_str().unwrap().len(), 64);
        assert_eq!(line["outcome"]["status"], "ok");
        assert_eq!(line["outcome"]["result"]["id"], "tx_1");
    }

    #[test]
    fn second_entry_chains_onto_first() {
        let path = tmp_path("chain");
        let log = AuditLog::open(&path).unwrap();
        log.record(
            "cli",
            "s1",
            "t1",
            &serde_json::json!({}),
            RecordOutcome::Ok(&serde_json::json!(null)),
        )
        .unwrap();
        log.record(
            "cli",
            "s1",
            "t2",
            &serde_json::json!({}),
            RecordOutcome::Err("backend_unavailable"),
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[1]["seq"], 2);
        assert_eq!(lines[1]["prev_hash"], lines[0]["hash"]);
        assert_eq!(lines[1]["outcome"]["status"], "err");
        assert_eq!(lines[1]["outcome"]["error_code"], "backend_unavailable");
    }

    #[test]
    fn reopening_a_valid_file_resumes_seq_and_hash() {
        let path = tmp_path("resume");
        {
            let log = AuditLog::open(&path).unwrap();
            log.record(
                "cli",
                "s1",
                "t1",
                &serde_json::json!({}),
                RecordOutcome::Ok(&serde_json::json!(null)),
            )
            .unwrap();
        }
        let log2 = AuditLog::open(&path).unwrap();
        log2.record(
            "cli",
            "s1",
            "t2",
            &serde_json::json!({}),
            RecordOutcome::Ok(&serde_json::json!(null)),
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["seq"], 2);
        assert_eq!(lines[1]["prev_hash"], lines[0]["hash"]);
    }

    #[test]
    fn tampered_line_is_rejected_on_reopen() {
        let path = tmp_path("tamper");
        {
            let log = AuditLog::open(&path).unwrap();
            log.record(
                "cli",
                "s1",
                "t1",
                &serde_json::json!({"amount": 50}),
                RecordOutcome::Ok(&serde_json::json!(null)),
            )
            .unwrap();
        }
        // Flip a byte in the persisted amount — hash no longer matches.
        let tampered = std::fs::read_to_string(&path).unwrap().replace("50", "99");
        std::fs::write(&path, tampered).unwrap();

        let err = AuditLog::open(&path).unwrap_err();
        assert!(matches!(err, AuditError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn truncated_json_line_is_rejected() {
        let path = tmp_path("truncated");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{{not valid json").unwrap();
        drop(f);
        let err = AuditLog::open(&path).unwrap_err();
        assert!(matches!(err, AuditError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn missing_file_starts_fresh_at_genesis() {
        let path = tmp_path("missing");
        assert!(!path.exists());
        let log = AuditLog::open(&path).unwrap();
        log.record(
            "cli",
            "s1",
            "t1",
            &serde_json::json!({}),
            RecordOutcome::Ok(&serde_json::json!(null)),
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_are_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("perms");
        let _log = AuditLog::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
