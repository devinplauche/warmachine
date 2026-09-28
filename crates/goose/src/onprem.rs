//! On-prem build: CUI/ITAR controls compiled into the binary.
//!
//! Only compiled with the `onprem` feature. Provides:
//! - Re-exports of the compile-time network allowlist
//!   ([`goose_providers::onprem`]).
//! - Payload sealing: session message content is encrypted at rest with
//!   AES-256-GCM (envelope encryption; the data-encryption key lives in the
//!   OS keychain, never on disk). Plaintext from databases created before
//!   sealing was introduced still reads, and is re-sealed on its next write.
//! - An append-only, hash-chained audit log of every model request plus
//!   session-lifecycle and extension-change events. Each entry records what
//!   was sent to the on-prem LLM (as a SHA-256 over the request payload,
//!   verifiable against the session database) chained to the previous
//!   entry's hash, so tampering with or deleting entries is detectable. This
//!   is the tamper-evident trail behind NIST 800-171 3.3.1 audit logging:
//!   the session database holds the content, this log proves the sequence.
//!   `verify_audit_log` re-checks the whole chain. The log rotates at 100 MB
//!   (`audit-<UTC>.log` archives, chain threaded across files), every 100th
//!   entry's hash is checkpointed to a sibling JSONL file for cross-checking,
//!   and the log directory and files are owner-only (0o700/0o600 on unix).
//!   If `WARMACHINE_ONPREM_AUDIT_SINK_URL` is baked in at compile time, a
//!   background forwarder ships new entries to that HTTPS SIEM endpoint
//!   (best-effort; the local log remains the source of truth). The
//!   forwarder's cursor is integrity-protected with a keychain-held key.

pub use goose_providers::onprem::{allowed_origins, check_url_allowed, primary_base_url};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

// The `onprem` feature implies `system-keyring` and `dep:aes-gcm` in
// Cargo.toml; this is a backstop so a future edit that drops the implication
// fails loudly instead of silently weakening the build.
#[cfg(not(feature = "system-keyring"))]
compile_error!(
    "the `onprem` build requires the `system-keyring` feature: \
     the session-encryption key is held in the OS keychain, never on disk"
);

// ---------------------------------------------------------------------------
// Payload sealing (encryption at rest)
// ---------------------------------------------------------------------------

const DEK_KEYRING_SERVICE: &str = "warmachine";
const DEK_KEYRING_ACCOUNT: &str = "session-dek";
const SEAL_ALG: &str = "aes-256-gcm";
const SEAL_VERSION: u64 = 1;

/// Load a 32-byte key from the OS keychain, generating and storing it on
/// first use. `what` names the key in error messages.
///
/// The key never touches disk: it lives in the platform keychain (Keychain /
/// Credential Manager / Secret Service) and only resides in process memory.
/// Keychain failure is fatal — failing closed is the point; a build that
/// cannot reach its key must not silently write plaintext instead. Every
/// failure is audited first: audit writes never touch the keychain, so the
/// hook cannot recurse.
#[cfg(feature = "system-keyring")]
fn keyring_key_or_create(service: &str, account: &str, what: &str) -> Result<[u8; 32]> {
    use base64::Engine as _;

    let entry = match keyring::Entry::new(service, account) {
        Ok(entry) => entry,
        Err(e) => {
            let err = anyhow::anyhow!("OS keychain unavailable for {what}: {e}");
            audit_keychain_failure("entry_new", &err);
            return Err(err);
        }
    };

    let decode = |encoded: &str| -> Result<[u8; 32]> {
        let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded.trim()) {
            Ok(bytes) => bytes,
            Err(e) => {
                let err = anyhow::anyhow!("stored {what} is not valid base64: {e}");
                audit_keychain_failure("decode", &err);
                return Err(err);
            }
        };
        match bytes.try_into() {
            Ok(key) => Ok(key),
            Err(_) => {
                let err = anyhow::anyhow!("stored {what} has wrong length");
                audit_keychain_failure("decode", &err);
                Err(err)
            }
        }
    };

    match entry.get_password() {
        Ok(encoded) => decode(&encoded),
        Err(keyring::Error::NoEntry) => {
            // First run on this machine: generate, store, then re-read, so a
            // concurrent first-run writer winning the race still leaves every
            // process using the same stored key.
            let mut key = [0u8; 32];
            if let Err(e) = rand::TryRng::try_fill_bytes(&mut rand::rngs::SysRng, &mut key) {
                let err = anyhow::anyhow!("OS RNG failure generating {what}: {e}");
                audit_keychain_failure("generate", &err);
                return Err(err);
            }
            if let Err(e) =
                entry.set_password(&base64::engine::general_purpose::STANDARD.encode(key))
            {
                let err = anyhow::anyhow!("cannot store {what}: {e}");
                audit_keychain_failure("store", &err);
                return Err(err);
            }
            let stored = match entry.get_password() {
                Ok(stored) => stored,
                Err(e) => {
                    let err = anyhow::anyhow!("cannot re-read {what}: {e}");
                    audit_keychain_failure("reread", &err);
                    return Err(err);
                }
            };
            decode(&stored)
        }
        Err(e) => {
            let err = anyhow::anyhow!("OS keychain unavailable for {what}: {e}");
            audit_keychain_failure("get_password", &err);
            Err(err)
        }
    }
}

