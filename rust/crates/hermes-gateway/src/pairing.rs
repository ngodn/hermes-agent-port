//! Port of gateway/pairing.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Code-based DM pairing: a `PairingStore` that manages per-platform pairing
//! codes and approved-user lists on disk under `$HERMES_HOME/platforms/pairing`
//! (with legacy `pairing/` fallback). This is the self-contained core: 8-char
//! codes from an unambiguous alphabet, salted SHA-256 code storage, 1-hour
//! expiry, per-user rate limiting, brute-force lockout, and atomic 0600 JSON
//! writes. The allowlist-mirror layer from the Python module (`_sync_allowlist_add`,
//! `_sync_allowlist_remove`, `_sync_live_adapter_allowlist_remove`,
//! `_iter_live_gateway_adapters`, `_adapter_platform_name`, `_purge_allowlist_entries`,
//! `_read_allowlist_env`, `_allowlist_env_for_platform`, `_split_allowlist`) is
//! left out here: it writes through `hermes_cli.config` and pokes live
//! `GatewayRunner` adapter snapshots, none of which are ported yet. The pairing
//! store is the authoritative grant record (the authz union honors it), so the
//! approve/revoke paths simply omit the best-effort mirror; it is deferred to
//! the runner / CLI-config port.
//!
//! Time is stored as epoch-second floats to match Python's `time.time()`
//! exactly (not RFC3339), since every timestamp comparison here is arithmetic
//! on those floats. Codes, salts, and request ids come from the kernel CSPRNG
//! (`/dev/urandom`, falling back to `getrandom(2)`), matching Python's
//! `secrets`; a code alphabet index uses rejection sampling to avoid modulo
//! bias. If the CSPRNG is unavailable the mint fails closed (returns `None`)
//! rather than emit a guessable, authorization-gating value.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config_file;
use crate::config_file::get_hermes_dir;
use crate::whatsapp_identity::{expand_whatsapp_aliases, normalize_whatsapp_identifier};

/// Unambiguous alphabet -- excludes 0/O, 1/I to prevent confusion.
pub const ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LENGTH: usize = 8;

/// Codes expire after 1 hour.
pub const CODE_TTL_SECONDS: f64 = 3600.0;
/// 1 request per user per 10 minutes.
pub const RATE_LIMIT_SECONDS: f64 = 600.0;
/// Lockout duration after too many failures.
pub const LOCKOUT_SECONDS: f64 = 3600.0;

/// Max pending codes per platform.
pub const MAX_PENDING_PER_PLATFORM: usize = 3;
/// Failed approvals before lockout.
pub const MAX_FAILED_ATTEMPTS: i64 = 5;

type Obj = Map<String, Value>;

// ----- time / randomness helpers -----

/// Seconds since the Unix epoch as a float, mirroring Python's `time.time()`.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Fill `buf` from the kernel CSPRNG. Pairing codes, salts, and request ids all
/// gate authorization, so they MUST come from a cryptographically secure source
/// (Python uses `secrets`), never a time/pid-seeded PRNG. Reads `/dev/urandom`,
/// falling back to the `getrandom(2)` syscall on Linux. Returns `false` when no
/// CSPRNG could be read, so callers fail closed (mint nothing) rather than emit
/// a guessable value.
fn fill_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return true;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut filled = 0usize;
        while filled < buf.len() {
            // SAFETY: writing into our own buffer for the remaining length.
            let rc = unsafe {
                libc::getrandom(
                    buf[filled..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - filled,
                    0,
                )
            };
            if rc > 0 {
                filled += rc as usize;
            } else if rc == 0 {
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
        }
        return filled == buf.len();
    }
    #[allow(unreachable_code)]
    false
}

/// `n` CSPRNG bytes, or `None` when the CSPRNG is unavailable.
fn rand_bytes(n: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; n];
    if fill_random(&mut out) {
        Some(out)
    } else {
        None
    }
}

/// A non-cryptographic 64-bit nonce for temp-file names only (uniqueness, not
/// security). Never used for a value that gates authorization.
fn nonce_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ c.wrapping_mul(0xD1B5_4A32_D192_ED03)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// `secrets.token_hex(n)` — `n` CSPRNG bytes, hex-encoded (2n chars). `None`
/// when the CSPRNG is unavailable.
fn token_hex(n: usize) -> Option<String> {
    rand_bytes(n).map(|b| to_hex(&b))
}

