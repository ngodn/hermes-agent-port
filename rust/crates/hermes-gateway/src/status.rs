//! Port of the runtime-status + process-lifecycle core of gateway/status.py.
//!
// Public API is ahead of some callers while the lifecycle is wired.
#![allow(dead_code)]
//!
//! The gateway's process-singleton and status surface:
//!
//!  * `gateway_state.json` — the persisted runtime health record the dashboard
//!    and `/api/status` read: gateway_state, active_agents, per-platform health,
//!    session-store status, code identity, and the writer's PID fingerprint.
//!  * a PID file (`gateway.pid`) and an exclusive runtime `flock`
//!    (`gateway.lock`) so a second gateway cannot start on the same profile.
//!  * a respawn-storm breaker over an append-only starts log.
//!  * pure liveness derivations (`derive_gateway_busy` / `_drainable`, staleness
//!    and PID-liveness checks) shared by every read surface.
//!
//! PID liveness uses a no-kill `kill(pid, 0)` probe plus a `(pid, start_time)`
//! fingerprint (`/proc/<pid>/stat` field 22) so a recycled PID is never
//! mistaken for the original process. Every persistence op is best-effort.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};

const GATEWAY_KIND: &str = "hermes-gateway";
const RUNTIME_STATUS_FILE: &str = "gateway_state.json";
const PID_FILE: &str = "gateway.pid";
const GATEWAY_LOCK_FILENAME: &str = "gateway.lock";
const RUNTIME_STATUS_STALE_TTL_S: i64 = 120;
/// Reject epoch values before 2000-01-01: a Hermes heartbeat never legitimately
/// predates that, so an older value is corrupt/hand-edited.
const EPOCH_MIN_PLAUSIBLE: f64 = 946_684_800.0;

// ── paths ────────────────────────────────────────────────────────────────────

fn process_home() -> PathBuf {
    crate::config_file::hermes_home()
}

fn pid_path() -> PathBuf {
    process_home().join(PID_FILE)
}

fn runtime_status_path() -> PathBuf {
    process_home().join(RUNTIME_STATUS_FILE)
}

fn gateway_lock_path() -> PathBuf {
    process_home().join(GATEWAY_LOCK_FILENAME)
}

fn starts_log_path() -> PathBuf {
    process_home().join("gateway-starts.log")
}

// ── time helpers ─────────────────────────────────────────────────────────────

fn utc_now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)
}

/// Coerce a persisted `updated_at` value to an RFC3339 string or `None`
/// (string | null contract for every read surface).
pub fn normalize_updated_at(value: &Value) -> Option<String> {
    match value {
        Value::Bool(_) => None,
        Value::String(s) => {
            let mut raw = s.trim().to_string();
            if raw.ends_with('Z') || raw.ends_with('z') {
                raw.pop();
                raw.push_str("+00:00");
            }
            if let Ok(dt) = DateTime::parse_from_rfc3339(&raw) {
                return Some(dt.to_rfc3339());
            }
            // Naive -> assume UTC (Python fromisoformat + replace(tzinfo=utc)).
            for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
                if let Ok(naive) = NaiveDateTime::parse_from_str(&raw, fmt) {
                    return Some(Utc.from_utc_datetime(&naive).to_rfc3339());
                }
            }
            None
        }
        Value::Number(n) => {
            let seconds = n.as_f64()?;
            if !seconds.is_finite() {
                return None;
            }
            let now = Utc::now().timestamp() as f64;
            if seconds < EPOCH_MIN_PLAUSIBLE || seconds > now + 86400.0 {
                return None;
            }
            let secs = seconds.floor() as i64;
            let nanos = ((seconds - seconds.floor()) * 1e9) as u32;
            Utc.timestamp_opt(secs, nanos)
                .single()
                .map(|dt| dt.to_rfc3339())
        }
        _ => None,
    }
}

fn marker_is_stale(written_at: &str, ttl_s: i64) -> bool {
    match DateTime::parse_from_rfc3339(written_at) {
        Ok(dt) => {
            let age = (Utc::now() - dt.with_timezone(&Utc)).num_seconds();
            age > ttl_s
        }
        Err(_) => true,
    }
}

// ── process identity ─────────────────────────────────────────────────────────

/// A stable per-process start-time fingerprint (Linux: `/proc/<pid>/stat`
/// field 22, clock ticks since boot), or `None`.
pub fn get_process_start_time(pid: i64) -> Option<i64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22 (1-indexed) is start time. comm (field 2) may contain spaces
    // inside parentheses, so parse after the closing ')'.
    let close = text.rfind(')')?;
    let rest = text.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After ')' the next field is state (field 3), so start_time (22) is index
    // 22 - 3 = 19 into `fields`.
    fields.get(19)?.parse::<i64>().ok()
}

