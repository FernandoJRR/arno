//! Shared hash-chain primitives for the append-only JSONL stores
//! (`audit.rs`, `transcript.rs` — SPEC §12.1, §12.2).
//!
//! Both stores are hash-chained JSONL by design (§11 decisions #12/#13) and
//! duplicated their genesis/hash/hex/replay logic byte-for-byte; this module
//! is the single implementation of exactly that shared machinery. Entry
//! *shapes* stay per-store (transcript.rs's doc records the deliberate
//! non-abstraction of its `Event` enum vs. audit's `Outcome`) — only the
//! chain mechanics live here.
//!
//! Rotation support (SPEC §11 decision #15): segments are named
//! `<stem>-<UTC %Y%m%dT%H%M%S>.jsonl` beside the active file. A new active
//! file's first entry chains onto the previous active file's last hash via a
//! `rotation` handoff object; replay accepts genesis (fresh file) or a
//! handoff verified against the newest sibling's tail — any mismatch is
//! corrupt, fail-closed, same posture as an edited line.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 64 zero hex chars — the chain anchor for a fresh file's first entry.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Errors from the shared chain machinery. Callers map these onto their own
/// store-specific error enums with `From`.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("chain open failed: {0}")]
    Open(std::io::Error),
    #[error("chain corrupt at line {line}: {problem}")]
    Corrupt { line: usize, problem: String },
}

/// Where a chain resume begins: the last `(seq, hash)` seen in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHead {
    pub seq: u64,
    pub hash: String,
}

/// Rotation handoff recorded in the first entry of a segment that continues
/// a previous file's chain (SPEC §11 decision #15).
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct RotationHandoff {
    pub from_file: String,
    pub prev_chain_head: String,
}

pub fn genesis() -> String {
    GENESIS_HASH.to_owned()
}

/// Hashes exactly the entry's own JSON (without a `hash` field yet) chained
/// onto `prev_hash` — the same value verifiers recompute in [`replay_chain`].
pub fn hash_entry(prev_hash: &str, entry_without_hash: &serde_json::Value) -> String {
    let body = serde_json::to_string(entry_without_hash).expect("entry always serializes");
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(body.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads every line of one JSONL segment, verifying each one's `hash` covers
/// `prev_hash` + its own content, and that its `prev_hash` matches the
/// previous line's `hash` (or genesis for line 1). Returns the final
/// [`ChainHead`] to resume from. An empty/absent handoff-aware caller treats
/// the first entry's `rotation` object per [`verify_handoff`] — this fn only
/// validates intra-file linkage.
pub fn replay(path: &Path) -> Result<ChainHead, ChainError> {
    replay_chain(&[path.to_owned()])
}

/// Replays multiple segments (in the given order) as ONE logical chain —
/// used by rotation-aware open and by `verify` (SPEC §11 decision #15). Each
/// segment must either start at genesis (fresh) or carry a valid handoff
/// onto the previous segment's tail.
pub fn replay_chain(paths: &[PathBuf]) -> Result<ChainHead, ChainError> {
    let mut seq = 0_u64;
    let mut expected_prev = genesis();
    for path in paths {
        let file = File::open(path).map_err(ChainError::Open)?;
        for (i, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line_no = i + 1;
            let line = line.map_err(ChainError::Open)?;
            if line.trim().is_empty() {
                continue;
            }
            let mut value: serde_json::Value =
                serde_json::from_str(&line).map_err(|e| ChainError::Corrupt {
                    line: line_no,
                    problem: format!("invalid JSON: {e}"),
                })?;
            let stored_hash = value
                .get("hash")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ChainError::Corrupt {
                    line: line_no,
                    problem: "missing hash field".into(),
                })?
                .to_owned();
            let stored_prev = value
                .get("prev_hash")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ChainError::Corrupt {
                    line: line_no,
                    problem: "missing prev_hash field".into(),
                })?
                .to_owned();

            let handoff: Option<RotationHandoff> = value
                .get("rotation")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()
                .map_err(|e| ChainError::Corrupt {
                    line: line_no,
                    problem: format!("invalid rotation handoff: {e}"),
                })?;
            if line_no == 1 {
                // First line of a segment chains either onto the previous
                // segment's tail via a handoff (rotation continuation) or
                // genesis (fresh file). Anything else is a broken seam.
                let ok = match &handoff {
                    Some(h) => h.prev_chain_head == expected_prev && stored_prev == expected_prev,
                    None => stored_prev == expected_prev,
                };
                if !ok {
                    return Err(ChainError::Corrupt {
                        line: line_no,
                        problem: "prev_hash does not match preceding entry's hash — chain broken"
                            .into(),
                    });
                }
            }

            let obj = value.as_object_mut().ok_or_else(|| ChainError::Corrupt {
                line: line_no,
                problem: "entry is not a JSON object".into(),
            })?;
            obj.remove("hash");
            let recomputed = hash_entry(&stored_prev, &value);
            if recomputed != stored_hash {
                return Err(ChainError::Corrupt {
                    line: line_no,
                    problem: "hash does not match entry content — tampered or truncated".into(),
                });
            }
            let this_seq = value
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ChainError::Corrupt {
                    line: line_no,
                    problem: "missing seq field".into(),
                })?;
            seq = this_seq;
            expected_prev = stored_hash;
        }
    }
    Ok(ChainHead {
        seq,
        hash: expected_prev,
    })
}