/// An 8-char code, each character drawn uniformly from [`ALPHABET`] via CSPRNG
/// bytes with rejection sampling (no modulo bias). `None` when the CSPRNG is
/// unavailable, so the caller mints nothing rather than a guessable code.
fn generate_code_string() -> Option<String> {
    let alpha: Vec<char> = ALPHABET.chars().collect();
    let n = alpha.len();
    if n == 0 || n > 256 {
        return None;
    }
    // Largest multiple of n that fits in a byte; reject values at or above it so
    // every alphabet index is equally likely. For a 32-char alphabet this is
    // 256, so nothing is ever rejected.
    let limit = (256 / n) * n;
    let mut code = String::with_capacity(CODE_LENGTH);
    let mut buf = [0u8; 1];
    let mut guard = 0usize;
    while code.len() < CODE_LENGTH {
        guard += 1;
        if guard > CODE_LENGTH * 256 {
            return None; // pathological CSPRNG stall
        }
        if !fill_random(&mut buf) {
            return None;
        }
        let v = buf[0] as usize;
        if v >= limit {
            continue;
        }
        code.push(alpha[v % n]);
    }
    Some(code)
}

/// SHA-256 of `salt || code`, hex-encoded. Mirrors `PairingStore._hash_code`.
fn hash_code(code: &str, salt: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(code.as_bytes());
    to_hex(&hasher.finalize())
}

/// Constant-time string comparison, mirroring `secrets.compare_digest`.
fn ct_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ----- identity helpers (mirror the module-level functions) -----

fn platform_uses_whatsapp_identity(platform: &str) -> bool {
    let p = platform.trim().to_lowercase();
    p == "whatsapp" || p == "whatsapp_cloud"
}

fn normalize_user_id(platform: &str, user_id: &str) -> String {
    let raw = user_id.trim();
    if platform_uses_whatsapp_identity(platform) {
        let normalized = normalize_whatsapp_identifier(raw);
        if normalized.is_empty() {
            raw.to_string()
        } else {
            normalized
        }
    } else {
        raw.to_string()
    }
}

fn user_id_aliases(platform: &str, user_id: &str) -> HashSet<String> {
    let raw = user_id.trim().to_string();
    let mut aliases = HashSet::new();
    if raw.is_empty() {
        return aliases;
    }
    aliases.insert(raw.clone());
    aliases.insert(normalize_user_id(platform, &raw));
    if platform_uses_whatsapp_identity(platform) {
        for alias in expand_whatsapp_aliases(&raw) {
            aliases.insert(alias);
        }
    }
    aliases.remove("");
    aliases
}

fn user_ids_match(platform: &str, left: &str, right: &str) -> bool {
    let l = user_id_aliases(platform, left);
    let r = user_id_aliases(platform, right);
    !l.is_empty() && !r.is_empty() && l.intersection(&r).next().is_some()
}

// ----- directory resolution -----

/// Resolve `path` for equality comparison. Canonicalizes when possible;
/// otherwise returns the path as-is (Python's `Path.resolve()` also works on
/// non-existent paths, and our constructed paths compare consistently).
fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The default (non-profile-scoped) pairing directory. Resolved fresh on every
/// call, matching `_default_pairing_dir`. (Python also honors a test-patched
/// `PAIRING_DIR` module global; Rust tests instead build a store on an explicit
/// temp dir, so that override hook is not needed.)
fn default_pairing_dir() -> PathBuf {
    get_hermes_dir("platforms/pairing", "pairing", None)
}

// ----- JSON file IO -----

/// Load a JSON file as an object. Non-existent, unreadable, malformed, or
/// non-object content all yield an empty object (defaults), never an error.
/// Mirrors the module-level `_load_json_file`.
fn load_json_object(path: &Path) -> Obj {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => Obj::new(),
        },
        Err(_) => Obj::new(),
    }
}

