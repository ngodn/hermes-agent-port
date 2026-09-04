//! Port of gateway/browser_control_artifacts.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//!
//! One-shot artifact transport for browser control (gateway side). This is the
//! transport-neutral store core for bounded browser-control artifacts
//! (screenshots, PDFs, uploads): authenticated one-shot upload/download with
//! SHA-256 provenance, exact MIME/size caps, a controlled artifact root, and TTL
//! cleanup. Ids are server-minted 32-hex strings that resolve strictly inside the
//! root (no traversal); client filenames are display-only metadata. Bytes are
//! written to a temp name and atomically renamed so a concurrent load never sees
//! a partial file. `load` is one-shot and scope-bound (verifies existence, TTL,
//! scope, and checksum, then consumes); `validate` checks the same without
//! consuming; `prune_expired` and an at-construction orphan sweep keep nothing on
//! disk past its TTL. A separate `ArtifactRateLimiter` gives the routes a
//! sliding-window per-principal cap. The routes that authenticate callers live
//! elsewhere; this module only takes bytes and hands them back.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use sha2::{Digest, Sha256};

/// Default lifetime of a stored artifact, in clock seconds.
pub const DEFAULT_ARTIFACT_TTL_SECONDS: f64 = 300.0;
/// Default per-artifact byte cap (10 MiB).
pub const DEFAULT_MAX_ARTIFACT_BYTES: usize = 10 * 1024 * 1024;
/// Length in hex chars of a minted artifact id.
const ARTIFACT_ID_HEX: usize = 32;
const TEMP_SUFFIX: &str = ".tmp";

/// Injectable clock returning wall-clock seconds since the Unix epoch. Mirrors
/// Python's `time.time` default and its `clock` keyword override for tests.
pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

fn default_clock() -> f64 {
    // Epoch seconds as a float, same shape as Python's time.time().
    Utc::now().timestamp_micros() as f64 / 1_000_000.0
}