/// Best-effort audit hook for keychain failures. Safe to call from key
/// loading itself: the audit log is a plain file, no keychain involved.
fn audit_keychain_failure(op: &str, err: &anyhow::Error) {
    if let Err(e) = audit_event(
        "keychain_failure",
        None,
        &serde_json::json!({"op": op, "error": format!("{err:#}")}),
    ) {
        tracing::warn!("audit log write failed: {e:#}");
    }
}

/// Load this install's data-encryption key from the OS keychain, generating
/// and storing it on first use.
#[cfg(feature = "system-keyring")]
fn session_dek() -> Result<[u8; 32]> {
    keyring_key_or_create(
        DEK_KEYRING_SERVICE,
        DEK_KEYRING_ACCOUNT,
        "session encryption key",
    )
}

/// Keychain-held key for the SIEM forward cursor's integrity tag.
///
/// Separate from the session DEK: it guards forwarding state, not content.
#[cfg(feature = "system-keyring")]
fn cursor_mac_key() -> Result<[u8; 32]> {
    keyring_key_or_create(
        DEK_KEYRING_SERVICE,
        CURSOR_MAC_KEYRING_ACCOUNT,
        "audit cursor key",
    )
}

/// Seal a serialized session payload for storage.
///
/// Returns a self-describing JSON envelope
/// `{"enc":"aes-256-gcm","v":1,"nonce":..,"ct":..}` (base64 fields), with a
/// fresh 96-bit nonce per message. AES-256-GCM gives confidentiality plus
/// integrity: tampered rows fail to open instead of decrypting to garbage.
pub fn seal_payload(plaintext_json: &str) -> Result<String> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
    use base64::Engine as _;

    let dek = session_dek()?;
    let mut nonce_bytes = [0u8; 12];
    rand::TryRng::try_fill_bytes(&mut rand::rngs::SysRng, &mut nonce_bytes)
        .map_err(|e| anyhow::anyhow!("OS RNG failure: {e}"))?;

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&dek));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext_json.as_bytes())
        .map_err(|e| anyhow::anyhow!("payload seal failed: {e}"))?;

    let engine = base64::engine::general_purpose::STANDARD;
    Ok(serde_json::json!({
        "enc": SEAL_ALG,
        "v": SEAL_VERSION,
        "nonce": engine.encode(nonce_bytes),
        "ct": engine.encode(ct),
    })
    .to_string())
}

/// Open a stored session payload.
///
/// Sealed envelopes are decrypted and integrity-checked; anything else is a
/// legacy plaintext row (databases written before payload sealing) and is
/// parsed as-is. Callers re-seal on write, so plaintext ages out of the
/// database through normal use.
pub fn open_payload<T: DeserializeOwned>(stored: &str) -> Result<T> {
    let value: serde_json::Value = serde_json::from_str(stored)?;
    if value.get("enc").and_then(|v| v.as_str()) == Some(SEAL_ALG) {
        decrypt_envelope(&value)
    } else {
        Ok(serde_json::from_value(value)?)
    }
}

fn decrypt_envelope<T: DeserializeOwned>(envelope: &serde_json::Value) -> Result<T> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
    use base64::Engine as _;

    let version = envelope.get("v").and_then(|v| v.as_u64()).unwrap_or(0);
    if version != SEAL_VERSION {
        anyhow::bail!("unsupported sealed payload version: {version}");
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let nonce_bytes = engine
        .decode(envelope.get("nonce").and_then(|v| v.as_str()).unwrap_or(""))
        .context("sealed payload has invalid nonce")?;
    if nonce_bytes.len() != 12 {
        anyhow::bail!("sealed payload has wrong nonce length");
    }
    let ct = engine
        .decode(envelope.get("ct").and_then(|v| v.as_str()).unwrap_or(""))
        .context("sealed payload has invalid ciphertext")?;

    let dek = session_dek()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&dek));
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ct.as_ref())
        .map_err(|_| anyhow::anyhow!("sealed payload failed integrity check"))?;
    let json = String::from_utf8(pt).context("sealed payload is not valid UTF-8")?;
    Ok(serde_json::from_str(&json)?)
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

const AUDIT_LOG_NAME: &str = "audit.log";
const GENESIS_HASH: &str = "GENESIS";

fn audit_log_path() -> Result<PathBuf> {
    Ok(crate::config::paths::Paths::config_dir().join(AUDIT_LOG_NAME))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex_bytes(&hasher.finalize())
}