/// Write `data` to `path` with owner-only (0600) permissions via a temp file
/// and atomic rename, so readers always see a complete file. Mirrors
/// `_secure_write`. Write failures are swallowed (best-effort), matching the
/// port rule that persistence never panics.
fn secure_write(path: &Path, data: &str) {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if std::fs::create_dir_all(&parent).is_err() {
        return;
    }
    let tmp = parent.join(format!(
        ".pairing-{}-{}.tmp",
        std::process::id(),
        nonce_u64()
    ));
    let write_res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn dumps(obj: &Obj) -> String {
    // indent=2, ensure_ascii=False -- serde_json pretty printer matches.
    serde_json::to_string_pretty(&Value::Object(obj.clone())).unwrap_or_else(|_| "{}".to_string())
}

// ----- split-directory migration -----

/// Merge split legacy/new pairing data into the active directory. Active data
/// wins on key conflict; otherwise the inactive data is unioned in. Mirrors
/// `_merge_pairing_dir`.
fn merge_pairing_dir(active_dir: &Path, alternate_dir: &Path) {
    if !alternate_dir.exists() || resolve(active_dir) == resolve(alternate_dir) {
        return;
    }
    let _ = std::fs::create_dir_all(active_dir);
    let entries = match std::fs::read_dir(alternate_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let src = entry.path();
        if src.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if !src.is_file() {
            continue;
        }
        let name = match src.file_name() {
            Some(n) => n,
            None => continue,
        };
        let dest = active_dir.join(name);
        let mut merged = load_json_object(&src);
        if merged.is_empty() {
            continue;
        }
        let current = load_json_object(&dest);
        let before = current.clone();
        // Active data wins on key conflict; otherwise union the inactive data.
        for (k, v) in current {
            merged.insert(k, v);
        }
        if merged != before {
            secure_write(&dest, &dumps(&merged));
        }
    }
}

/// Heal installs whose pairing data ended up split across the legacy
/// (`{home}/pairing`) and new (`{home}/platforms/pairing`) directories.
/// Mirrors `_migrate_split_pairing_dirs`.
fn migrate_split_pairing_dirs(home: Option<&Path>, active: Option<&Path>) {
    let home = home
        .map(|p| p.to_path_buf())
        .unwrap_or_else(config_file::hermes_home);
    let old_dir = home.join("pairing");
    let new_dir = home.join("platforms").join("pairing");
    let active = active
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_pairing_dir);
    let alternate = if resolve(&active) == resolve(&old_dir) {
        new_dir
    } else {
        old_dir
    };
    merge_pairing_dir(&active, &alternate);
}

/// Manages pairing codes and approved user lists.
///
/// Data files per platform live in the store's directory:
///   - `{platform}-pending.json`  : pending pairing requests
///   - `{platform}-approved.json` : approved (paired) users
///   - `_rate_limits.json`        : rate-limit / lockout tracking
pub struct PairingStore {
    dir: PathBuf,
    // Protects all read-modify-write cycles. Python uses an RLock, but no
    // locked method here re-enters another locked method (the internal helpers
    // do not lock), so a plain Mutex is sufficient.
    lock: Mutex<()>,
    profile: Option<String>,
}

impl PairingStore {
    /// Construct a store. With `Some(non-empty)` profile, storage resolves from
    /// that profile's own home; otherwise the global pairing directory for the
    /// current `HERMES_HOME`. Mirrors `PairingStore.__init__`.
    pub fn new(profile: Option<&str>) -> Self {
        let use_profile = profile.map(|p| !p.is_empty()).unwrap_or(false);
        if use_profile {
            let name = profile.unwrap();
            let root = config_file::hermes_root(); // get_default_hermes_root
            let profile_home = if name == "default" {
                root
            } else {
                root.join("profiles").join(name)
            };
            let dir = get_hermes_dir("platforms/pairing", "pairing", Some(&profile_home));
            let _ = std::fs::create_dir_all(&dir);
            migrate_split_pairing_dirs(Some(&profile_home), Some(&dir));
            PairingStore {
                dir,
                lock: Mutex::new(()),
                profile: Some(name.to_string()),
            }
        } else {
            let dir = default_pairing_dir();
            let _ = std::fs::create_dir_all(&dir);
            migrate_split_pairing_dirs(None, None);
            PairingStore {
                dir,
                lock: Mutex::new(()),
                profile: profile.map(|s| s.to_string()),
            }
        }
    }

    /// Construct a store rooted directly at `dir`, skipping profile resolution
    /// and split-directory migration. Used by tests for isolation.
    fn from_dir(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        PairingStore {
            dir,
            lock: Mutex::new(()),
            profile: None,
        }
    }

    /// Profile name this store is scoped to, or `None` for the global store.
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    fn pending_path(&self, platform: &str) -> PathBuf {
        self.dir.join(format!("{platform}-pending.json"))
    }

    fn approved_path(&self, platform: &str) -> PathBuf {
        self.dir.join(format!("{platform}-approved.json"))
    }

    fn rate_limit_path(&self) -> PathBuf {
        self.dir.join("_rate_limits.json")
    }