/// No-kill liveness probe: `kill(pid, 0)`. EPERM means the process exists but
/// we may not signal it; ESRCH means it is gone.
#[cfg(unix)]
pub fn pid_exists(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs no action, only error checking.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

#[cfg(not(unix))]
pub fn pid_exists(_pid: i64) -> bool {
    false
}

fn current_pid() -> i64 {
    std::process::id() as i64
}

fn code_identity_fields() -> Vec<(String, Value)> {
    let mut fields = vec![(
        "code_version".to_string(),
        Value::from(env!("CARGO_PKG_VERSION")),
    )];
    if let Ok(sha) = std::env::var("HERMES_CODE_SHA") {
        if !sha.trim().is_empty() {
            fields.push(("code_sha".to_string(), Value::from(sha)));
        }
    }
    fields
}

fn build_pid_record() -> Value {
    json!({
        "pid": current_pid(),
        "kind": GATEWAY_KIND,
        "argv": std::env::args().collect::<Vec<_>>(),
        "start_time": get_process_start_time(current_pid()),
        "hermes_home": process_home().to_string_lossy(),
    })
}

fn build_runtime_status_record() -> Value {
    let mut rec = build_pid_record();
    let obj = rec.as_object_mut().unwrap();
    obj.insert("gateway_state".into(), json!("starting"));
    obj.insert("exit_reason".into(), Value::Null);
    obj.insert("restart_requested".into(), json!(false));
    obj.insert("active_agents".into(), json!(0));
    obj.insert("platforms".into(), json!({}));
    obj.insert("session_store".into(), json!({"status": "unknown"}));
    obj.insert("updated_at".into(), json!(utc_now_iso()));
    for (k, v) in code_identity_fields() {
        obj.insert(k, v);
    }
    rec
}

// ── JSON file IO ─────────────────────────────────────────────────────────────

fn read_json_file(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => Some(v),
        _ => None,
    }
}

fn write_json_file(path: &Path, payload: &Value) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(payload) else {
        return;
    };
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// ── runtime status writer ────────────────────────────────────────────────────

/// A partial update to `gateway_state.json`. `None` fields are left unchanged
/// ("unset"); pass `Some(Value::Null)` to clear a per-platform field.
#[derive(Default)]
pub struct StatusUpdate {
    pub gateway_state: Option<Value>,
    pub exit_reason: Option<Value>,
    pub restart_requested: Option<bool>,
    pub active_agents: Option<Value>,
    pub served_profiles: Option<Vec<String>>,
    pub session_store: Option<Value>,
    pub clear_profile_platforms: bool,
    pub platform: Option<String>,
    pub platform_state: Option<Value>,
    pub error_code: Option<Value>,
    pub error_message: Option<Value>,
    pub needs_attention: Option<bool>,
    pub retrying_since: Option<Value>,
}

/// Persist gateway runtime health. Re-stamps the current writer's PID / code
/// identity every write, then applies the update's set fields.
pub fn write_runtime_status(update: &StatusUpdate) {
    write_runtime_status_to(&runtime_status_path(), update)
}