/// Integrity tag for the SIEM forward cursor: SHA-256(key || cursor).
///
/// The `hmac` crate is only a transitive dependency and this build adds no
/// new dependencies, so the tag is a keyed SHA-256 rather than HMAC-SHA256.
/// The preimage is a fixed-format hex cursor value prefixed by a 256-bit
/// keychain-held key, which keeps the construction sound for this use: an
/// attacker who can rewrite the cursor file still cannot forge the tag.
fn cursor_tag(key: &[u8; 32], cursor: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update(cursor.as_bytes());
    hex_bytes(&hasher.finalize())
}

/// Rotate the audit log once it exceeds this size. Rotation preserves the
/// hash chain: the new file's first entry chains from the archived file's
/// last hash, and `verify_audit_log` replays archives in filename order.
const AUDIT_ROTATE_BYTES: u64 = 100 * 1024 * 1024;

/// Checkpoint the hash of every Nth entry to a sibling JSONL file so the
/// verifier can cross-check the chain without trusting it alone. The
/// SIEM-forwarded copy remains the primary external anchor; checkpoints are
/// local defense-in-depth.
const AUDIT_CHECKPOINT_EVERY: usize = 100;

/// Scan the log once, returning the last entry's hash and the entry count.
///
/// A missing file is an empty log, not an error: the first write creates it.
/// The count rides along with the existing predecessor-hash scan — one O(n)
/// pass, not two — and drives checkpointing in the write path.
fn log_tail(path: &Path) -> (String, usize) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (GENESIS_HASH.to_string(), 0),
    };
    let reader = BufReader::new(file);
    let mut last_hash = GENESIS_HASH.to_string();
    let mut count = 0usize;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(h) = entry.get("entry_hash").and_then(|h| h.as_str()) {
                last_hash = h.to_string();
            }
        }
    }
    (last_hash, count)
}

/// Create the audit log's directory with owner-only permissions (0o700 on
/// unix), mirroring the session database directory hardening.
fn ensure_audit_dir(log_path: &Path) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create audit log dir {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("cannot secure audit log dir {}", parent.display()))?;
        }
    }
    Ok(())
}

/// Exclusive cross-process guard for the audit-log critical section
/// (predecessor-hash read → rotation → append). Without it, two CLI processes
/// appending concurrently fork the hash chain and the second entry fails
/// verification. fs2 advisory locks release when the file closes, so a
/// crashed holder cannot wedge the log.
struct AuditWriteGuard {
    _file: std::fs::File,
}

fn acquire_audit_write_lock(log_path: &Path) -> Result<AuditWriteGuard> {
    use fs2::FileExt;

    let lock_path = log_path.with_file_name("audit.lock");
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&lock_path)
        .with_context(|| format!("cannot open audit lock {}", lock_path.display()))?;
    for _ in 0..50 {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(AuditWriteGuard { _file: file }),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot lock {}", lock_path.display()));
            }
        }
    }
    anyhow::bail!("timed out acquiring audit log lock {}", lock_path.display());
}

/// Rotated archives of `audit.log`, oldest first (the timestamped names sort
/// chronologically).
fn rotated_audit_logs(log_path: &Path) -> Result<Vec<PathBuf>> {
    let parent = log_path
        .parent()
        .with_context(|| format!("audit log path has no parent: {}", log_path.display()))?;
    let mut archives = Vec::new();
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("cannot list audit log dir {}", parent.display()))?;
        }
    };
    for entry in entries.map_while(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("audit-") && name.ends_with(".log") {
            archives.push(entry.path());
        }
    }
    archives.sort();
    Ok(archives)
}

/// Last entry hash of the newest rotated archive, if any. Continues the chain
/// when the live log is missing — e.g. a crash between rotation and the next
/// write — instead of restarting at GENESIS and forking the chain.
fn latest_archive_tail(log_path: &Path) -> Result<Option<String>> {
    let mut archives = rotated_audit_logs(log_path)?;
    Ok(archives.pop().map(|p| log_tail(&p).0))
}

/// Checkpoint file for a log file: `audit.log` → `audit.checkpoint`,
/// `audit-<ts>.log` → `audit-<ts>.checkpoint`. Rotating the log renames the
/// checkpoint alongside it, so each file carries its own checkpoints.
fn checkpoint_path_for_log(log_path: &Path) -> PathBuf {
    let stem = log_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(AUDIT_LOG_NAME);
    let stem = stem.strip_suffix(".log").unwrap_or(stem);
    log_path.with_file_name(format!("{stem}.checkpoint"))
}

