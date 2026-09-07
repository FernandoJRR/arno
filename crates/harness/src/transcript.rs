//! Persistent, append-only, hash-chained transcript of conversation and
//! tool-call events — every user message, assistant final answer, tool call
//! request/result, and confirmation freeze/redeem (SPEC §12, third AGENTS.md
//! #7 exception).
//!
//! Deliberately **not** the same file as [`crate::audit::AuditLog`]: that
//! module's narrow, specifically-justified scope is "what write actually
//! executed" (a financial ledger); this is the broader, noisier general
//! narrative of what happened in a conversation, including reads and chat
//! text. Mixing them would blur the audit log's clean, auditor-facing scope.
//! The two modules are structurally similar (both hash-chained JSONL, both
//! `Mutex<Inner>`-guarded single writers) but intentionally duplicated rather
//! than abstracted — different entry shapes, two call sites.
//!
//! Same hash-chaining mechanics as `audit.rs`: each line's `hash` covers its
//! own content plus the previous line's hash, so editing, deleting, or
//! reordering any historical line breaks the chain from that point forward.
//! A corrupt file aborts boot rather than silently starting fresh, same
//! posture as the audit log — accepted knowingly even though this file sees
//! far higher write volume (every tool call, not just confirmed writes), so
//! an unclean shutdown mid-write is a more likely event here.
//!
//! Contains real finance data by design (tool args/results verbatim) — unlike
//! `tracing::` output, which per AGENTS.md must never carry tool payloads.
//! File permissions are narrowed to the owner on Unix.

use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::chains::{self, ChainError, ChainHead};

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("transcript log open failed: {0}")]
    Open(std::io::Error),
    #[error("transcript log write failed: {0}")]
    Write(std::io::Error),
    #[error("transcript log corrupt at line {line}: {problem}")]
    Corrupt { line: usize, problem: String },
}