fn write_runtime_status_to(path: &Path, update: &StatusUpdate) {
    let mut payload = read_json_file(path).unwrap_or_else(build_runtime_status_record);
    let obj = payload.as_object_mut().unwrap();
    let current = build_pid_record();

    obj.entry("platforms").or_insert_with(|| json!({}));
    if update.clear_profile_platforms {
        if let Some(Value::Object(platforms)) = obj.get_mut("platforms") {
            platforms.retain(|k, _| !k.contains(':'));
        }
    }

    obj.insert("kind".into(), current["kind"].clone());
    obj.insert("pid".into(), current["pid"].clone());
    obj.insert("argv".into(), current["argv"].clone());
    obj.insert("start_time".into(), current["start_time"].clone());
    obj.insert("updated_at".into(), json!(utc_now_iso()));
    for (k, v) in code_identity_fields() {
        obj.insert(k, v);
    }

    if let Some(v) = &update.gateway_state {
        obj.insert("gateway_state".into(), v.clone());
    }
    if let Some(v) = &update.exit_reason {
        obj.insert("exit_reason".into(), v.clone());
    }
    if let Some(v) = update.restart_requested {
        obj.insert("restart_requested".into(), json!(v));
    }
    if let Some(v) = &update.active_agents {
        obj.insert("active_agents".into(), json!(parse_active_agents(v)));
    }
    if let Some(profiles) = &update.served_profiles {
        obj.insert("served_profiles".into(), json!(profiles));
    }
    if let Some(store) = &update.session_store {
        let state = store
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| matches!(*s, "ok" | "unavailable" | "retrying" | "unknown"))
            .unwrap_or("unknown");
        obj.insert("session_store".into(), json!({ "status": state }));
    }

    if let Some(platform) = &update.platform {
        let platforms = obj
            .get_mut("platforms")
            .and_then(Value::as_object_mut)
            .expect("platforms object");
        let entry = platforms
            .entry(platform.clone())
            .or_insert_with(|| json!({}));
        let pobj = entry.as_object_mut().unwrap();
        if let Some(v) = &update.platform_state {
            pobj.insert("state".into(), v.clone());
        }
        if let Some(v) = &update.error_code {
            pobj.insert("error_code".into(), v.clone());
        }
        if let Some(v) = &update.error_message {
            pobj.insert("error_message".into(), v.clone());
        }
        if let Some(v) = update.needs_attention {
            pobj.insert("needs_attention".into(), json!(v));
        }
        if let Some(v) = &update.retrying_since {
            pobj.insert("retrying_since".into(), v.clone());
        }
        pobj.insert("updated_at".into(), json!(utc_now_iso()));
        pobj.insert("writer_pid".into(), current["pid"].clone());
        pobj.insert("writer_start_time".into(), current["start_time"].clone());
    }

    write_json_file(path, &payload);
}

/// Read the persisted runtime health record (defaults to the active profile).
pub fn read_runtime_status(path: Option<&Path>) -> Option<Value> {
    match path {
        Some(p) => read_json_file(p),
        None => read_json_file(&runtime_status_path()),
    }
}

/// True when the snapshot is older than `ttl_s` (a missing/unparseable
/// timestamp is treated as stale).
pub fn runtime_status_is_stale(record: Option<&Value>, ttl_s: i64) -> bool {
    let Some(Value::Object(rec)) = record else {
        return true;
    };
    let updated_at = rec.get("updated_at").and_then(Value::as_str).unwrap_or("");
    marker_is_stale(updated_at, ttl_s)
}

/// True when the recorded PID is still alive (with the start-time PID-reuse
/// guard). `False` when the record has no usable PID.
pub fn runtime_status_pid_is_live(record: Option<&Value>) -> bool {
    let Some(pid) = pid_from_record(record) else {
        return false;
    };
    if !pid_exists(pid) {
        return false;
    }
    let recorded_start = record
        .and_then(|r| r.get("start_time"))
        .and_then(Value::as_i64);
    let current_start = get_process_start_time(pid);
    if let (Some(rec), Some(cur)) = (recorded_start, current_start) {
        if rec != cur {
            return false;
        }
    }
    true
}

fn pid_from_record(record: Option<&Value>) -> Option<i64> {
    record?.get("pid").and_then(Value::as_i64)
}

/// Coerce a persisted `active_agents` value to a clamped non-negative int.
pub fn parse_active_agents(raw: &Value) -> i64 {
    let n = match raw {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    n.unwrap_or(0).max(0)
}

/// Whether the gateway is actively processing in-flight turns.
pub fn derive_gateway_busy(
    gateway_running: bool,
    gateway_state: &Value,
    active_agents: &Value,
) -> bool {
    gateway_running
        && gateway_state.as_str() == Some("running")
        && parse_active_agents(active_agents) > 0
}

/// Whether the gateway can accept a begin-drain request right now.
pub fn derive_gateway_drainable(gateway_running: bool, gateway_state: &Value) -> bool {
    gateway_running && gateway_state.as_str() == Some("running")
}

// ── PID file ─────────────────────────────────────────────────────────────────

pub fn write_pid_file() {
    write_json_file(&pid_path(), &build_pid_record());
}

pub fn read_pid_record() -> Option<Value> {
    read_json_file(&pid_path())
}

pub fn remove_pid_file() {
    let _ = std::fs::remove_file(pid_path());
}

// ── runtime lock (flock) ─────────────────────────────────────────────────────

static LOCK_HANDLE: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Acquire the exclusive per-profile runtime lock (non-blocking). Returns
/// `true` when this process now owns it, `false` when another holds it.
#[cfg(unix)]
pub fn acquire_gateway_runtime_lock() -> bool {
    use std::os::unix::io::AsRawFd;

    let mut guard = LOCK_HANDLE.lock().unwrap();
    if guard.is_some() {
        return true; // already held
    }
    let path = gateway_lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    else {
        return false;
    };
    // SAFETY: flock on a valid open fd.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return false;
    }
    // Record the owner for diagnostics IN PLACE on the locked fd. It must NOT go
    // through write_json_file's atomic rename: a rename swaps the file's inode,
    // and the flock lives on the inode, so a rename would silently release our
    // exclusion and let a second gateway lock the fresh inode.
    write_locked_record_in_place(&mut file);
    *guard = Some(file);
    true
}