/// Rotate `audit.log` to `audit-<UTC timestamp>.log` once it exceeds
/// [`AUDIT_ROTATE_BYTES`], carrying the checkpoint file along. Returns the
/// archived file's last entry hash when a rotation happened, so the new
/// file's first entry chains from it instead of GENESIS.
fn maybe_rotate_audit_log(path: &Path) -> Result<Option<String>> {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("cannot stat audit log {}", path.display()))?;
        }
    };
    if size <= AUDIT_ROTATE_BYTES {
        return Ok(None);
    }
    let (archived_tail, _) = log_tail(path);
    let parent = path
        .parent()
        .with_context(|| format!("audit log path has no parent: {}", path.display()))?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut archive = parent.join(format!("audit-{ts}.log"));
    let mut n = 1u32;
    while archive.exists() {
        n += 1;
        archive = parent.join(format!("audit-{ts}-{n}.log"));
    }
    std::fs::rename(path, &archive)
        .with_context(|| format!("cannot rotate audit log to {}", archive.display()))?;
    let cp = checkpoint_path_for_log(path);
    if cp.exists() {
        let cp_archive = checkpoint_path_for_log(&archive);
        std::fs::rename(&cp, &cp_archive).with_context(|| {
            format!("cannot rotate audit checkpoint to {}", cp_archive.display())
        })?;
    }
    Ok(Some(archived_tail))
}

/// Append one entry line with owner-only permissions (0o600 on unix).
fn write_entry_line(log_path: &Path, entry: &serde_json::Value) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(log_path)
        .with_context(|| format!("cannot open audit log {}", log_path.display()))?;
    writeln!(file, "{entry}")
        .with_context(|| format!("cannot write audit log {}", log_path.display()))?;
    Ok(())
}

/// Append a checkpoint record for the Nth entry: `{"count": N, "entry_hash":
/// "..."}` as JSONL, owner-only like the log itself.
fn write_checkpoint(log_path: &Path, count: usize, entry_hash: &str) -> Result<()> {
    let cp_path = checkpoint_path_for_log(log_path);
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&cp_path)
        .with_context(|| format!("cannot open audit checkpoint {}", cp_path.display()))?;
    let record = serde_json::json!({"count": count, "entry_hash": entry_hash});
    writeln!(file, "{record}")
        .with_context(|| format!("cannot write audit checkpoint {}", cp_path.display()))?;
    Ok(())
}

/// Append one entry to the audit log under the write lock.
///
/// `build` receives the locked-in predecessor hash and returns the complete
/// entry JSON (including its own `entry_hash`). Rotation, the tail scan, and
/// the append all happen inside the critical section, so concurrent processes
/// cannot fork the chain.
fn append_chained_entry(build: impl FnOnce(&str) -> Result<serde_json::Value>) -> Result<()> {
    let path = audit_log_path()?;
    ensure_audit_dir(&path)?;
    let _guard = acquire_audit_write_lock(&path)?;
    let (prev_hash, count) = match maybe_rotate_audit_log(&path)? {
        Some(archived_tail) => (archived_tail, 0),
        None => {
            let (tail, count) = log_tail(&path);
            if count == 0 {
                match latest_archive_tail(&path)? {
                    Some(archived_tail) => (archived_tail, 0),
                    None => (tail, count),
                }
            } else {
                (tail, count)
            }
        }
    };
    let entry = build(&prev_hash)?;
    write_entry_line(&path, &entry)?;
    let count = count + 1;
    if count % AUDIT_CHECKPOINT_EVERY == 0 {
        if let Some(entry_hash) = entry.get("entry_hash").and_then(|v| v.as_str()) {
            write_checkpoint(&path, count, entry_hash)?;
        }
    }
    Ok(())
}

fn utc_now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Append one audit entry for a model request.
///
/// `request_json` is the serialized request payload; the log stores its
/// SHA-256 (not the content itself — content lives in the session database).
/// Best-effort: callers must not fail a request because the audit write failed.
pub fn audit_model_request(
    session_id: &str,
    model: &str,
    endpoint: &str,
    request_json: &str,
) -> Result<()> {
    append_chained_entry(|prev_hash| {
        let request_sha256 = sha256_hex(request_json);
        let ts = utc_now_ts();
        let entry_hash = sha256_hex(&format!(
            "{prev_hash}|{ts}|{session_id}|{model}|{endpoint}|{request_sha256}"
        ));

        Ok(serde_json::json!({
            "ts": ts,
            "session_id": session_id,
            "model": model,
            "endpoint": endpoint,
            "request_chars": request_json.chars().count(),
            "request_sha256": request_sha256,
            "prev_hash": prev_hash,
            "entry_hash": entry_hash,
        }))
    })
}

/// Append a generalized audit event: session lifecycle (`session_start`,
/// `session_end`), extension changes (`extension_added`), and future event
/// types. `details` carries small non-sensitive metadata; the hash chain
/// covers the JSON encoding of the entry as written, and verification
/// re-encodes the parsed entry, so any modification breaks the chain.
/// Best-effort: callers must not fail the operation because the audit write failed.
pub fn audit_event(
    event: &str,
    session_id: Option<&str>,
    details: &serde_json::Value,
) -> Result<()> {
    let session_id = session_id.unwrap_or("");
    // Hashed in the exact encoding written to the file; verification
    // re-encodes the parsed entry, which round-trips deterministically.
    let details_json = serde_json::to_string(details)?;
    let ts = utc_now_ts();
    append_chained_entry(|prev_hash| {
        let entry_hash = sha256_hex(&format!(
            "{prev_hash}|{ts}|{event}|{session_id}|{details_json}"
        ));

        Ok(serde_json::json!({
            "ts": ts,
            "event": event,
            "session_id": session_id,
            "details": details,
            "prev_hash": prev_hash,
            "entry_hash": entry_hash,
        }))
    })
}