/// What actually happened to a redeemed frozen dispatch — mirrors
/// `audit::RecordOutcome`, kept as its own small type since this module is
/// deliberately independent of `audit.rs`.
pub enum Outcome<'a> {
    Ok(&'a serde_json::Value),
    Err(&'a str),
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SerOutcome<'a> {
    Ok { result: &'a serde_json::Value },
    Err { error_code: &'a str },
}

/// One thing worth keeping a durable record of. `#[serde(tag = "kind")]`
/// flattened into [`Entry`] gives flat, greppable/`jq`-able lines, e.g.
/// `{"kind":"tool_call","tool":"securo.list_transactions","args":{...}}`.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event<'a> {
    UserMessage {
        text: &'a str,
    },
    AssistantFinal {
        text: &'a str,
    },
    ToolCall {
        call_id: &'a str,
        tool: &'a str,
        args: &'a serde_json::Value,
    },
    ToolResult {
        call_id: &'a str,
        tool: &'a str,
        result: &'a serde_json::Value,
    },
    /// A write was proposed (frozen), confirmed or not — `audit.rs` only
    /// records a *redeemed* write, so this is the only durable record that a
    /// write was ever proposed at all.
    ConfirmationRequested {
        tool: &'a str,
        args: &'a serde_json::Value,
    },
    ConfirmationRedeemed {
        tool: &'a str,
        args: &'a serde_json::Value,
        outcome: SerOutcome<'a>,
    },
    /// A pending action was explicitly rejected (SPEC §4.3 revised) rather
    /// than redeemed or left to expire — nothing executed.
    ConfirmationCancelled {
        tool: &'a str,
        args: &'a serde_json::Value,
    },
}

#[derive(Serialize)]
struct Entry<'a> {
    seq: u64,
    ts_unix_ms: u128,
    client_id: &'a str,
    session_id: &'a str,
    #[serde(flatten)]
    event: Event<'a>,
    prev_hash: &'a str,
    /// Set only on the first entry of a fresh active file that continues a
    /// rotated segment's chain (SPEC §11 decision #15).
    #[serde(skip_serializing_if = "Option::is_none")]
    rotation: Option<&'a chains::RotationHandoff>,
}

struct Inner {
    file: File,
    seq: u64,
    last_hash: String,
    /// Set at open when a rotation just happened: emitted on the first
    /// recorded entry, then cleared.
    next_entry_rotation: Option<chains::RotationHandoff>,
}

#[derive(Debug)]
pub struct TranscriptLog(Mutex<Inner>);

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl TranscriptLog {
    /// Opens (creating if absent) the transcript log at `path`. If the file
    /// already has entries, replays and verifies the entire hash chain first
    /// — any mismatch aborts with `TranscriptError::Corrupt` rather than
    /// silently starting over (same fail-loud precedent as `audit.rs` and
    /// "unreachable configured backend is a config error", `crate::mcp`).
    /// Rotation (SPEC §11 decision #15): fires at boot only — keeps the
    /// single-writer FIFO model untouched (AGENTS.md #5) and the seam
    /// deterministic. After a rotation the fresh active file's first entry
    /// carries a `rotation` handoff and chains onto the rotated file's tail
    /// hash.
    pub fn open(
        path: &Path,
        rotate_bytes: u64,
        keep_segments: usize,
    ) -> Result<Self, TranscriptError> {
        let rotated_head = chains::maybe_rotate(path, rotate_bytes, keep_segments)
            .map_err(TranscriptError::from)?;
        // Replay all rotated segments + the active file as ONE logical chain —
        // this is what validates the rotation seam (fail-closed on any
        // mismatch, SPEC §11 decision #15). With no file at all, start fresh.
        let mut walk = chains::segments_for(path);
        let exists = path.exists();
        if exists {
            walk.push(path.to_path_buf());
        }
        let (mut seq, mut last_hash) = if exists {
            chains::replay_chain(&walk)
                .map_err(TranscriptError::from)
                .map(|ChainHead { seq, hash }| (seq, hash))?
        } else {
            (0, genesis())
        };
        let rotation_handoff = rotated_head.as_ref().map(|head| chains::RotationHandoff {
            from_file: chains::segments_for(path)
                .pop()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default(),
            prev_chain_head: head.hash.clone(),
        });
        if let Some(head) = &rotated_head {
            // The fresh active file continues the previous segment's chain:
            // seq and last_hash pick up where the rotated file left off.
            seq = head.seq;
            last_hash = head.hash.clone();
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(TranscriptError::Open)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!(path = %path.display(), error = %e, "could not restrict transcript log permissions to 0600");
            }
        }
        Ok(Self(Mutex::new(Inner {
            file,
            seq,
            last_hash,
            next_entry_rotation: rotation_handoff,
        })))
    }

    /// Appends one event. A write failure here is the caller's problem to
    /// log loudly — it must never block or alter the response already
    /// decided; this is a record of what happened, not a gate on whether it
    /// may happen.
    pub fn record(
        &self,
        client_id: &str,
        session_id: &str,
        event: Event<'_>,
    ) -> Result<(), TranscriptError> {
        let mut inner = self.0.lock().expect("transcript lock");
        let seq = inner.seq + 1;
        let ts_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let rotation = inner.next_entry_rotation.take();
        let entry = Entry {
            seq,
            ts_unix_ms,
            client_id,
            session_id,
            event,
            prev_hash: &inner.last_hash,
            rotation: rotation.as_ref(),
        };
        let mut value = serde_json::to_value(&entry).expect("transcript entry always serializes");
        let hash = hash_entry(&inner.last_hash, &value);
        value["hash"] = serde_json::Value::String(hash.clone());
        let line = format!("{value}\n");

        inner
            .file
            .write_all(line.as_bytes())
            .map_err(TranscriptError::Write)?;
        inner.file.flush().map_err(TranscriptError::Write)?;
        inner.seq = seq;
        inner.last_hash = hash;
        Ok(())
    }
}

impl<'a> From<Outcome<'a>> for SerOutcome<'a> {
    fn from(o: Outcome<'a>) -> Self {
        match o {
            Outcome::Ok(result) => SerOutcome::Ok { result },
            Outcome::Err(error_code) => SerOutcome::Err { error_code },
        }
    }
}

fn genesis() -> String {
    chains::genesis()
}

fn hash_entry(prev_hash: &str, entry_without_hash: &serde_json::Value) -> String {
    chains::hash_entry(prev_hash, entry_without_hash)
}