    /// Load a per-platform JSON file. Non-object / malformed content yields an
    /// empty object. A permission error is surfaced as a warning (the classic
    /// Docker root-owned-0600-file symptom) before degrading to empty, matching
    /// the Python `_load_json` special case.
    fn load_json(&self, path: &Path) -> Obj {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(map)) => map,
                _ => Obj::new(),
            },
            Err(e) => {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    #[cfg(unix)]
                    let euid = unsafe { libc::geteuid() }.to_string();
                    #[cfg(not(unix))]
                    let euid = "n/a".to_string();
                    warn!(
                        "Pairing file {} exists but is not readable as uid={} ({}). \
                         If you ran `docker exec <container> hermes pairing approve ...` as root, \
                         re-run with `docker exec -u hermes <container> ...` and chown the file to \
                         the hermes user, or restart the container so the entrypoint fixes ownership.",
                        path.display(),
                        euid,
                        e,
                    );
                }
                Obj::new()
            }
        }
    }

    fn save_json(&self, path: &Path, data: &Obj) {
        secure_write(path, &dumps(data));
    }

    // ----- Approved users -----

    /// Check if a user is approved (paired) on a platform.
    pub fn is_approved(&self, platform: &str, user_id: &str) -> bool {
        let approved = self.load_json(&self.approved_path(platform));
        for approved_user_id in approved.keys() {
            if user_ids_match(platform, approved_user_id, user_id) {
                return true;
            }
        }
        false
    }

    /// List approved users, optionally filtered by platform. Each entry is an
    /// object with `platform`, `user_id`, and the stored info fields spread in.
    pub fn list_approved(&self, platform: Option<&str>) -> Vec<Value> {
        let mut results = Vec::new();
        let platforms = match platform {
            Some(p) => vec![p.to_string()],
            None => self.all_platforms("approved"),
        };
        for p in platforms {
            let approved = self.load_json(&self.approved_path(&p));
            for (uid, info) in approved {
                let mut entry = Obj::new();
                entry.insert("platform".to_string(), Value::String(p.clone()));
                entry.insert("user_id".to_string(), Value::String(uid));
                if let Value::Object(fields) = info {
                    for (k, v) in fields {
                        entry.insert(k, v);
                    }
                }
                results.push(Value::Object(entry));
            }
        }
        results
    }

    /// Add a user to the approved list. Must be called under the lock. Mirrors
    /// `_approve_user`, minus the allowlist mirror (see module doc).
    fn approve_user(&self, platform: &str, user_id: &str, user_name: &str) {
        let path = self.approved_path(platform);
        let mut approved = self.load_json(&path);
        let normalized_user_id = normalize_user_id(platform, user_id);
        let duplicate_ids: Vec<String> = approved
            .keys()
            .filter(|k| user_ids_match(platform, k, &normalized_user_id))
            .cloned()
            .collect();
        for k in duplicate_ids {
            approved.shift_remove(&k);
        }
        let mut record = Obj::new();
        record.insert(
            "user_name".to_string(),
            Value::String(user_name.to_string()),
        );
        record.insert(
            "approved_at".to_string(),
            Value::Number(serde_json::Number::from_f64(now()).unwrap_or_else(|| 0.into())),
        );
        approved.insert(normalized_user_id, Value::Object(record));
        self.save_json(&path, &approved);
        // NOTE: the operator-allowlist mirror (`_sync_allowlist_add`) is
        // deferred to the runner / CLI-config port; the pairing store grant is
        // authoritative on its own.
    }

    /// Remove a user from the approved list. Returns true if found. Mirrors
    /// `revoke`, minus the allowlist mirror (see module doc).
    pub fn revoke(&self, platform: &str, user_id: &str) -> bool {
        let path = self.approved_path(platform);
        let _guard = self.lock.lock().unwrap();
        let mut approved = self.load_json(&path);
        let matching_ids: Vec<String> = approved
            .keys()
            .filter(|k| user_ids_match(platform, k, user_id))
            .cloned()
            .collect();
        if !matching_ids.is_empty() {
            for k in matching_ids {
                approved.shift_remove(&k);
            }
            self.save_json(&path, &approved);
            // NOTE: `_sync_allowlist_remove` (env + live-adapter snapshot) is
            // deferred to the runner / CLI-config port.
            return true;
        }
        false
    }

    // ----- Pending codes -----

    /// Remove a pending request and approve its user. Must hold the lock.
    /// Mirrors `_finish_approval`.
    fn finish_approval(
        &self,
        platform: &str,
        pending: &mut Obj,
        matched_key: &str,
        matched_entry: &Obj,
    ) -> Value {
        pending.shift_remove(matched_key);
        self.save_json(&self.pending_path(platform), pending);

        // A successful approval proves the requester legitimate, so the
        // brute-force failure streak must not carry over.
        self.reset_failed_attempts(platform);

        let user_id = matched_entry
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let user_name = matched_entry
            .get("user_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.approve_user(platform, &user_id, &user_name);

        let mut out = Obj::new();
        out.insert("user_id".to_string(), Value::String(user_id));
        out.insert("user_name".to_string(), Value::String(user_name));
        Value::Object(out)
    }

    /// Generate a pairing code for a new user. Returns the code, or `None` if
    /// the user is rate-limited, max pending is reached, or the platform is
    /// locked out. Only a salted SHA-256 hash of the code is persisted.
    pub fn generate_code(&self, platform: &str, user_id: &str, user_name: &str) -> Option<String> {
        let _guard = self.lock.lock().unwrap();
        self.cleanup_expired(platform);
        let normalized_user_id = normalize_user_id(platform, user_id);

        if self.is_locked_out(platform) {
            return None;
        }
        if self.is_rate_limited(platform, user_id) {
            return None;
        }

        let mut pending = self.load_json(&self.pending_path(platform));
        if pending.len() >= MAX_PENDING_PER_PLATFORM {
            return None;
        }

        // Fail closed if the CSPRNG is unavailable: mint nothing rather than a
        // guessable code/salt/id (these gate authorization).
        let code = generate_code_string()?;
        let salt = rand_bytes(16)?;
        let code_hash = hash_code(&code, &salt);
        let entry_id = token_hex(8)?;

        let mut entry = Obj::new();
        entry.insert("hash".to_string(), Value::String(code_hash));
        entry.insert("salt".to_string(), Value::String(to_hex(&salt)));
        entry.insert("user_id".to_string(), Value::String(normalized_user_id));
        entry.insert(
            "user_name".to_string(),
            Value::String(user_name.to_string()),
        );
        entry.insert(
            "created_at".to_string(),
            Value::Number(serde_json::Number::from_f64(now()).unwrap_or_else(|| 0.into())),
        );
        pending.insert(entry_id, Value::Object(entry));
        self.save_json(&self.pending_path(platform), &pending);

        self.record_rate_limit(platform, user_id);

        Some(code)
    }

    /// Approve a pairing code. Returns `{user_id, user_name}` on success, or
    /// `None` if the code is invalid/expired OR the platform is locked out.
    /// Mirrors `approve_code`.
    pub fn approve_code(&self, platform: &str, code: &str) -> Option<Value> {
        let _guard = self.lock.lock().unwrap();
        self.cleanup_expired(platform);
        let code = code.to_uppercase();
        let code = code.trim();

        // Lockout check must run before the pending lookup so an already-issued
        // valid code cannot be accepted once the lockout fires.
        if self.is_locked_out(platform) {
            return None;
        }

        let mut pending = self.load_json(&self.pending_path(platform));

        let mut matched: Option<(String, Obj)> = None;
        for (entry_id, entry) in pending.iter() {
            let entry = match entry {
                Value::Object(m) => m,
                _ => continue,
            };
            if !entry.contains_key("salt") || !entry.contains_key("hash") {
                continue;
            }
            let salt_hex = match entry.get("salt").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let salt = match from_hex(salt_hex) {
                Some(s) => s,
                None => continue,
            };
            let stored_hash = match entry.get("hash").and_then(|v| v.as_str()) {
                Some(h) => h,
                None => continue,
            };
            let candidate = hash_code(code, &salt);
            if ct_eq(&candidate, stored_hash) {
                matched = Some((entry_id.clone(), entry.clone()));
                break;
            }
        }

        match matched {
            None => {
                self.record_failed_attempt(platform);
                None
            }
            Some((key, entry)) => Some(self.finish_approval(platform, &mut pending, &key, &entry)),
        }
    }

    /// True when `value` has the shape of a `list_pending` request id: 16
    /// lowercase-or-uppercase hex chars. Mirrors `looks_like_request_id`.
    pub fn looks_like_request_id(value: &str) -> bool {
        let value = value.trim();
        value.len() == 16 && value.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// Approve a pending request by its server-side request id (the admin-
    /// surface grant path). Does NOT count a miss toward lockout and is not
    /// gated by one. Mirrors `approve_request`.
    pub fn approve_request(&self, platform: &str, request_id: &str) -> Option<Value> {
        let _guard = self.lock.lock().unwrap();
        self.cleanup_expired(platform);
        let request_id = request_id.trim().to_lowercase();
        if request_id.is_empty() {
            return None;
        }

        let mut pending = self.load_json(&self.pending_path(platform));
        let mut matched: Option<(String, Obj)> = None;
        for (entry_id, entry) in pending.iter() {
            let entry = match entry {
                Value::Object(m) => m,
                _ => continue,
            };
            if !entry.contains_key("salt") || !entry.contains_key("hash") {
                continue;
            }
            if ct_eq(&entry_id.to_lowercase(), &request_id) {
                matched = Some((entry_id.clone(), entry.clone()));
                break;
            }
        }
        matched.map(|(key, entry)| self.finish_approval(platform, &mut pending, &key, &entry))
    }

    /// List pending pairing requests, optionally filtered by platform. Codes
    /// are stored hashed and never returned; each modern entry exposes a
    /// `request_id`. Mirrors `list_pending`.
    pub fn list_pending(&self, platform: Option<&str>) -> Vec<Value> {
        let mut results = Vec::new();
        let _guard = self.lock.lock().unwrap();
        let platforms = match platform {
            Some(p) => vec![p.to_string()],
            None => self.all_platforms("pending"),
        };
        for p in platforms {
            self.cleanup_expired(&p);
            let pending = self.load_json(&self.pending_path(&p));
            for (entry_id, info) in pending {
                let info = match info {
                    Value::Object(m) => m,
                    _ => continue,
                };
                let created_at = match info.get("created_at").and_then(|v| v.as_f64()) {
                    Some(c) => c,
                    None => continue,
                };
                let age_min = ((now() - created_at) / 60.0) as i64;
                let is_modern = info.get("hash").and_then(|v| v.as_str()).is_some()
                    && info.get("salt").and_then(|v| v.as_str()).is_some();
                let mut entry = Obj::new();
                entry.insert("platform".to_string(), Value::String(p.clone()));
                entry.insert(
                    "request_id".to_string(),
                    Value::String(if is_modern { entry_id } else { String::new() }),
                );
                entry.insert(
                    "user_id".to_string(),
                    info.get("user_id")
                        .cloned()
                        .unwrap_or(Value::String(String::new())),
                );
                entry.insert(
                    "user_name".to_string(),
                    info.get("user_name")
                        .cloned()
                        .unwrap_or(Value::String(String::new())),
                );
                entry.insert(
                    "age_minutes".to_string(),
                    Value::Number(serde_json::Number::from(age_min)),
                );
                results.push(Value::Object(entry));
            }
        }
        results
    }

    /// Clear all pending requests. Returns the count removed. Mirrors
    /// `clear_pending`.
    pub fn clear_pending(&self, platform: Option<&str>) -> usize {
        let _guard = self.lock.lock().unwrap();
        let mut count = 0;
        let platforms = match platform {
            Some(p) => vec![p.to_string()],
            None => self.all_platforms("pending"),
        };
        for p in platforms {
            let pending = self.load_json(&self.pending_path(&p));
            count += pending.len();
            self.save_json(&self.pending_path(&p), &Obj::new());
        }
        count
    }

    // ----- Rate limiting and lockout -----

    fn is_rate_limited(&self, platform: &str, user_id: &str) -> bool {
        let limits = self.load_json(&self.rate_limit_path());
        for alias in user_id_aliases(platform, user_id) {
            let key = format!("{platform}:{alias}");
            let last_request = limits.get(&key).and_then(|v| v.as_f64()).unwrap_or(0.0);
            if (now() - last_request) < RATE_LIMIT_SECONDS {
                return true;
            }
        }
        false
    }

    fn record_rate_limit(&self, platform: &str, user_id: &str) {
        let mut limits = self.load_json(&self.rate_limit_path());
        let now_val = now();
        for alias in user_id_aliases(platform, user_id) {
            let key = format!("{platform}:{alias}");
            limits.insert(
                key,
                Value::Number(serde_json::Number::from_f64(now_val).unwrap_or_else(|| 0.into())),
            );
        }
        self.save_json(&self.rate_limit_path(), &limits);
    }

    fn is_locked_out(&self, platform: &str) -> bool {
        let limits = self.load_json(&self.rate_limit_path());
        let lockout_key = format!("_lockout:{platform}");
        let lockout_until = limits
            .get(&lockout_key)
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        now() < lockout_until
    }

    fn record_failed_attempt(&self, platform: &str) {
        let mut limits = self.load_json(&self.rate_limit_path());
        let fail_key = format!("_failures:{platform}");
        let fails = limits
            .get(&fail_key)
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as i64
            + 1;
        limits.insert(
            fail_key.clone(),
            Value::Number(serde_json::Number::from(fails)),
        );
        if fails >= MAX_FAILED_ATTEMPTS {
            let lockout_key = format!("_lockout:{platform}");
            limits.insert(
                lockout_key,
                Value::Number(
                    serde_json::Number::from_f64(now() + LOCKOUT_SECONDS)
                        .unwrap_or_else(|| 0.into()),
                ),
            );
            limits.insert(fail_key, Value::Number(serde_json::Number::from(0)));
            println!(
                "[pairing] Platform {platform} locked out for {}s after {} failed attempts",
                LOCKOUT_SECONDS as i64, MAX_FAILED_ATTEMPTS
            );
        }
        self.save_json(&self.rate_limit_path(), &limits);
    }

    fn reset_failed_attempts(&self, platform: &str) {
        let mut limits = self.load_json(&self.rate_limit_path());
        let fail_key = format!("_failures:{platform}");
        let current = limits
            .get(&fail_key)
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if current != 0.0 {
            limits.insert(fail_key, Value::Number(serde_json::Number::from(0)));
            self.save_json(&self.rate_limit_path(), &limits);
        }
    }

    // ----- Cleanup -----

    /// Remove expired pending codes. Anything without a numeric `created_at`
    /// (malformed / legacy) is treated as expired. Mirrors `_cleanup_expired`.
    fn cleanup_expired(&self, platform: &str) {
        let path = self.pending_path(platform);
        let mut pending = self.load_json(&path);
        let now_val = now();
        let mut expired = Vec::new();
        for (entry_id, info) in pending.iter() {
            match info {
                Value::Object(m) => match m.get("created_at").and_then(|v| v.as_f64()) {
                    Some(created_at) => {
                        if (now_val - created_at) > CODE_TTL_SECONDS {
                            expired.push(entry_id.clone());
                        }
                    }
                    None => expired.push(entry_id.clone()),
                },
                _ => expired.push(entry_id.clone()),
            }
        }
        if !expired.is_empty() {
            for entry_id in expired {
                pending.shift_remove(&entry_id);
            }
            self.save_json(&path, &pending);
        }
    }

    /// List all platforms that have a data file of the given suffix (excluding
    /// the internal `_`-prefixed files). Mirrors `_all_platforms`.
    fn all_platforms(&self, suffix: &str) -> Vec<String> {
        let mut platforms = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return platforms,
        };
        let tail = format!("-{suffix}.json");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if let Some(stripped) = name.strip_suffix(&tail) {
                if !stripped.starts_with('_') {
                    platforms.push(stripped.to_string());
                }
            }
        }
        platforms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (PairingStore, PathBuf) {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "hermes_pairing_{}_{}_{}",
            tag,
            std::process::id(),
            nonce_u64()
        ));
        (PairingStore::from_dir(dir.clone()), dir)
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn code_generation_shape() {
        let (store, dir) = temp_store("shape");
        let code = store.generate_code("telegram", "user1", "Alice").unwrap();
        assert_eq!(code.len(), CODE_LENGTH);
        assert!(code.chars().all(|c| ALPHABET.contains(c)));
        // Pending file stores only a hash, never the plaintext code.
        let pending = store.load_json(&store.pending_path("telegram"));
        assert_eq!(pending.len(), 1);
        for (_, entry) in pending {
            let entry = entry.as_object().unwrap();
            assert!(entry.contains_key("hash"));
            assert!(entry.contains_key("salt"));
            assert!(!entry
                .get("hash")
                .unwrap()
                .as_str()
                .unwrap()
                .contains(code.as_str()));
        }
        cleanup(&dir);
    }

    #[test]
    fn store_roundtrip_approve() {
        let (store, dir) = temp_store("roundtrip");
        let code = store.generate_code("telegram", "user1", "Alice").unwrap();
        assert!(!store.is_approved("telegram", "user1"));
        let result = store.approve_code("telegram", &code).unwrap();
        assert_eq!(result.get("user_id").unwrap().as_str().unwrap(), "user1");
        assert!(store.is_approved("telegram", "user1"));
        // Pending consumed.
        assert!(store.list_pending(Some("telegram")).is_empty());
        // Listed.
        let approved = store.list_approved(Some("telegram"));
        assert_eq!(approved.len(), 1);
        assert_eq!(
            approved[0].get("user_id").unwrap().as_str().unwrap(),
            "user1"
        );
        cleanup(&dir);
    }

    #[test]
    fn approve_then_revoke() {
        let (store, dir) = temp_store("revoke");
        let code = store.generate_code("discord", "u42", "Bob").unwrap();
        store.approve_code("discord", &code).unwrap();
        assert!(store.is_approved("discord", "u42"));
        assert!(store.revoke("discord", "u42"));
        assert!(!store.is_approved("discord", "u42"));
        // Second revoke finds nothing.
        assert!(!store.revoke("discord", "u42"));
        cleanup(&dir);
    }

    #[test]
    fn wrong_code_rejected_and_case_insensitive() {
        let (store, dir) = temp_store("wrongcode");
        let code = store.generate_code("telegram", "user1", "Alice").unwrap();
        assert!(store.approve_code("telegram", "NOTITRIGHT").is_none());
        // Lowercase + surrounding whitespace still approves.
        let padded = format!("  {}  ", code.to_lowercase());
        assert!(store.approve_code("telegram", &padded).is_some());
        cleanup(&dir);
    }

    #[test]
    fn expiry_removes_stale_pending() {
        let (store, dir) = temp_store("expiry");
        let code = store.generate_code("telegram", "user1", "Alice").unwrap();
        // Rewrite created_at to be older than the TTL.
        let path = store.pending_path("telegram");
        let mut pending = store.load_json(&path);
        let keys: Vec<String> = pending.keys().cloned().collect();
        for k in keys {
            if let Some(Value::Object(entry)) = pending.get_mut(&k) {
                entry.insert(
                    "created_at".to_string(),
                    Value::Number(
                        serde_json::Number::from_f64(now() - CODE_TTL_SECONDS - 10.0).unwrap(),
                    ),
                );
            }
        }
        store.save_json(&path, &pending);
        // Expired entry is pruned and cannot be approved.
        assert!(store.approve_code("telegram", &code).is_none());
        assert!(store.list_pending(Some("telegram")).is_empty());
        cleanup(&dir);
    }

    #[test]
    fn rate_limit_blocks_second_request() {
        let (store, dir) = temp_store("ratelimit");
        assert!(store.generate_code("telegram", "user1", "").is_some());
        // Same user, immediately -> rate-limited.
        assert!(store.generate_code("telegram", "user1", "").is_none());
        cleanup(&dir);
    }

    #[test]
    fn max_pending_per_platform() {
        let (store, dir) = temp_store("maxpending");
        assert!(store.generate_code("telegram", "u1", "").is_some());
        assert!(store.generate_code("telegram", "u2", "").is_some());
        assert!(store.generate_code("telegram", "u3", "").is_some());
        // Fourth distinct user exceeds the cap.
        assert!(store.generate_code("telegram", "u4", "").is_none());
        cleanup(&dir);
    }

    #[test]
    fn lockout_after_failed_attempts() {
        let (store, dir) = temp_store("lockout");
        let code = store.generate_code("telegram", "user1", "Alice").unwrap();
        for _ in 0..MAX_FAILED_ATTEMPTS {
            assert!(store.approve_code("telegram", "ZZZZZZZZ").is_none());
        }
        assert!(store.is_locked_out("telegram"));
        // Even the valid code is refused while locked out.
        assert!(store.approve_code("telegram", &code).is_none());
        cleanup(&dir);
    }

    #[test]
    fn approve_request_by_id() {
        let (store, dir) = temp_store("reqid");
        store.generate_code("telegram", "user1", "Alice").unwrap();
        let pending = store.list_pending(Some("telegram"));
        assert_eq!(pending.len(), 1);
        let request_id = pending[0].get("request_id").unwrap().as_str().unwrap();
        assert!(PairingStore::looks_like_request_id(request_id));
        let result = store.approve_request("telegram", request_id).unwrap();
        assert_eq!(result.get("user_id").unwrap().as_str().unwrap(), "user1");
        assert!(store.is_approved("telegram", "user1"));
        // Unknown id yields None but does not lock out.
        assert!(store.approve_request("telegram", &"a".repeat(16)).is_none());
        assert!(!store.is_locked_out("telegram"));
        cleanup(&dir);
    }

    #[test]
    fn looks_like_request_id_shape() {
        assert!(PairingStore::looks_like_request_id("0123456789abcdef"));
        assert!(PairingStore::looks_like_request_id("  0123456789ABCDEF  "));
        assert!(!PairingStore::looks_like_request_id("ABCDEFGH")); // a code, not an id
        assert!(!PairingStore::looks_like_request_id("short"));
        assert!(!PairingStore::looks_like_request_id("0123456789abcdeg")); // non-hex
    }

    #[test]
    fn clear_pending_counts() {
        let (store, dir) = temp_store("clear");
        store.generate_code("telegram", "u1", "").unwrap();
        store.generate_code("telegram", "u2", "").unwrap();
        assert_eq!(store.clear_pending(Some("telegram")), 2);
        assert!(store.list_pending(Some("telegram")).is_empty());
        cleanup(&dir);
    }
}
