//! Append-only, hash-chained audit log (AC-4.4).
//!
//! The entry format and chain math are identical to the on-prem
//! build's audit component (`crates/goose/src/onprem.rs`): each line
//! is one JSON object `{ts, event, session_id, details, prev_hash,
//! entry_hash}` where `entry_hash = sha256("{prev_hash}|{ts}|{event}|
//! {session_id}|{details_json}")` and the chain starts at `GENESIS`.
//! A log written by either build verifies under the other's rules.
//!
//! The code is not shared with onprem on purpose: the on-prem build
//! compiles the harness crate out entirely (feature layout, AC-7),
//! so onprem cannot link this module. The shared contract is the
//! format, pinned here and by the format test vectors below.
//! Rotation and checkpoint files are on-prem operational concerns and
//! are not replicated at personal-build scale.

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const GENESIS_HASH: &str = "GENESIS";

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn utc_now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The chain hash of one entry, in the exact on-prem encoding.
pub fn entry_hash(
    prev_hash: &str,
    ts: &str,
    event: &str,
    session_id: &str,
    details_json: &str,
) -> String {
    sha256_hex(&format!(
        "{prev_hash}|{ts}|{event}|{session_id}|{details_json}"
    ))
}

/// Recompute an entry's hash from its fields. Legacy `model_request`
/// entries (no `event` field, written by early on-prem builds) keep
/// their original hash scheme, matching onprem's verifier.
pub fn recompute_entry_hash(entry: &Value) -> Result<String> {
    let get = |field: &str| {
        entry
            .get(field)
            .and_then(|v| v.as_str())
            .with_context(|| format!("audit entry missing field '{field}'"))
            .map(str::to_string)
    };
    let prev_hash = get("prev_hash")?;
    let ts = get("ts")?;

    if entry.get("event").is_none() {
        let session_id = get("session_id")?;
        let model = get("model")?;
        let endpoint = get("endpoint")?;
        let request_sha256 = get("request_sha256")?;
        return Ok(sha256_hex(&format!(
            "{prev_hash}|{ts}|{session_id}|{model}|{endpoint}|{request_sha256}"
        )));
    }

    let event = get("event")?;
    let session_id = get("session_id")?;
    let details_json = serde_json::to_string(entry.get("details").unwrap_or(&Value::Null))?;
    Ok(entry_hash(
        &prev_hash,
        &ts,
        &event,
        &session_id,
        &details_json,
    ))
}

fn lock_path_for(log_path: &Path) -> PathBuf {
    let mut name = log_path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    log_path.with_file_name(name)
}

/// Spin-lock via lock-file creation, the same approach the on-prem
/// writer uses; the guard removes the file on drop.
struct WriteGuard(PathBuf);