/// Finds the active file's rotated siblings: `<stem>-<14-digit UTC stamp>.jsonl`
/// in the same directory, oldest first by filename (the stamp sorts
/// chronologically). Used by rotation-aware open and verify.
pub fn segments_for(active: &Path) -> Vec<PathBuf> {
    let stem = active
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let mut segments: Vec<PathBuf> = fs::read_dir(
        active
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    )
    .into_iter()
    .flatten()
    .flatten()
    .map(|e| e.path())
    .filter(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let ext_ok = p.extension().and_then(|e| e.to_str()) == Some("jsonl");
        // <stem>-<YYYYMMDDTHHMMSS>.jsonl — stamp is exactly 15 chars.
        ext_ok
            && name.len() == stem.len() + 1 + 15 + ".jsonl".len()
            && name.starts_with(stem)
            && name.as_bytes().get(stem.len()) == Some(&b'-')
    })
    .collect();
    segments.sort();
    segments
}

/// Rotates the active file at boot if it exceeds `max_bytes` (0 = never).
/// Renames it to `<stem>-<UTC stamp>.jsonl` beside itself and prunes the
/// oldest segments beyond `keep`. Returns the head of the rotated file (to
/// chain the new active file's first entry onto) when rotation happened.
pub fn maybe_rotate(
    active: &Path,
    max_bytes: u64,
    keep: usize,
) -> Result<Option<ChainHead>, ChainError> {
    if max_bytes == 0 {
        return Ok(None);
    }
    let len = match fs::metadata(active) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ChainError::Open(e)),
    };
    if len <= max_bytes {
        return Ok(None);
    }
    let head = if len == 0 {
        ChainHead {
            seq: 0,
            hash: genesis(),
        }
    } else {
        replay(active)?
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let stamp = chrono::DateTime::from_timestamp(stamp as i64, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .format("%Y%m%dT%H%M%S");
    let stem = active
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let ext = active
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jsonl");
    let mut rotated = active.to_path_buf();
    rotated.set_file_name(format!("{stem}-{stamp}.{ext}"));
    if rotated.exists() {
        // Two rotations within one second (tests). Append a uniquifying tail.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        rotated.set_file_name(format!("{stem}-{stamp}{nanos:09}.{ext}"));
    }
    fs::rename(active, &rotated).map_err(ChainError::Open)?;
    prune_segments(active, keep)?;
    tracing::info!(
        rotated = %rotated.display(),
        bytes = len,
        "rotated chain log segment (SPEC §11 decision #15)"
    );
    Ok(Some(head))
}

/// Deletes the oldest rotated segments beyond `keep` (never the active file).
pub fn prune_segments(active: &Path, keep: usize) -> Result<(), ChainError> {
    let segments = segments_for(active);
    for old in segments.iter().take(segments.len().saturating_sub(keep)) {
        fs::remove_file(old).map_err(ChainError::Open)?;
        tracing::info!(pruned = %old.display(), "pruned old chain log segment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "arno-chains-test-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Writes one self-consistent chained entry; returns its hash.
    fn append_entry(file: &mut File, seq: u64, prev_hash: &str, marker: &str) -> String {
        let mut entry = serde_json::json!({
            "seq": seq,
            "marker": marker,
            "prev_hash": prev_hash,
        });
        let hash = hash_entry(prev_hash, &entry);
        entry["hash"] = serde_json::Value::String(hash.clone());
        writeln!(file, "{entry}").unwrap();
        hash
    }

    #[test]
    fn replay_two_entries_resume_head() {
        let dir = tmp_dir("replay");
        let path = dir.join("x.jsonl");
        {
            let mut f = File::create(&path).unwrap();
            let h1 = append_entry(&mut f, 1, &genesis(), "a");
            append_entry(&mut f, 2, &h1, "b");
        }
        let head = replay(&path).unwrap();
        assert_eq!(head.seq, 2);
        assert_eq!(head.hash.len(), 64);
    }

    #[test]
    fn replay_chain_across_segments_with_handoff() {
        let dir = tmp_dir("handoff");
        let active = dir.join("x.jsonl");
        let seg1 = dir.join("x-20260906T120000.jsonl");
        let h1;
        {
            let mut f = File::create(&seg1).unwrap();
            let h = append_entry(&mut f, 1, &genesis(), "a");
            h1 = append_entry(&mut f, 2, &h, "b");
        }
        {
            let mut f = File::create(&active).unwrap();
            let mut entry = serde_json::json!({
                "seq": 3,
                "marker": "c",
                "prev_hash": h1,
                "rotation": {
                    "from_file": "x-20260906T120000.jsonl",
                    "prev_chain_head": h1,
                },
            });
            let hash = hash_entry(&h1, &entry);
            entry["hash"] = serde_json::Value::String(hash);
            writeln!(f, "{entry}").unwrap();
        }
        let head = replay_chain(&[seg1, active]).unwrap();
        assert_eq!(head.seq, 3);
    }

    #[test]
    fn segments_for_lists_oldest_first_and_ignores_other_files() {
        let dir = tmp_dir("segments");
        let active = dir.join("audit.jsonl");
        File::create(&active).unwrap();
        for name in [
            "audit-20260906T100000.jsonl",
            "audit-20260906T110000.jsonl",
            "transcript-20260906T110000.jsonl", // other store — ignored
            "audit.jsonl",                      // active has no stamp — ignored
        ] {
            File::create(dir.join(name)).unwrap();
        }
        let segs = segments_for(&active);
        let names: Vec<String> = segs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["audit-20260906T100000.jsonl", "audit-20260906T110000.jsonl"]
        );
    }

    #[test]
    fn file_over_threshold_rotates_and_new_file_chains_onto_old_tail() {
        let dir = tmp_dir("rotate");
        let active = dir.join("x.jsonl");
        {
            let mut f = File::create(&active).unwrap();
            let h = append_entry(&mut f, 1, &genesis(), "a");
            append_entry(&mut f, 2, &h, "b");
        }
        let old_len = fs::metadata(&active).unwrap().len();
        let head = maybe_rotate(&active, old_len - 1, 12).unwrap().unwrap();
        // Old file renamed away; active file now absent (caller recreates it).
        assert!(!active.exists());
        let segs = segments_for(&active);
        assert_eq!(segs.len(), 1);
        // The head is the rotated file's last hash.
        assert_eq!(head.seq, 2);
        let seg_head = replay(&segs[0]).unwrap();
        assert_eq!(head, seg_head);
    }

    #[test]
    fn segments_beyond_keep_are_pruned_oldest_first() {
        let dir = tmp_dir("prune");
        let active = dir.join("x.jsonl");
        File::create(&active).unwrap();
        for name in [
            "x-20260906T100000.jsonl",
            "x-20260906T110000.jsonl",
            "x-20260906T120000.jsonl",
        ] {
            File::create(dir.join(name)).unwrap();
        }
        prune_segments(&active, 2).unwrap();
        let segs = segments_for(&active);
        let names: Vec<String> = segs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["x-20260906T110000.jsonl", "x-20260906T120000.jsonl"]
        );
    }

    #[test]
    fn rotation_disabled_keeps_appending() {
        let dir = tmp_dir("off");
        let active = dir.join("x.jsonl");
        {
            let mut f = File::create(&active).unwrap();
            append_entry(&mut f, 1, &genesis(), "a");
        }
        assert!(maybe_rotate(&active, 0, 12).unwrap().is_none());
        assert!(active.exists());
        assert!(segments_for(&active).is_empty());
    }

    #[test]
    fn missing_file_is_a_no_op() {
        let dir = tmp_dir("missing");
        let active = dir.join("x.jsonl");
        assert!(maybe_rotate(&active, 1, 12).unwrap().is_none());
    }

    #[test]
    fn first_line_of_segment_must_chain_onto_expected_prev() {
        let dir = tmp_dir("seam");
        let seg_a = dir.join("x-20260906T100000.jsonl");
        let seg_b = dir.join("x.jsonl");
        {
            let mut f = File::create(&seg_a).unwrap();
            append_entry(&mut f, 1, &genesis(), "a");
        }
        {
            // Second segment without a handoff and with a prev that matches
            // neither the prior segment's tail nor genesis — corrupt seam.
            let mut f = File::create(&seg_b).unwrap();
            let mut entry = serde_json::json!({
                "seq": 2,
                "marker": "b",
                "prev_hash": "deadbeef".repeat(8),
            });
            let hash = hash_entry(&"deadbeef".repeat(8), &entry);
            entry["hash"] = serde_json::Value::String(hash);
            writeln!(f, "{entry}").unwrap();
        }
        let err = replay_chain(&[seg_a, seg_b]).unwrap_err();
        assert!(matches!(err, ChainError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn handoff_with_wrong_prev_chain_head_is_rejected() {
        let dir = tmp_dir("badhandoff");
        let seg_a = dir.join("x-20260906T100000.jsonl");
        let seg_b = dir.join("x.jsonl");
        let real_tail;
        {
            let mut f = File::create(&seg_a).unwrap();
            let h = append_entry(&mut f, 1, &genesis(), "a");
            real_tail = append_entry(&mut f, 2, &h, "b");
        }
        {
            // Handoff claims continuation from a hash that isn't the actual
            // tail of the previous segment — forged or corrupted seam.
            let fake = "abababab".repeat(8);
            let mut f = File::create(&seg_b).unwrap();
            let mut entry = serde_json::json!({
                "seq": 3,
                "marker": "c",
                "prev_hash": real_tail,
                "rotation": {
                    "from_file": "x-20260906T100000.jsonl",
                    "prev_chain_head": fake,
                },
            });
            let hash = hash_entry(&real_tail, &entry);
            entry["hash"] = serde_json::Value::String(hash);
            writeln!(f, "{entry}").unwrap();
        }
        let err = replay_chain(&[seg_a, seg_b]).unwrap_err();
        assert!(matches!(err, ChainError::Corrupt { .. }), "{err}");
    }
}