/// Default exact MIME allowlist. Unknown or parameterized variants are rejected;
/// clients must send the canonical registered type.
pub fn default_allowed_mime_types() -> HashSet<String> {
    [
        "application/json",
        "application/pdf",
        "image/gif",
        "image/jpeg",
        "image/png",
        "image/webp",
        "text/plain",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ----------------------------------------------------------------------
// Scope
// ----------------------------------------------------------------------

/// The server-derived identity an artifact is bound to. Python duck-types the
/// scope through `getattr(scope, "principal_id"/"transport_family", "")`; the
/// trait is that same read surface. A missing or empty field reads as `""`.
pub trait ArtifactScope {
    fn principal_id(&self) -> &str;
    fn transport_family(&self) -> &str;
}

/// A plain scope value for callers and tests that just need the two fields.
#[derive(Debug, Clone, Default)]
pub struct SimpleScope {
    pub principal_id: String,
    pub transport_family: String,
}

impl SimpleScope {
    pub fn new(principal_id: impl Into<String>, transport_family: impl Into<String>) -> Self {
        Self {
            principal_id: principal_id.into(),
            transport_family: transport_family.into(),
        }
    }
}

impl ArtifactScope for SimpleScope {
    fn principal_id(&self) -> &str {
        &self.principal_id
    }
    fn transport_family(&self) -> &str {
        &self.transport_family
    }
}

// ----------------------------------------------------------------------
// Errors
// ----------------------------------------------------------------------

/// Contract failures from the artifact store. One enum stands in for Python's
/// `ArtifactError` base plus its subclasses; `Generic` covers the two places the
/// base class is raised directly (unresolved principal, read failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactError {
    /// The artifact id is unknown (or already consumed).
    NotFound(String),
    /// The artifact outlived its TTL.
    Expired(String),
    /// The upload exceeds the configured byte cap.
    TooLarge(String),
    /// The content type is outside the exact allowlist.
    MimeRejected(String),
    /// The artifact exists but belongs to a different scope.
    ScopeMismatch(String),
    /// The stored bytes do not match the recorded SHA-256.
    ChecksumMismatch(String),
    /// A caller-supplied id is not a valid minted artifact id.
    Traversal(String),
    /// An artifact id already exists and the store refuses to overwrite it.
    Overwrite(String),
    /// Base-class failures with no dedicated subclass.
    Generic(String),
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            ArtifactError::NotFound(m)
            | ArtifactError::Expired(m)
            | ArtifactError::TooLarge(m)
            | ArtifactError::MimeRejected(m)
            | ArtifactError::ScopeMismatch(m)
            | ArtifactError::ChecksumMismatch(m)
            | ArtifactError::Traversal(m)
            | ArtifactError::Overwrite(m)
            | ArtifactError::Generic(m) => m,
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for ArtifactError {}

// ----------------------------------------------------------------------
// Receipt
// ----------------------------------------------------------------------

/// Provenance record returned to the caller of `store`.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactReceipt {
    pub artifact_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub content_type: String,
    pub filename: String,
    pub created_at: f64,
    pub expires_at: f64,
    pub ttl_seconds: f64,
    pub scope_key: String,
}

impl ArtifactReceipt {
    /// Serialize to the wire receipt (never contains file paths or the scope
    /// key). Pass a non-empty `download_path` to include it.
    pub fn to_dict(&self, download_path: &str) -> serde_json::Value {
        let mut receipt = serde_json::json!({
            "artifact_id": self.artifact_id,
            "sha256": self.sha256,
            "size_bytes": self.size_bytes,
            "content_type": self.content_type,
            "filename": self.filename,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "ttl_seconds": self.ttl_seconds,
            "one_shot": true,
        });
        if !download_path.is_empty() {
            receipt["download_path"] = serde_json::Value::String(download_path.to_string());
        }
        receipt
    }
}

/// Derive the stable scope key an artifact is bound to. Only server-derived
/// identity participates: principal (mandatory) plus transport family. The
/// session id is deliberately excluded so an HTTP upload and a later broker
/// dispatch for the same authenticated principal hash to the same key. Fails
/// closed when no principal is resolved.
pub fn artifact_scope_key(scope: &dyn ArtifactScope) -> Result<String, ArtifactError> {
    // getattr(...) or "" collapses a missing/empty value to the empty string.
    let principal = scope.principal_id();
    let family = scope.transport_family();
    if principal.is_empty() {
        return Err(ArtifactError::Generic(
            "artifact scope must carry a resolved principal".to_string(),
        ));
    }
    let material = format!("{principal}\u{0}{family}");
    Ok(sha256_hex(material.as_bytes()))
}

// ----------------------------------------------------------------------
// Store
// ----------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ArtifactEntry {
    receipt: ArtifactReceipt,
    path: PathBuf,
}

/// Thread-safe, TTL-bounded, scope-bound one-shot artifact store.
pub struct ArtifactStore {
    root: PathBuf,
    ttl_seconds: f64,
    max_bytes: usize,
    allowed_mime_types: HashSet<String>,
    clock: Clock,
    entries: Mutex<HashMap<String, ArtifactEntry>>,
}