/// Recompute an entry's hash from its fields. Legacy `model_request` entries
/// (no `event` field) keep their original hash scheme so entries written by
/// earlier builds still verify.
fn recompute_entry_hash(entry: &serde_json::Value) -> Result<String> {
    let get = |field: &str| {
        entry
            .get(field)
            .and_then(|v| v.as_str())
            .with_context(|| format!("audit entry missing field '{field}'"))
    };
    let prev_hash = get("prev_hash")?;
    let ts = get("ts")?;

    if entry.get("event").is_none() {
        // Legacy model_request shape.
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
    let details_json =
        serde_json::to_string(entry.get("details").unwrap_or(&serde_json::Value::Null))?;
    Ok(sha256_hex(&format!(
        "{prev_hash}|{ts}|{event}|{session_id}|{details_json}"
    )))
}

/// Outcome of [`verify_audit_log`].
///
/// `entries` counts every entry verified across the rotated archives and the
/// live log. `non_monotonic_timestamps` counts entries whose RFC3339 timestamp
/// is earlier than the previous entry's (or fails to parse) — reported as an
/// operator signal, never a verification failure: clock skew is not proof of
/// tampering.
pub struct VerifyReport {
    pub entries: usize,
    pub non_monotonic_timestamps: usize,
}

/// Checkpoint records for one log file: entry count → entry hash.
///
/// A missing checkpoint file is not an error (logs written before
/// checkpoints existed, or a file with fewer than 100 entries): those entries
/// are verified by the hash chain alone. A present-but-corrupt checkpoint
/// file fails closed.
fn read_checkpoints(checkpoint_path: &Path) -> Result<std::collections::HashMap<u64, String>> {
    let mut map = std::collections::HashMap::new();
    let content = match std::fs::read_to_string(checkpoint_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(map),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("cannot read audit checkpoint {}", checkpoint_path.display())
            })?;
        }
    };
    for (lineno, line) in content.lines().enumerate() {
        let lineno = lineno + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: serde_json::Value = serde_json::from_str(line).with_context(|| {
            format!(
                "audit checkpoint {} line {lineno} is not valid JSON",
                checkpoint_path.display()
            )
        })?;
        let count = record
            .get("count")
            .and_then(|v| v.as_u64())
            .with_context(|| {
                format!(
                    "audit checkpoint {} line {lineno} missing count",
                    checkpoint_path.display()
                )
            })?;
        let entry_hash = record
            .get("entry_hash")
            .and_then(|v| v.as_str())
            .with_context(|| {
                format!(
                    "audit checkpoint {} line {lineno} missing entry_hash",
                    checkpoint_path.display()
                )
            })?;
        map.insert(count, entry_hash.to_string());
    }
    Ok(map)
}