/// Write the owner record over the already-locked lock file, truncating in
/// place so the inode (and thus the flock) is preserved. Best-effort.
#[cfg(unix)]
fn write_locked_record_in_place(file: &mut std::fs::File) {
    use std::io::{Seek, SeekFrom, Write};
    if let Ok(bytes) = serde_json::to_vec(&build_pid_record()) {
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.write_all(&bytes);
        let _ = file.flush();
    }
}

#[cfg(not(unix))]
pub fn acquire_gateway_runtime_lock() -> bool {
    false
}

pub fn release_gateway_runtime_lock() {
    let mut guard = LOCK_HANDLE.lock().unwrap();
    // Dropping the File closes the fd, releasing the flock.
    *guard = None;
    let _ = std::fs::remove_file(gateway_lock_path());
}

pub fn owns_gateway_runtime_lock() -> bool {
    LOCK_HANDLE.lock().unwrap().is_some()
}

// ── respawn-storm breaker ────────────────────────────────────────────────────

/// The result of a respawn-storm check.
#[derive(Debug, Clone, PartialEq)]
pub struct StormInfo {
    pub count: usize,
    pub window_s: f64,
    pub backoff_s: f64,
}

/// Record this gateway start and report whether a respawn storm is underway.
/// Best-effort; a bookkeeping failure returns `None` (no storm).
pub fn record_start_and_check_storm(
    max_starts: usize,
    window_s: f64,
    backoff_cap_s: f64,
) -> Option<StormInfo> {
    record_start_and_check_storm_at(&starts_log_path(), max_starts, window_s, backoff_cap_s)
}