impl From<ChainError> for TranscriptError {
    fn from(e: ChainError) -> Self {
        match e {
            ChainError::Open(io) => TranscriptError::Open(io),
            ChainError::Corrupt { line, problem } => TranscriptError::Corrupt { line, problem },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "arno-transcript-test-{tag}-{}-{}.jsonl",
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
        let log = TranscriptLog::open(&path, 0, 12).unwrap();
        log.record(
            "cli",
            "s1",
            Event::UserMessage {
                text: "show me my transactions",
            },
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(line["seq"], 1);
        assert_eq!(line["kind"], "user_message");
        assert_eq!(line["text"], "show me my transactions");
        assert_eq!(line["prev_hash"], genesis());
        assert_eq!(line["hash"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn second_entry_chains_onto_first() {
        let path = tmp_path("chain");
        let log = TranscriptLog::open(&path, 0, 12).unwrap();
        log.record(
            "cli",
            "s1",
            Event::ToolCall {
                call_id: "call_1",
                tool: "securo.list_transactions",
                args: &serde_json::json!({}),
            },
        )
        .unwrap();
        log.record(
            "cli",
            "s1",
            Event::ToolResult {
                call_id: "call_1",
                tool: "securo.list_transactions",
                result: &serde_json::json!({"total": 8}),
            },
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[1]["seq"], 2);
        assert_eq!(lines[1]["kind"], "tool_result");
        assert_eq!(lines[1]["result"]["total"], 8);
        assert_eq!(lines[1]["prev_hash"], lines[0]["hash"]);
    }

    #[test]
    fn confirmation_events_serialize_with_tagged_outcome() {
        let path = tmp_path("confirm");
        let log = TranscriptLog::open(&path, 0, 12).unwrap();
        log.record(
            "cli",
            "s1",
            Event::ConfirmationRequested {
                tool: "securo.propose_create_transaction",
                args: &serde_json::json!({"apply": true, "amount": 50}),
            },
        )
        .unwrap();
        log.record(
            "cli",
            "s1",
            Event::ConfirmationRedeemed {
                tool: "securo.propose_create_transaction",
                args: &serde_json::json!({"apply": true, "amount": 50}),
                outcome: Outcome::Ok(&serde_json::json!({"id": "tx_1"})).into(),
            },
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["kind"], "confirmation_requested");
        assert_eq!(lines[1]["kind"], "confirmation_redeemed");
        assert_eq!(lines[1]["outcome"]["status"], "ok");
        assert_eq!(lines[1]["outcome"]["result"]["id"], "tx_1");
    }

    #[test]
    fn reopening_a_valid_file_resumes_seq_and_hash() {
        let path = tmp_path("resume");
        {
            let log = TranscriptLog::open(&path, 0, 12).unwrap();
            log.record("cli", "s1", Event::AssistantFinal { text: "hi" })
                .unwrap();
        }
        let log2 = TranscriptLog::open(&path, 0, 12).unwrap();
        log2.record("cli", "s1", Event::AssistantFinal { text: "bye" })
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
            let log = TranscriptLog::open(&path, 0, 12).unwrap();
            log.record(
                "cli",
                "s1",
                Event::ToolCall {
                    call_id: "c1",
                    tool: "t",
                    args: &serde_json::json!({"amount": 50}),
                },
            )
            .unwrap();
        }
        // Flip a byte in the persisted amount — hash no longer matches.
        let tampered = std::fs::read_to_string(&path).unwrap().replace("50", "99");
        std::fs::write(&path, tampered).unwrap();

        let err = TranscriptLog::open(&path, 0, 12).unwrap_err();
        assert!(matches!(err, TranscriptError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn truncated_json_line_is_rejected() {
        let path = tmp_path("truncated");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{{not valid json").unwrap();
        drop(f);
        let err = TranscriptLog::open(&path, 0, 12).unwrap_err();
        assert!(matches!(err, TranscriptError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn missing_file_starts_fresh_at_genesis() {
        let path = tmp_path("missing");
        assert!(!path.exists());
        let log = TranscriptLog::open(&path, 0, 12).unwrap();
        log.record("cli", "s1", Event::UserMessage { text: "hi" })
            .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_are_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("perms");
        let _log = TranscriptLog::open(&path, 0, 12).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