/// Verify the audit log's hash chain end to end, across rotations.
///
/// Replays rotated `audit-*.log` archives in chronological (filename) order,
/// then the live `audit.log`, threading `prev_hash` across files so a
/// rotation never breaks the chain. Every 100th entry of each file is
/// cross-checked against that file's checkpoint record — a mismatch is
/// tamper. Timestamps are checked for monotonicity and reported, never a
/// failure.
///
/// Fails on the first broken link (a `prev_hash` that doesn't match the
/// previous entry, across file boundaries) or tampered entry (an
/// `entry_hash` that doesn't recompute, or a checkpoint mismatch), naming the
/// offending file and line.
pub fn verify_audit_log() -> Result<VerifyReport> {
    let live_path = audit_log_path()?;
    let mut files = rotated_audit_logs(&live_path)?;
    if live_path.exists() {
        files.push(live_path.clone());
    }
    if files.is_empty() {
        anyhow::bail!("cannot open audit log {}", live_path.display());
    }

    let mut report = VerifyReport {
        entries: 0,
        non_monotonic_timestamps: 0,
    };
    let mut prev_hash = GENESIS_HASH.to_string();
    let mut prev_ts: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    for path in &files {
        let file = std::fs::File::open(path)
            .with_context(|| format!("cannot open audit log {}", path.display()))?;
        let checkpoints = read_checkpoints(&checkpoint_path_for_log(path))?;
        let mut file_count = 0u64;
        for (lineno, line) in BufReader::new(file).lines().enumerate() {
            let lineno = lineno + 1;
            let line =
                line.with_context(|| format!("cannot read {} line {lineno}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("{} line {lineno} is not valid JSON", path.display()))?;
            let entry_prev = entry
                .get("prev_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing>");
            if entry_prev != prev_hash {
                anyhow::bail!(
                    "audit log chain broken in {} at line {lineno}: prev_hash mismatch",
                    path.display()
                );
            }
            let recomputed = recompute_entry_hash(&entry).with_context(|| {
                format!("cannot recompute hash for {} line {lineno}", path.display())
            })?;
            let entry_hash = entry
                .get("entry_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing>");
            if recomputed != entry_hash {
                anyhow::bail!(
                    "audit log tamper detected in {} at line {lineno}",
                    path.display()
                );
            }
            file_count += 1;
            report.entries += 1;
            if file_count.is_multiple_of(AUDIT_CHECKPOINT_EVERY as u64) {
                if let Some(expected) = checkpoints.get(&file_count) {
                    if expected != entry_hash {
                        anyhow::bail!(
                            "audit log checkpoint mismatch in {} at entry {file_count}: \
                             checkpoint does not match entry hash",
                            path.display()
                        );
                    }
                }
            }
            match entry
                .get("ts")
                .and_then(|v| v.as_str())
                .map(chrono::DateTime::parse_from_rfc3339)
            {
                Some(Ok(ts)) => {
                    if prev_ts.is_some_and(|prev| ts < prev) {
                        report.non_monotonic_timestamps += 1;
                    }
                    prev_ts = Some(ts);
                }
                _ => report.non_monotonic_timestamps += 1,
            }
            prev_hash = entry_hash.to_string();
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Audit log forwarding
// ---------------------------------------------------------------------------

/// Build-time audit sink: the HTTPS endpoint new audit entries are shipped
/// to (e.g. a SIEM ingestion API). Baked in at compile time via
/// `WARMACHINE_ONPREM_AUDIT_SINK_URL`; when unset, forwarding is disabled
/// and the local log is the only copy. Like the LLM base URL, this cannot
/// be changed at runtime — the sink is part of the build's compliance
/// posture, not a user preference.
fn audit_sink_url() -> Option<&'static str> {
    option_env!("WARMACHINE_ONPREM_AUDIT_SINK_URL")
}

/// Optional bearer token for the sink, baked in at compile time via
/// `WARMACHINE_ONPREM_AUDIT_SINK_TOKEN`. Sent as an `Authorization: Bearer`
/// header. Prefer mutual TLS or network-level auth where the SIEM supports
/// it; the token is the portable fallback.
fn audit_sink_token() -> Option<&'static str> {
    option_env!("WARMACHINE_ONPREM_AUDIT_SINK_TOKEN")
}

/// Validate the baked-in SIEM sink URL before the forwarder starts.
///
/// Fails when the URL is not parseable, does not use `https`, or is not on
/// the build's network allowlist — a build-time typo must not silently ship
/// audit metadata to the wrong host. No sink baked in is valid: forwarding
/// stays disabled and the local log is the only copy.
pub fn validate_audit_sink() -> Result<()> {
    let Some(sink) = audit_sink_url() else {
        return Ok(());
    };
    let url = url::Url::parse(sink)
        .with_context(|| format!("baked-in audit sink URL is invalid: {sink}"))?;
    if url.scheme() != "https" {
        anyhow::bail!("baked-in audit sink URL must use https: {sink}");
    }
    check_url_allowed(sink)
        .with_context(|| format!("baked-in audit sink URL is not allowlisted: {sink}"))?;
    Ok(())
}

const FORWARD_CURSOR_NAME: &str = "audit.forward.cursor";
const FORWARD_INTERVAL_SECS: u64 = 60;
const FORWARD_BATCH_LINES: usize = 500;
const FORWARD_TIMEOUT_SECS: u64 = 30;

/// Keyring account holding the forward cursor's integrity key (service
/// `warmachine`, shared with the session DEK's service).
const CURSOR_MAC_KEYRING_ACCOUNT: &str = "audit-cursor-mac";

fn forward_cursor_path() -> Result<PathBuf> {
    Ok(crate::config::paths::Paths::config_dir().join(FORWARD_CURSOR_NAME))
}

/// The `entry_hash` of the last entry successfully forwarded, if the cursor
/// file is present and passes integrity verification.
///
/// The cursor is stored as `{"cursor": hash, "mac": tag}` where the tag is
/// [`cursor_tag`] under a keychain-held key. A cursor that fails verification
/// is never trusted: the incident is logged LOUD and as an `audit_cursor_tamper`
/// event in the audit log itself, and forwarding anchors at the pre-tamper
/// tail so this pass ships the tamper alert to the SIEM instead of silently
/// dropping it. (A plain anchor-at-end would leave the SIEM blind to the
/// attack — the exact failure mode this protects against.)
fn read_forward_cursor() -> Option<String> {
    let raw = std::fs::read_to_string(forward_cursor_path().ok()?)
        .ok()?
        .trim()
        .to_string();
    if raw.is_empty() {
        return None;
    }
    if !raw.starts_with('{') {
        // Legacy plain-hash cursor (pre-integrity builds): accept once; the
        // next successful forward rewrites it in the protected format.
        return Some(raw);
    }
    let (cursor, mac) = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(doc) => match (
            doc.get("cursor").and_then(|v| v.as_str()),
            doc.get("mac").and_then(|v| v.as_str()),
        ) {
            (Some(cursor), Some(mac)) => (cursor.to_string(), mac.to_string()),
            _ => {
                return handle_cursor_tamper("malformed", &raw);
            }
        },
        Err(_) => {
            return handle_cursor_tamper("malformed", &raw);
        }
    };
    match cursor_mac_key() {
        Ok(key) if cursor_tag(&key, &cursor) == mac => Some(cursor),
        Ok(_) => handle_cursor_tamper("mac_mismatch", &cursor),
        Err(e) => {
            tracing::error!("audit cursor key unavailable, cursor treated as untrusted: {e:#}");
            handle_cursor_tamper(&format!("key_unavailable: {e:#}"), &cursor)
        }
    }
}

/// Record a cursor integrity failure and decide where forwarding resumes.
///
/// The tampered value is never trusted. The tail hash is captured before the
/// tamper event is appended, so the forwarder's scan finds it and this pass
/// forwards the tamper alert itself to the SIEM. If the log had no entries
/// yet, there is no pre-tamper tail to anchor at: resume with no cursor so
/// the scan collects everything, including the tamper alert.
fn handle_cursor_tamper(reason: &str, cursor: &str) -> Option<String> {
    let (tail_before, had_entries) = audit_log_path()
        .ok()
        .map(|p| {
            let (hash, count) = log_tail(&p);
            (hash, count > 0)
        })
        .unwrap_or((GENESIS_HASH.to_string(), false));
    tracing::error!(
        "AUDIT CURSOR INTEGRITY FAILURE ({reason}): the SIEM forward cursor failed verification \
         and was NOT trusted; forwarding re-anchored and the incident was logged to the audit \
         trail — investigate immediately"
    );
    if let Err(e) = audit_event(
        "audit_cursor_tamper",
        None,
        &serde_json::json!({
            "reason": reason,
            "cursor": cursor.chars().take(128).collect::<String>(),
        }),
    ) {
        tracing::error!("audit log write failed while recording cursor tamper: {e:#}");
    }
    had_entries.then_some(tail_before)
}

fn write_forward_cursor(entry_hash: &str) -> Result<()> {
    let path = forward_cursor_path()?;
    ensure_audit_dir(&path)?;
    let key = cursor_mac_key()?;
    let doc = serde_json::json!({"cursor": entry_hash, "mac": cursor_tag(&key, entry_hash)});
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&path)
        .with_context(|| format!("cannot write audit cursor {}", path.display()))?;
    writeln!(file, "{doc}")
        .with_context(|| format!("cannot write audit cursor {}", path.display()))?;
    Ok(())
}

/// Collect `(entry_hash, line)` pairs for entries appended after the cursor.
///
/// On first run (no cursor), all entries are collected: the sink receives the
/// full history. When the cursor fails integrity verification, it is not
/// trusted — see [`read_forward_cursor`]: the tamper is alerted and this pass
/// re-anchors at the pre-tamper tail so the alert itself is forwarded. When
/// the cursor's hash is simply no longer in the log (log truncated or
/// rotated between passes), forwarding anchors at the current end of the log:
/// the local file remains the complete record, and backfilling history after
/// an anomaly is a manual operator task (`verify_audit_log` + any NDJSON
/// shipper). Rotated archives are verified locally but never forwarded: the
/// SIEM's copy of pre-rotation history depends on the forwarder having kept
/// up before the rotation.
fn unforwarded_entries() -> Result<Vec<(String, String)>> {
    let path = audit_log_path()?;
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("cannot open audit log {}", path.display()))?
        }
    };

    let cursor = read_forward_cursor();
    let mut entries = Vec::new();
    let mut found_cursor = cursor.is_none();
    let mut last_hash: Option<String> = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let entry_hash = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| {
                v.get("entry_hash")
                    .and_then(|h| h.as_str())
                    .map(str::to_string)
            });
        let Some(entry_hash) = entry_hash else {
            continue;
        };
        last_hash = Some(entry_hash.clone());
        if !found_cursor {
            if Some(entry_hash.as_str()) == cursor.as_deref() {
                found_cursor = true;
            }
            continue;
        }
        entries.push((entry_hash, line));
    }
    if !found_cursor {
        // Stale cursor (log truncated/rotated, or first run after the cursor
        // file was deleted): anchor at the current end of the log so the next
        // run picks up new entries. History stays in the local file.
        if let Some(last) = last_hash {
            write_forward_cursor(&last)?;
        }
    }
    Ok(entries)
}