fn record_start_and_check_storm_at(
    path: &Path,
    max_starts: usize,
    window_s: f64,
    backoff_cap_s: f64,
) -> Option<StormInfo> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let now = Utc::now().timestamp() as f64 + (Utc::now().timestamp_subsec_micros() as f64 / 1e6);

    let mut existing: Vec<f64> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(ts) = line.parse::<f64>() {
                existing.push(ts);
            }
        }
    }
    existing.push(now);

    let recent: Vec<f64> = existing
        .iter()
        .copied()
        .filter(|ts| now - ts <= window_s)
        .collect();

    // Ring-buffer the persisted log so it stays bounded.
    let keep = (max_starts * 4).max(40);
    let to_write = if existing.len() > keep {
        &existing[existing.len() - keep..]
    } else {
        &existing[..]
    };
    let body = to_write
        .iter()
        .map(|ts| format!("{ts}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }

    if recent.len() > max_starts {
        let over = (recent.len() - max_starts).min(6);
        let backoff = backoff_cap_s.min(5.0 * 2f64.powi(over as i32));
        Some(StormInfo {
            count: recent.len(),
            window_s,
            backoff_s: backoff,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_status_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn normalize_updated_at_variants() {
        assert!(normalize_updated_at(&json!("2026-04-13T17:02:06+02:00")).is_some());
        assert!(normalize_updated_at(&json!("2026-04-13T17:02:06Z")).is_some());
        // Naive -> assumed UTC.
        let naive = normalize_updated_at(&json!("2026-04-13T17:02:06")).unwrap();
        assert!(naive.contains("+00:00"));
        // Plausible epoch.
        assert!(normalize_updated_at(&json!(1_776_085_326.0)).is_some());
        // Implausible / garbage.
        assert_eq!(normalize_updated_at(&json!(0)), None);
        assert_eq!(normalize_updated_at(&json!(true)), None);
        assert_eq!(normalize_updated_at(&json!("garbage")), None);
        assert_eq!(normalize_updated_at(&Value::Null), None);
    }

    #[test]
    fn parse_active_agents_clamps() {
        assert_eq!(parse_active_agents(&json!(3)), 3);
        assert_eq!(parse_active_agents(&json!(-4)), 0);
        assert_eq!(parse_active_agents(&json!("7")), 7);
        assert_eq!(parse_active_agents(&json!("nope")), 0);
        assert_eq!(parse_active_agents(&Value::Null), 0);
    }

    #[test]
    fn busy_and_drainable_contract() {
        assert!(derive_gateway_busy(true, &json!("running"), &json!(2)));
        assert!(!derive_gateway_busy(true, &json!("running"), &json!(0)));
        assert!(!derive_gateway_busy(false, &json!("running"), &json!(5)));
        assert!(!derive_gateway_busy(true, &json!("draining"), &json!(5)));
        assert!(derive_gateway_drainable(true, &json!("running")));
        assert!(!derive_gateway_drainable(true, &json!("draining")));
        assert!(!derive_gateway_drainable(false, &json!("running")));
    }

    #[test]
    fn write_read_runtime_status_roundtrip() {
        let dir = temp_dir("rt");
        let path = dir.join(RUNTIME_STATUS_FILE);
        write_runtime_status_to(
            &path,
            &StatusUpdate {
                gateway_state: Some(json!("running")),
                active_agents: Some(json!(2)),
                session_store: Some(json!({"status": "retrying"})),
                platform: Some("telegram".into()),
                platform_state: Some(json!("connected")),
                ..Default::default()
            },
        );
        let rec = read_runtime_status(Some(&path)).unwrap();
        assert_eq!(rec["gateway_state"], json!("running"));
        assert_eq!(rec["active_agents"], json!(2));
        assert_eq!(rec["session_store"]["status"], json!("retrying"));
        assert_eq!(rec["platforms"]["telegram"]["state"], json!("connected"));
        assert_eq!(rec["pid"], json!(current_pid()));
        assert!(rec["updated_at"].is_string());
        // A second write preserves the platform entry and updates state.
        write_runtime_status_to(
            &path,
            &StatusUpdate {
                gateway_state: Some(json!("draining")),
                ..Default::default()
            },
        );
        let rec2 = read_runtime_status(Some(&path)).unwrap();
        assert_eq!(rec2["gateway_state"], json!("draining"));
        assert_eq!(rec2["platforms"]["telegram"]["state"], json!("connected"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_profile_platforms_drops_scoped_entries() {
        let dir = temp_dir("clear");
        let path = dir.join(RUNTIME_STATUS_FILE);
        // Seed a process-level and a profile-scoped platform entry.
        write_runtime_status_to(
            &path,
            &StatusUpdate {
                platform: Some("telegram".into()),
                platform_state: Some(json!("ok")),
                ..Default::default()
            },
        );
        write_runtime_status_to(
            &path,
            &StatusUpdate {
                platform: Some("coder:telegram".into()),
                platform_state: Some(json!("ok")),
                ..Default::default()
            },
        );
        // A fresh process clears the scoped ("profile:platform") entries.
        write_runtime_status_to(
            &path,
            &StatusUpdate {
                clear_profile_platforms: true,
                ..Default::default()
            },
        );
        let rec = read_runtime_status(Some(&path)).unwrap();
        assert!(rec["platforms"].get("telegram").is_some());
        assert!(rec["platforms"].get("coder:telegram").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_liveness_of_self() {
        // This process is alive with a matching start_time.
        let rec = build_pid_record();
        assert!(runtime_status_pid_is_live(Some(&rec)));
        assert!(pid_exists(current_pid()));
        // A PID that is almost certainly dead.
        assert!(!pid_exists(2_000_000_000));
    }

    #[test]
    fn storm_breaker_trips_after_threshold() {
        let dir = temp_dir("storm");
        let log = dir.join("gateway-starts.log");
        // 3 starts allowed; the 4th within the window trips.
        assert!(record_start_and_check_storm_at(&log, 3, 120.0, 300.0).is_none());
        assert!(record_start_and_check_storm_at(&log, 3, 120.0, 300.0).is_none());
        assert!(record_start_and_check_storm_at(&log, 3, 120.0, 300.0).is_none());
        let storm = record_start_and_check_storm_at(&log, 3, 120.0, 300.0);
        assert!(storm.is_some());
        assert!(storm.unwrap().count >= 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_lock_is_exclusive_within_process() {
        // Acquire, confirm ownership, release. (Cross-process exclusion is a
        // kernel flock property; here we cover the state machine.)
        // Uses the real profile path, so only assert the ownership transitions.
        if acquire_gateway_runtime_lock() {
            assert!(owns_gateway_runtime_lock());
            // Re-acquire is idempotent.
            assert!(acquire_gateway_runtime_lock());
            release_gateway_runtime_lock();
            assert!(!owns_gateway_runtime_lock());
        }
    }
}