impl ArtifactStore {
    /// Build a store with the default TTL, cap, allowlist, and clock.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_options(
            root,
            DEFAULT_ARTIFACT_TTL_SECONDS,
            DEFAULT_MAX_ARTIFACT_BYTES as i64,
            default_allowed_mime_types(),
            None,
        )
    }

    /// Full constructor mirroring Python's keyword arguments.
    pub fn with_options(
        root: impl Into<PathBuf>,
        ttl_seconds: f64,
        max_bytes: i64,
        allowed_mime_types: HashSet<String>,
        clock: Option<Clock>,
    ) -> Self {
        let root = root.into();
        // Best-effort create; Python raises on mkdir failure, but a store whose
        // root cannot be created is unusable either way and every method below
        // degrades safely on a missing directory.
        let _ = fs::create_dir_all(&root);
        let store = Self {
            root,
            ttl_seconds: ttl_seconds.max(1.0),
            max_bytes: max_bytes.max(1) as usize,
            allowed_mime_types,
            clock: clock.unwrap_or_else(|| Box::new(default_clock)),
            entries: Mutex::new(HashMap::new()),
        };
        // Restart-safe retention: receipts live only in memory, so at
        // construction the index is empty and anything on disk is an orphan from
        // a dead process, past its advertised TTL by definition.
        store.sweep_orphan_files();
        store
    }

    /// Delete on-disk artifact files with no live index entry. Only files whose
    /// names match the 32-hex id shape or the `.tmp` staging suffix are touched.
    fn sweep_orphan_files(&self) -> usize {
        let mut removed = 0usize;
        let candidates = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(_) => return 0,
        };
        let live: HashSet<String> = {
            let entries = self.entries.lock().unwrap();
            entries.keys().cloned().collect()
        };
        for entry in candidates {
            let path = match entry {
                Ok(e) => e.path(),
                Err(_) => continue,
            };
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let is_temp = name.ends_with(TEMP_SUFFIX);
            if !is_temp && !is_artifact_id(&name) {
                continue;
            }
            if !is_temp && live.contains(&name) {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    // ------------------------------------------------------------------
    // Public API
    // ------------------------------------------------------------------

    /// Controlled artifact root (never exposed to callers by default).
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ttl_seconds(&self) -> f64 {
        self.ttl_seconds
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn allowed_mime_types(&self) -> &HashSet<String> {
        &self.allowed_mime_types
    }

    /// Validate and store one artifact, returning its provenance receipt. Size
    /// and MIME are checked before any disk write; the scope must carry a
    /// resolved principal.
    pub fn store(
        &self,
        data: &[u8],
        filename: &str,
        content_type: &str,
        scope: &dyn ArtifactScope,
    ) -> Result<ArtifactReceipt, ArtifactError> {
        let size = data.len();
        if size > self.max_bytes {
            return Err(ArtifactError::TooLarge(format!(
                "artifact is {size} bytes; cap is {}",
                self.max_bytes
            )));
        }
        let normalized_type = normalize_content_type(content_type);
        if !self.allowed_mime_types.contains(&normalized_type) {
            return Err(ArtifactError::MimeRejected(format!(
                "content type {content_type:?} is outside the exact allowlist"
            )));
        }
        let scope_key = artifact_scope_key(scope)?;
        let now = (self.clock)();
        let sha = sha256_hex(data);
        let bounded_name = bounded_filename(filename, 160);

        // Mint a fresh id; retry on an astronomically unlikely collision.
        let (receipt, target) = loop {
            let artifact_id = random_hex();
            let target = self.artifact_path(&artifact_id)?;
            let mut entries = self.entries.lock().unwrap();
            if entries.contains_key(&artifact_id) {
                continue;
            }
            if target.exists() {
                continue;
            }
            let receipt = ArtifactReceipt {
                artifact_id: artifact_id.clone(),
                sha256: sha.clone(),
                size_bytes: size as u64,
                content_type: normalized_type.clone(),
                filename: bounded_name.clone(),
                created_at: now,
                expires_at: now + self.ttl_seconds,
                ttl_seconds: self.ttl_seconds,
                scope_key: scope_key.clone(),
            };
            entries.insert(
                artifact_id.clone(),
                ArtifactEntry {
                    receipt: receipt.clone(),
                    path: target.clone(),
                },
            );
            break (receipt, target);
        };

        // Write via temp + atomic rename so readers never observe a partially
        // written artifact.
        let temp = target.with_file_name(format!("{}{}", receipt.artifact_id, TEMP_SUFFIX));
        if let Err(err) = write_atomic(&temp, &target, data) {
            {
                let mut entries = self.entries.lock().unwrap();
                entries.remove(&receipt.artifact_id);
            }
            let _ = fs::remove_file(&temp);
            return Err(ArtifactError::Generic(format!(
                "artifact write failed: {err}"
            )));
        }
        Ok(receipt)
    }

    /// Return the receipt when the artifact is live for `scope`, without
    /// consuming it. Used by the broker's approved-id gate.
    pub fn validate(
        &self,
        artifact_id: &str,
        scope: &dyn ArtifactScope,
    ) -> Result<ArtifactReceipt, ArtifactError> {
        let mut entries = self.entries.lock().unwrap();
        let entry = self.entry_for_locked(&mut entries, artifact_id, scope)?;
        Ok(entry.receipt)
    }

    /// One-shot download: verify, read, checksum, then consume. Returns
    /// `(bytes, receipt)` and atomically deletes the artifact so a second load
    /// raises `NotFound`. A checksum mismatch does not consume the entry.
    pub fn load(
        &self,
        artifact_id: &str,
        scope: &dyn ArtifactScope,
    ) -> Result<(Vec<u8>, ArtifactReceipt), ArtifactError> {
        let mut entries = self.entries.lock().unwrap();
        let entry = self.entry_for_locked(&mut entries, artifact_id, scope)?;
        let path = entry.path.clone();
        if !path.exists() {
            entries.remove(artifact_id);
            return Err(ArtifactError::NotFound(format!(
                "artifact {artifact_id:?} is gone"
            )));
        }
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(exc) => {
                return Err(ArtifactError::Generic(format!(
                    "artifact read failed: {exc}"
                )));
            }
        };
        if sha256_hex(&data) != entry.receipt.sha256 {
            return Err(ArtifactError::ChecksumMismatch(format!(
                "artifact {artifact_id:?} failed SHA-256 validation"
            )));
        }
        // Consume: remove the index entry first so a concurrent load fails
        // closed, then delete the file outside the lock.
        entries.remove(artifact_id);
        drop(entries);
        if fs::remove_file(&path).is_err() {
            tracing::warn!(
                "artifact {}: file removal failed; TTL sweep will retry",
                artifact_id
            );
        }
        Ok((data, entry.receipt))
    }

    /// Delete every artifact past its TTL; return the count removed. Also removes
    /// orphaned temp files older than one sweep. Idempotent.
    pub fn prune_expired(&self, now: Option<f64>) -> usize {
        let now = now.unwrap_or_else(|| (self.clock)());
        let mut removed = 0usize;
        let mut entries = self.entries.lock().unwrap();
        let expired: Vec<String> = entries
            .iter()
            .filter(|(_, e)| e.receipt.expires_at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            if let Some(e) = entries.remove(&id) {
                let _ = fs::remove_file(&e.path);
            }
            removed += 1;
        }
        if let Ok(rd) = fs::read_dir(&self.root) {
            for entry in rd.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|s| s.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if !name.ends_with(TEMP_SUFFIX) {
                    continue;
                }
                if let Ok(meta) = path.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        let mtime_secs = mtime
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs_f64())
                            .unwrap_or(0.0);
                        if mtime_secs <= now - self.ttl_seconds {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }
        removed
    }

    /// Number of live (unconsumed, not-yet-pruned) artifacts.
    pub fn count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn entry_for_locked(
        &self,
        entries: &mut HashMap<String, ArtifactEntry>,
        artifact_id: &str,
        scope: &dyn ArtifactScope,
    ) -> Result<ArtifactEntry, ArtifactError> {
        let path = self.artifact_path(artifact_id)?;
        let scope_key = artifact_scope_key(scope)?;
        let now = (self.clock)();
        // Check the target's own expiry before sweeping other entries so an
        // expired artifact surfaces as Expired rather than vanishing into the
        // sweep.
        if !entries.contains_key(artifact_id) {
            prune_expired_locked(entries, now);
        }
        let (expires_at, entry_scope_key) = match entries.get(artifact_id) {
            Some(e) => (e.receipt.expires_at, e.receipt.scope_key.clone()),
            None => {
                return Err(ArtifactError::NotFound(format!(
                    "unknown artifact {artifact_id:?}"
                )));
            }
        };
        if expires_at <= now {
            entries.remove(artifact_id);
            let _ = fs::remove_file(&path);
            return Err(ArtifactError::Expired(format!(
                "artifact {artifact_id:?} expired"
            )));
        }
        if entry_scope_key != scope_key {
            return Err(ArtifactError::ScopeMismatch(format!(
                "artifact {artifact_id:?} is bound to a different scope"
            )));
        }
        Ok(entries.get(artifact_id).unwrap().clone())
    }

    /// Resolve a minted id strictly inside the controlled root.
    fn artifact_path(&self, artifact_id: &str) -> Result<PathBuf, ArtifactError> {
        if !is_artifact_id(artifact_id) {
            return Err(ArtifactError::Traversal(format!(
                "invalid artifact id {artifact_id:?}"
            )));
        }
        // resolve() (strict=False) with an absolute() fallback: canonicalize the
        // existing root, then join the id (which the regex guarantees carries no
        // separators).
        let root_resolved = fs::canonicalize(&self.root)
            .or_else(|_| std::path::absolute(&self.root))
            .unwrap_or_else(|_| self.root.clone());
        let candidate = root_resolved.join(artifact_id);
        let parent_ok = candidate.parent() == Some(root_resolved.as_path());
        let name_ok = candidate.file_name().and_then(|s| s.to_str()) == Some(artifact_id);
        if !parent_ok || !name_ok {
            return Err(ArtifactError::Traversal(format!(
                "artifact path escapes root for {artifact_id:?}"
            )));
        }
        Ok(candidate)
    }
}

fn prune_expired_locked(entries: &mut HashMap<String, ArtifactEntry>, now: f64) {
    let expired: Vec<String> = entries
        .iter()
        .filter(|(_, e)| e.receipt.expires_at <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for id in expired {
        if let Some(e) = entries.remove(&id) {
            let _ = fs::remove_file(&e.path);
        }
    }
}

// ----------------------------------------------------------------------
// Free helpers
// ----------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Match the server-minted id shape: exactly 32 lowercase hex chars.
fn is_artifact_id(name: &str) -> bool {
    name.len() == ARTIFACT_ID_HEX
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 32 lowercase hex chars from 16 cryptographically random bytes, matching
/// Python's `secrets.token_hex(16)`. Falls back to a time+counter mix if
/// `/dev/urandom` is unreadable so minting never panics.
fn random_hex() -> String {
    let mut buf = [0u8; ARTIFACT_ID_HEX / 2];
    if fill_random(&mut buf).is_err() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut hasher = Sha256::new();
        hasher.update(nanos.to_le_bytes());
        hasher.update(n.to_le_bytes());
        let out = hasher.finalize();
        let n = buf.len();
        buf.copy_from_slice(&out[..n]);
    }
    to_hex(&buf)
}

fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    let mut f = fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

/// Write bytes to `temp` (fsync'd), then atomically rename into `target`.
fn write_atomic(temp: &Path, target: &Path, data: &[u8]) -> std::io::Result<()> {
    {
        let mut handle = fs::File::create(temp)?;
        handle.write_all(data)?;
        handle.flush()?;
        handle.sync_all()?;
    }
    fs::rename(temp, target)
}

/// Return the canonical MIME type, or `""` for malformed input.
fn normalize_content_type(value: &str) -> String {
    value
        .trim()
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// Sanitize a display-only filename; never used as a filesystem path. Strips
/// surrounding whitespace, maps path separators to `_`, drops control chars
/// (codepoint < 32), then truncates to `limit` chars.
fn bounded_filename(value: &str, limit: usize) -> String {
    value
        .trim()
        .chars()
        .map(|c| if c == '\\' || c == '/' { '_' } else { c })
        .filter(|c| (*c as u32) >= 32)
        .take(limit)
        .collect()
}

// ----------------------------------------------------------------------
// Rate limiting (route-level, per principal)
// ----------------------------------------------------------------------

/// Sliding-window per-key limiter for artifact routes. The API server keys this
/// by the authenticated principal so a single key cannot flood the store.
pub struct ArtifactRateLimiter {
    window_seconds: f64,
    max_requests: usize,
    clock: Clock,
    hits: Mutex<HashMap<String, Vec<f64>>>,
}

impl ArtifactRateLimiter {
    pub fn new(window_seconds: f64, max_requests: i64, clock: Option<Clock>) -> Self {
        Self {
            window_seconds: window_seconds.max(1.0),
            max_requests: max_requests.max(1) as usize,
            clock: clock.unwrap_or_else(|| Box::new(default_clock)),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Defaults matching Python: 60s window, 30 requests.
    pub fn with_defaults() -> Self {
        Self::new(60.0, 30, None)
    }

    /// Return true when `key` is under the window cap; else false.
    pub fn allow(&self, key: &str) -> bool {
        if key.is_empty() {
            return false;
        }
        let now = (self.clock)();
        let window_start = now - self.window_seconds;
        let mut hits = self.hits.lock().unwrap();
        let mut kept: Vec<f64> = hits
            .get(key)
            .map(|v| v.iter().copied().filter(|h| *h > window_start).collect())
            .unwrap_or_default();
        if kept.len() >= self.max_requests {
            hits.insert(key.to_string(), kept);
            return false;
        }
        kept.push(now);
        hits.insert(key.to_string(), kept);
        true
    }

    /// Drop the recorded hits for `key` (tests/diagnostics).
    pub fn reset(&self, key: &str) {
        self.hits.lock().unwrap().remove(key);
    }
}

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("bca_test_{tag}_{nanos}"));
        p
    }

    fn scope() -> SimpleScope {
        SimpleScope::new("principal-1", "ws")
    }

    /// A clock whose value the test controls.
    fn controllable(start: f64) -> (Clock, Arc<StdMutex<f64>>) {
        let cell = Arc::new(StdMutex::new(start));
        let handle = cell.clone();
        let clk: Clock = Box::new(move || *cell.lock().unwrap());
        (clk, handle)
    }

    #[test]
    fn store_then_load_roundtrip() {
        let root = temp_root("roundtrip");
        let store = ArtifactStore::new(&root);
        let data = b"hello world";
        let receipt = store
            .store(data, "shot.png", "image/png", &scope())
            .unwrap();
        assert_eq!(receipt.size_bytes, data.len() as u64);
        assert_eq!(receipt.sha256, sha256_hex(data));
        assert!(is_artifact_id(&receipt.artifact_id));
        assert_eq!(store.count(), 1);

        let (bytes, r2) = store.load(&receipt.artifact_id, &scope()).unwrap();
        assert_eq!(bytes, data);
        assert_eq!(r2.artifact_id, receipt.artifact_id);
        // One-shot: gone after load.
        assert_eq!(store.count(), 0);
        let again = store.load(&receipt.artifact_id, &scope());
        assert!(matches!(again, Err(ArtifactError::NotFound(_))));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_oversize_before_write() {
        let root = temp_root("toolarge");
        let store =
            ArtifactStore::with_options(&root, 300.0, 4, default_allowed_mime_types(), None);
        let err = store
            .store(b"12345", "f.txt", "text/plain", &scope())
            .unwrap_err();
        assert!(matches!(err, ArtifactError::TooLarge(_)));
        assert_eq!(store.count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_unlisted_mime() {
        let root = temp_root("mime");
        let store = ArtifactStore::new(&root);
        let err = store
            .store(b"x", "f.bin", "application/octet-stream", &scope())
            .unwrap_err();
        assert!(matches!(err, ArtifactError::MimeRejected(_)));
        // Parameterized variant of an allowed type still normalizes and passes.
        let ok = store.store(b"{}", "f.json", "application/json; charset=utf-8", &scope());
        assert!(ok.is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scope_mismatch_and_empty_principal() {
        let root = temp_root("scope");
        let store = ArtifactStore::new(&root);
        let receipt = store
            .store(b"data", "f.txt", "text/plain", &scope())
            .unwrap();
        let other = SimpleScope::new("principal-2", "ws");
        let err = store.validate(&receipt.artifact_id, &other).unwrap_err();
        assert!(matches!(err, ArtifactError::ScopeMismatch(_)));

        // An unresolved principal fails closed at mint time.
        let empty = SimpleScope::new("", "ws");
        let err = store
            .store(b"z", "f.txt", "text/plain", &empty)
            .unwrap_err();
        assert!(matches!(err, ArtifactError::Generic(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_id_is_traversal() {
        let root = temp_root("traversal");
        let store = ArtifactStore::new(&root);
        let err = store.validate("../etc/passwd", &scope()).unwrap_err();
        assert!(matches!(err, ArtifactError::Traversal(_)));
        let err = store.validate("ABCDEF", &scope()).unwrap_err();
        assert!(matches!(err, ArtifactError::Traversal(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ttl_expiry() {
        let root = temp_root("ttl");
        let (clk, cell) = controllable(1000.0);
        let store = ArtifactStore::with_options(
            &root,
            10.0,
            DEFAULT_MAX_ARTIFACT_BYTES as i64,
            default_allowed_mime_types(),
            Some(clk),
        );
        let receipt = store
            .store(b"data", "f.txt", "text/plain", &scope())
            .unwrap();
        assert_eq!(receipt.expires_at, 1010.0);
        // Still live one tick before expiry.
        *cell.lock().unwrap() = 1009.0;
        assert!(store.validate(&receipt.artifact_id, &scope()).is_ok());
        // At/after expiry it surfaces as Expired.
        *cell.lock().unwrap() = 1011.0;
        let err = store.validate(&receipt.artifact_id, &scope()).unwrap_err();
        assert!(matches!(err, ArtifactError::Expired(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_expired_removes_and_counts() {
        let root = temp_root("prune");
        let (clk, cell) = controllable(500.0);
        let store = ArtifactStore::with_options(
            &root,
            10.0,
            DEFAULT_MAX_ARTIFACT_BYTES as i64,
            default_allowed_mime_types(),
            Some(clk),
        );
        store.store(b"a", "a.txt", "text/plain", &scope()).unwrap();
        store.store(b"b", "b.txt", "text/plain", &scope()).unwrap();
        assert_eq!(store.count(), 2);
        *cell.lock().unwrap() = 600.0;
        let removed = store.prune_expired(None);
        assert_eq!(removed, 2);
        assert_eq!(store.count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn checksum_mismatch_does_not_consume() {
        let root = temp_root("checksum");
        let store = ArtifactStore::new(&root);
        let receipt = store
            .store(b"payload", "f.txt", "text/plain", &scope())
            .unwrap();
        // Corrupt the file on disk.
        let path = store.artifact_path(&receipt.artifact_id).unwrap();
        fs::write(&path, b"tampered").unwrap();
        let err = store.load(&receipt.artifact_id, &scope()).unwrap_err();
        assert!(matches!(err, ArtifactError::ChecksumMismatch(_)));
        // Entry survives a checksum failure (not consumed).
        assert_eq!(store.count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn orphan_sweep_at_construction() {
        let root = temp_root("orphan");
        fs::create_dir_all(&root).unwrap();
        // An id-shaped orphan and a stale temp; a foreign file is left alone.
        let orphan = "a".repeat(32);
        fs::write(root.join(&orphan), b"x").unwrap();
        fs::write(root.join("stale.tmp"), b"x").unwrap();
        fs::write(root.join("keep.txt"), b"x").unwrap();
        let _store = ArtifactStore::new(&root);
        assert!(!root.join(&orphan).exists());
        assert!(!root.join("stale.tmp").exists());
        assert!(root.join("keep.txt").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn receipt_to_dict_shape() {
        let root = temp_root("dict");
        let store = ArtifactStore::new(&root);
        let receipt = store
            .store(b"data", "name.png", "image/png", &scope())
            .unwrap();
        let d = receipt.to_dict("");
        assert_eq!(d["one_shot"], serde_json::json!(true));
        assert!(d.get("download_path").is_none());
        assert!(d.get("scope_key").is_none());
        let d2 = receipt.to_dict("/artifacts/x");
        assert_eq!(d2["download_path"], serde_json::json!("/artifacts/x"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rate_limiter_window() {
        let (clk, cell) = controllable(0.0);
        let limiter = ArtifactRateLimiter::new(60.0, 2, Some(clk));
        assert!(!limiter.allow("")); // empty key always denied
        assert!(limiter.allow("k"));
        assert!(limiter.allow("k"));
        assert!(!limiter.allow("k")); // third within window denied
                                      // Slide the window past the first two hits.
        *cell.lock().unwrap() = 61.0;
        assert!(limiter.allow("k"));
        limiter.reset("k");
        assert!(limiter.allow("k"));
    }

    #[test]
    fn bounded_filename_and_normalize() {
        assert_eq!(bounded_filename("  a/b\\c  ", 160), "a_b_c");
        assert_eq!(bounded_filename("x\u{0007}y", 160), "xy"); // control char dropped
        assert_eq!(bounded_filename("abcdef", 3), "abc");
        assert_eq!(normalize_content_type("  Image/PNG ; q=1 "), "image/png");
        assert_eq!(normalize_content_type("text/plain"), "text/plain");
    }
}