/// POST one batch of NDJSON audit entries to the sink.
///
/// The sink must accept `application/x-ndjson`. Any 2xx is success; anything
/// else is an error and the cursor is not advanced, so entries are retried on
/// the next pass. The request runs over the process-default TLS stack, which
/// in on-prem builds is the FIPS-validated provider (installed at startup
/// before this task is spawned).
async fn post_audit_batch(client: &reqwest::Client, sink: &str, lines: &[String]) -> Result<()> {
    let body = lines.join("\n");
    let mut req = client
        .post(sink)
        .header("Content-Type", "application/x-ndjson")
        .body(body);
    if let Some(token) = audit_sink_token() {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("audit forward to {sink} failed"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("audit sink returned {status}");
    }
    Ok(())
}

/// Forward all unforwarded audit entries to the sink, in batches.
///
/// Returns the number of entries forwarded. Best-effort: failures are
/// returned as errors for the caller to log, but the local audit log is
/// unaffected — it remains the source of truth and entries are retried on
/// the next call. Does nothing when no sink URL was baked in at compile time.
pub async fn forward_audit_log() -> Result<usize> {
    let Some(sink) = audit_sink_url() else {
        return Ok(0);
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FORWARD_TIMEOUT_SECS))
        .build()
        .context("cannot build audit forward HTTP client")?;

    let entries = unforwarded_entries()?;
    let mut forwarded = 0usize;
    for chunk in entries.chunks(FORWARD_BATCH_LINES) {
        let lines: Vec<String> = chunk.iter().map(|(_, line)| line.clone()).collect();
        post_audit_batch(&client, sink, &lines).await?;
        let last_hash = &chunk[chunk.len() - 1].0;
        write_forward_cursor(last_hash)?;
        forwarded += chunk.len();
    }
    Ok(forwarded)
}