impl WriteGuard {
    fn acquire(log_path: &Path) -> Result<Self> {
        let lock = lock_path_for(log_path);
        for _ in 0..200 {
            match OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_file) => return Ok(Self(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e).context("acquiring audit write lock"),
            }
        }
        anyhow::bail!("timed out acquiring audit write lock {}", lock.display())
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn read_entries(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line)
            .with_context(|| format!("audit line {} is not valid JSON", idx + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Append one entry to the chained log at `path`, creating it if
/// needed. Best-effort callers may ignore the result; the gate logs
/// failures rather than failing user work on an audit write.
pub fn append(path: &Path, event: &str, session_id: &str, details: Value) -> Result<Value> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _guard = WriteGuard::acquire(path)?;
    let entries = read_entries(path)?;
    let prev_hash = entries
        .last()
        .and_then(|e| e.get("entry_hash"))
        .and_then(|v| v.as_str())
        .unwrap_or(GENESIS_HASH)
        .to_string();
    let ts = utc_now_ts();
    let details_json = serde_json::to_string(&details)?;
    let hash = entry_hash(&prev_hash, &ts, event, session_id, &details_json);
    let entry = serde_json::json!({
        "ts": ts,
        "event": event,
        "session_id": session_id,
        "details": details,
        "prev_hash": prev_hash,
        "entry_hash": hash,
    });
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(&entry)?)?;
    Ok(entry)
}

#[derive(Debug, Default, PartialEq)]
pub struct VerifyReport {
    pub entries: usize,
    pub non_monotonic_timestamps: usize,
}

/// Verify the whole chain at `path`: linkage, entry hashes, and
/// (reported, not fatal) timestamp monotonicity. Errors name the
/// first broken entry.
pub fn verify(path: &Path) -> Result<VerifyReport> {
    let entries = read_entries(path)?;
    let mut report = VerifyReport {
        entries: entries.len(),
        ..Default::default()
    };
    let mut expected_prev = GENESIS_HASH.to_string();
    let mut last_ts: Option<String> = None;
    for (idx, entry) in entries.iter().enumerate() {
        let prev = entry
            .get("prev_hash")
            .and_then(|v| v.as_str())
            .with_context(|| format!("entry {} missing prev_hash", idx + 1))?;
        if prev != expected_prev {
            anyhow::bail!(
                "entry {} breaks the chain: prev_hash {prev} != expected {expected_prev}",
                idx + 1
            );
        }
        let recomputed = recompute_entry_hash(entry)
            .with_context(|| format!("entry {} cannot be rehashed", idx + 1))?;
        let stored = entry
            .get("entry_hash")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if recomputed != stored {
            anyhow::bail!(
                "entry {} hash mismatch: stored {stored}, recomputed {recomputed}",
                idx + 1
            );
        }
        let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(prev_ts) = &last_ts {
            if ts < prev_ts.as_str() {
                report.non_monotonic_timestamps += 1;
            }
        }
        last_ts = Some(ts.to_string());
        expected_prev = stored.to_string();
    }
    Ok(report)
}

/// The most recent entries, oldest first (for `audit list`).
pub fn tail(path: &Path, limit: usize) -> Result<Vec<Value>> {
    let entries = read_entries(path)?;
    let start = entries.len().saturating_sub(limit);
    Ok(entries[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wm-audit-test-{}-{name}.log", std::process::id()))
    }

    #[test]
    fn chain_verifies_and_tampering_breaks_it() {
        let path = temp_log("chain");
        let _ = std::fs::remove_file(&path);
        append(
            &path,
            "trigger_fire",
            "s1",
            serde_json::json!({"trigger": "ci"}),
        )
        .unwrap();
        append(
            &path,
            "approval_decided",
            "s1",
            serde_json::json!({"id": "x"}),
        )
        .unwrap();
        let report = verify(&path).unwrap();
        assert_eq!(report.entries, 2);

        // Tamper with the first entry's details: its hash no longer
        // recomputes, and verification names the entry.
        let mut entries = read_entries(&path).unwrap();
        entries[0]["details"]["trigger"] = serde_json::json!("forged");
        let forged = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, forged).unwrap();
        let error = verify(&path).unwrap_err().to_string();
        assert!(error.contains("entry 1"), "unexpected error: {error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hash_matches_onprem_formula() {
        // Test vector computed from the on-prem encoding:
        // sha256("{prev}|{ts}|{event}|{session}|{details_json}").
        let hash = entry_hash(
            "GENESIS",
            "2026-10-25T00:00:00Z",
            "tool_call",
            "sess",
            "{\"tool\":\"shell\",\"decision\":\"allow\"}",
        );
        assert_eq!(
            hash,
            sha256_hex("GENESIS|2026-10-25T00:00:00Z|tool_call|sess|{\"tool\":\"shell\",\"decision\":\"allow\"}")
        );
        // Legacy model_request entries recompute under their own scheme.
        let legacy = serde_json::json!({
            "ts": "2026-10-25T00:00:00Z",
            "session_id": "sess",
            "model": "m",
            "endpoint": "https://llm.internal/v1",
            "request_sha256": "abc",
            "prev_hash": "GENESIS",
            "entry_hash": "unused"
        });
        assert_eq!(
            recompute_entry_hash(&legacy).unwrap(),
            sha256_hex("GENESIS|2026-10-25T00:00:00Z|sess|m|https://llm.internal/v1|abc")
        );
    }
}