/// Spawn the background audit forwarder.
///
/// Does an immediate forward attempt, then re-checks every
/// `FORWARD_INTERVAL_SECS`. Failures are logged and retried; they never
/// affect the running session. No-op when no sink URL was baked in. Must be
/// called from within a Tokio runtime, after `init_fips_crypto()` so the
/// forwarding TLS uses the FIPS provider.
pub fn spawn_audit_forwarder() {
    if audit_sink_url().is_none() {
        return;
    }
    tokio::spawn(async {
        loop {
            match forward_audit_log().await {
                Ok(0) => {}
                Ok(n) => tracing::info!("forwarded {n} audit log entries to SIEM sink"),
                Err(e) => tracing::warn!("audit log forwarding failed (will retry): {e:#}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(FORWARD_INTERVAL_SECS)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Session retention
// ---------------------------------------------------------------------------

/// Default session retention: sessions untouched for this long are purged.
const DEFAULT_RETENTION_DAYS: u64 = 90;

/// Retention cutoff from `WARMACHINE_SESSION_RETENTION_DAYS` (default 90).
///
/// The retention policy cannot be disabled in on-prem builds: a value of 0 or
/// an unparsable value fails closed rather than silently keeping sessions
/// forever, because unbounded retention of CUI/ITAR session data violates the
/// data-minimization posture this build exists to enforce.
pub fn session_retention_cutoff() -> Result<chrono::DateTime<chrono::Utc>> {
    let days: u64 = match std::env::var("WARMACHINE_SESSION_RETENTION_DAYS") {
        Ok(raw) => raw.parse().with_context(|| {
            format!(
                "WARMACHINE_SESSION_RETENTION_DAYS must be a positive number of days, got {raw:?}"
            )
        })?,
        Err(_) => DEFAULT_RETENTION_DAYS,
    };
    if days == 0 {
        anyhow::bail!(
            "WARMACHINE_SESSION_RETENTION_DAYS=0 disables retention; \
             on-prem builds require a positive retention period"
        );
    }
    let cutoff = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(days as i64))
        .context("retention period out of range")?;
    Ok(cutoff)
}

// ---------------------------------------------------------------------------
// FIPS 140-3 validated cryptography
// ---------------------------------------------------------------------------

/// Install the FIPS 140-3 validated crypto provider as the process default.
///
/// Uses rustls's `fips` feature, which switches the crypto backend to the
/// FIPS-validated AWS-LC module (FIPS 140-3 certificate #4816). Must be called
/// before any TLS `ClientConfig`/`ServerConfig` is created — reqwest (used by
/// all provider HTTP clients) picks up the process-default provider.
///
/// This is idempotent: if another part of the process already installed a
/// provider, the FIPS install is skipped (the `let _ =` ignores the
/// "already installed" error). Callers that need a hard guarantee should use
/// `verify_fips_mode` on their TLS configs.
#[cfg(feature = "fips")]
pub fn init_fips_crypto() {
    let _ = rustls::crypto::default_fips_provider().install_default();
}

/// Returns true if the FIPS-validated provider is the process default.
///
/// Used by startup checks to confirm the binary is actually running with
/// FIPS-approved cryptography, not just compiled with the feature.
#[cfg(feature = "fips")]
pub fn is_fips_provider_active() -> bool {
    use rustls::crypto::CryptoProvider;
    CryptoProvider::get_default()
        .map(|p| p.fips())
        .unwrap_or(false)
}
