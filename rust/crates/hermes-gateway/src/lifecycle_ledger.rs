//! Port of gateway/lifecycle_ledger.py.
//!
// Public API is ahead of its callers (the boot/exit lifecycle wires it).
#![allow(dead_code)]
//!
//! Durable termination-reason evidence. A SIGKILL / OOM kill / VM death takes
//! the process out before any handler runs, so the next boot otherwise has no
//! record that the previous life died violently. This closes that gap with a
//! tiny state machine in `<HERMES_HOME>/state/gateway.lifecycle.json`:
//!
//!  * `record_startup` reads the previous life's sentinel: `phase == "running"`
//!    means it never reached an exit path -> unclean death. The finding (with
//!    the last heartbeat's memory sample) is appended to `gateway-exit-diag.log`
//!    and carried onto the new sentinel as `prior_unclean_exit` /
//!    `prior_suspected_oom` (which `memory_status` surfaces), then the sentinel
//!    is re-claimed `phase=running` for the new life.
//!  * `mark_exited` rewrites the sentinel `phase=exited` on a clean exit, but
//!    only when this process provably owns it (so a `--replace` takeover isn't
//!    clobbered).
//!
//! `sample_memory` is the cheap `/proc` snapshot the 30s heartbeat embeds.
//! Everything is best-effort: forensics must never affect the lifecycle it
//! observes.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{json, Map, Value};

// OOM-suspicion thresholds applied to the last heartbeat's memory sample.
const LOW_MEM_AVAILABLE_KIB: i64 = 64 * 1024;
const LOW_MEM_AVAILABLE_FRACTION: f64 = 0.05;

fn process_home() -> PathBuf {
    crate::config_file::hermes_home()
}

fn base_or(home: Option<&Path>) -> PathBuf {
    home.map(|h| h.to_path_buf()).unwrap_or_else(process_home)
}

/// `<HERMES_HOME>/state/gateway.lifecycle.json`.
pub fn get_lifecycle_sentinel_path(home: Option<&Path>) -> PathBuf {
    base_or(home).join("state").join("gateway.lifecycle.json")
}

/// `<HERMES_HOME>/state/gateway.heartbeat` (the loop-liveness heartbeat the
/// watchdog and `memory_status` read).
pub fn get_loop_heartbeat_path(home: Option<&Path>) -> PathBuf {
    base_or(home).join("state").join("gateway.heartbeat")
}

fn heartbeat_path(home: Option<&Path>) -> PathBuf {
    get_loop_heartbeat_path(home)
}

/// A process-relative monotonic clock in seconds (Python `time.monotonic`).
fn monotonic_secs() -> f64 {
    use std::sync::OnceLock;
    static BASE: OnceLock<std::time::Instant> = OnceLock::new();
    BASE.get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

/// Atomically rewrite the loop-liveness heartbeat file. `start_time` is the
/// gateway process start (wall epoch seconds) so supervisors can detect PID
/// reuse. Embeds a `sample_memory()` snapshot so the heartbeat doubles as a
/// rolling pre-death telemetry record. Best-effort; never raises.
pub fn write_loop_heartbeat(
    pid: Option<i64>,
    start_time: Option<f64>,
    home: Option<&Path>,
    extra: Option<Map<String, Value>>,
) -> PathBuf {
    let path = get_loop_heartbeat_path(home);
    let mut payload = Map::new();
    payload.insert(
        "pid".into(),
        json!(pid.unwrap_or(std::process::id() as i64)),
    );
    payload.insert("updated_at".into(), json!(Utc::now().to_rfc3339()));
    payload.insert("monotonic".into(), json!(monotonic_secs()));
    if let Some(st) = start_time {
        payload.insert("start_time".into(), json!(st));
    }
    let mem = sample_memory();
    if !mem.is_empty() {
        payload.insert("mem".into(), Value::Object(mem));
    }
    if let Some(extra) = extra {
        for (k, v) in extra {
            payload.insert(k, v);
        }
    }
    write_json_atomic(&path, &Value::Object(payload));
    path
}

fn write_json_atomic(path: &Path, payload: &Value) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let Ok(bytes) = serde_json::to_vec(payload) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn exit_diag_path(home: Option<&Path>) -> PathBuf {
    base_or(home).join("logs").join("gateway-exit-diag.log")
}

fn state_db_path(home: Option<&Path>) -> PathBuf {
    base_or(home).join("state.db")
}

/// Cheap memory snapshot: own RSS + system availability + swap (KiB). Pure
/// `/proc` reads, Linux-only (empty elsewhere), never raises.
pub fn sample_memory() -> Map<String, Value> {
    let mut sample = Map::new();
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    if let Some(kib) = rest
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<i64>().ok())
                    {
                        sample.insert("rss_kib".into(), json!(kib));
                    }
                    break;
                }
            }
        }
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            let mut vals: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
            for line in meminfo.lines() {
                let Some((key, rest)) = line.split_once(':') else {
                    continue;
                };
                if matches!(key, "MemTotal" | "MemAvailable" | "SwapTotal" | "SwapFree") {
                    if let Some(v) = rest
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<i64>().ok())
                    {
                        vals.insert(
                            match key {
                                "MemTotal" => "MemTotal",
                                "MemAvailable" => "MemAvailable",
                                "SwapTotal" => "SwapTotal",
                                _ => "SwapFree",
                            },
                            v,
                        );
                    }
                    if vals.len() == 4 {
                        break;
                    }
                }
            }
            if let Some(&t) = vals.get("MemTotal") {
                sample.insert("mem_total_kib".into(), json!(t));
            }
            if let Some(&a) = vals.get("MemAvailable") {
                sample.insert("mem_available_kib".into(), json!(a));
            }
            if let (Some(&st), Some(&sf)) = (vals.get("SwapTotal"), vals.get("SwapFree")) {
                sample.insert("swap_used_kib".into(), json!(st - sf));
            }
        }
    }
    sample
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v @ Value::Object(_)) => Some(v),
        _ => None,
    }
}

fn write_sentinel(payload: &Value, home: Option<&Path>) {
    let path = get_lifecycle_sentinel_path(home);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let Ok(bytes) = serde_json::to_vec(payload) else {
        return;
    };
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn append_exit_diag(record: &Value, home: Option<&Path>) {
    use std::io::Write;
    let path = exit_diag_path(home);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{record}");
    }
}

/// True when `pid` is a live process matching `start_time` (±2s). Guards the
/// `--replace` takeover race (a live matching owner is a planned handover, not
/// a death).
///
/// Note: the sentinel's `start_time` is a wall-clock epoch (`time.time()` in
/// Python), which is a different unit from `status::get_process_start_time`
/// (`/proc` ticks). This mirrors the Python exactly; the comparison only runs
/// when the PID is still alive (the rare takeover case), and a dead PID short-
/// circuits to `false` before it.
fn pid_alive_with_start_time(pid: Option<i64>, start_time: Option<f64>) -> bool {
    let Some(pid_int) = pid else { return false };
    if pid_int <= 0 {
        return false;
    }
    if !crate::status::pid_exists(pid_int) {
        return false;
    }
    let Some(start_time) = start_time else {
        return true; // alive; cannot disambiguate PID reuse -> err on "alive"
    };
    match crate::status::get_process_start_time(pid_int) {
        None => true,
        Some(actual) => (actual as f64 - start_time).abs() <= 2.0,
    }
}

/// Inspect the previous life's sentinel; return evidence when it died
/// uncleanly, else `None`. Read-only.
pub fn detect_unclean_exit(home: Option<&Path>) -> Option<Map<String, Value>> {
    let sentinel = read_json(&get_lifecycle_sentinel_path(home))?;
    if sentinel.get("phase").and_then(Value::as_str) != Some("running") {
        return None;
    }
    let pid = sentinel.get("pid").and_then(Value::as_i64);
    let start_time = sentinel.get("start_time").and_then(Value::as_f64);
    if pid_alive_with_start_time(pid, start_time) {
        return None; // live owner -> planned takeover, not a death
    }

    let mut evidence = Map::new();
    evidence.insert(
        "prior_pid".into(),
        sentinel.get("pid").cloned().unwrap_or(Value::Null),
    );
    evidence.insert(
        "prior_started_at".into(),
        sentinel.get("started_at").cloned().unwrap_or(Value::Null),
    );
    evidence.insert(
        "prior_start_time".into(),
        sentinel.get("start_time").cloned().unwrap_or(Value::Null),
    );

    if let Some(hb) = read_json(&heartbeat_path(home)) {
        evidence.insert(
            "last_heartbeat_at".into(),
            hb.get("updated_at").cloned().unwrap_or(Value::Null),
        );
        if let Some(mem @ Value::Object(_)) = hb.get("mem") {
            evidence.insert("last_heartbeat_mem".into(), mem.clone());
            let avail = mem.get("mem_available_kib").and_then(Value::as_i64);
            let total = mem.get("mem_total_kib").and_then(Value::as_i64);
            if let Some(avail) = avail {
                let low = avail < LOW_MEM_AVAILABLE_KIB
                    || total
                        .filter(|&t| t > 0)
                        .map(|t| (avail as f64 / t as f64) < LOW_MEM_AVAILABLE_FRACTION)
                        .unwrap_or(false);
                if low {
                    evidence.insert("suspected_oom".into(), json!(true));
                }
            }
        }
    }
    Some(evidence)
}

/// `"ok"`, `"absent"`, or the first `quick_check` complaint. Called only after
/// an unclean death (a SIGKILL mid-WAL-checkpoint can tear the store). Never
/// raises.
pub fn check_state_db_integrity(home: Option<&Path>) -> String {
    let path = state_db_path(home);
    if !path.exists() {
        return "absent".to_string();
    }
    match rusqlite::Connection::open(&path) {
        Ok(conn) => {
            match conn.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0)) {
                Ok(s) => s,
                Err(e) => format!("check-failed: {e}"),
            }
        }
        Err(e) => format!("check-failed: {e}"),
    }
}

/// Boot-time entry point: report any unclean previous exit, then claim the
/// sentinel for the current life. Returns the evidence dict or `None`.
pub fn record_startup(home: Option<&Path>) -> Option<Map<String, Value>> {
    let evidence = detect_unclean_exit(home);

    if let Some(ev) = &evidence {
        let mut ev = ev.clone();
        let verdict = check_state_db_integrity(home);
        if verdict != "ok" && verdict != "absent" {
            tracing::error!(
                %verdict,
                "state.db FAILED integrity check after an unclean gateway exit; run `hermes doctor`"
            );
        }
        ev.insert("state_db_integrity".into(), json!(verdict));
        let mut record = Map::new();
        record.insert("ts".into(), json!(Utc::now().to_rfc3339()));
        record.insert("tag".into(), json!("gateway.previous_unclean_exit"));
        record.insert("pid".into(), json!(std::process::id()));
        for (k, v) in &ev {
            record.insert(k.clone(), v.clone());
        }
        append_exit_diag(&Value::Object(record), home);
        let suspected_oom = ev
            .get("suspected_oom")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        tracing::warn!(
            prior_pid = ?ev.get("prior_pid"),
            suspected_oom,
            "previous gateway life exited UNCLEANLY (no exit path ran: SIGKILL / OOM / VM death)"
        );
    }

    let mut claim = Map::new();
    claim.insert("phase".into(), json!("running"));
    claim.insert("pid".into(), json!(std::process::id()));
    claim.insert("start_time".into(), json!(now_epoch()));
    claim.insert("started_at".into(), json!(Utc::now().to_rfc3339()));
    if evidence.is_some() {
        claim.insert("prior_unclean_exit".into(), json!(true));
        if evidence
            .as_ref()
            .and_then(|e| e.get("suspected_oom"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            claim.insert("prior_suspected_oom".into(), json!(true));
        }
    }
    write_sentinel(&Value::Object(claim), home);
    evidence
}

/// Mark the current life as cleanly exited. Only rewrites the sentinel when it
/// is provably owned by this process. Idempotent, never raises.
pub fn mark_exited(exit_code: Option<i64>, reason: &str, home: Option<&Path>) {
    if let Some(sentinel) = read_json(&get_lifecycle_sentinel_path(home)) {
        let owner = sentinel.get("pid").and_then(Value::as_i64);
        if owner != Some(std::process::id() as i64) {
            return; // not ours (or unknown ownership) -> leave it alone
        }
    }
    let mut payload = Map::new();
    payload.insert("phase".into(), json!("exited"));
    payload.insert("pid".into(), json!(std::process::id()));
    payload.insert(
        "exit_code".into(),
        exit_code.map(|c| json!(c)).unwrap_or(Value::Null),
    );
    payload.insert("exit_reason".into(), json!(reason));
    payload.insert("exited_at".into(), json!(Utc::now().to_rfc3339()));
    write_sentinel(&Value::Object(payload), home);
}

/// Container-boot helper: `clean` / `unclean` / `unknown` summary of how the
/// profile's last gateway life ended. Read-only, exception-free.
pub fn read_prior_exit_label(profile_home: &Path) -> String {
    match read_json(&get_lifecycle_sentinel_path(Some(profile_home))) {
        Some(sentinel) => match sentinel.get("phase").and_then(Value::as_str) {
            Some("exited") => "clean".to_string(),
            // At container boot the old PID namespace is gone, so any "running"
            // sentinel is from a life that never exited cleanly.
            Some("running") => "unclean".to_string(),
            _ => "unknown".to_string(),
        },
        None => "unknown".to_string(),
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_lifecycle_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(p.join("state")).unwrap();
        p
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sample_memory_reads_proc() {
        let s = sample_memory();
        assert!(s.contains_key("rss_kib"));
        assert!(s.contains_key("mem_total_kib"));
        assert!(s.contains_key("mem_available_kib"));
    }

    #[test]
    fn record_then_mark_exited_roundtrip() {
        let home = temp_home("clean");
        // First boot: no prior sentinel -> no unclean evidence.
        assert!(record_startup(Some(&home)).is_none());
        let sentinel = read_json(&get_lifecycle_sentinel_path(Some(&home))).unwrap();
        assert_eq!(sentinel["phase"], json!("running"));
        assert_eq!(sentinel["pid"], json!(std::process::id()));
        // Clean exit (we own it) -> phase exited -> label "clean".
        mark_exited(Some(0), "graceful_shutdown", Some(&home));
        assert_eq!(read_prior_exit_label(&home), "clean");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn detects_unclean_prior_life() {
        let home = temp_home("unclean");
        // A prior sentinel stuck in "running" with a dead PID = unclean death.
        let sentinel = json!({
            "phase": "running",
            "pid": 2_000_000_000i64, // almost certainly dead
            "start_time": Value::Null,
            "started_at": "2026-09-04T00:00:00+00:00"
        });
        write_sentinel(&sentinel, Some(&home));
        // Seed a heartbeat with a low-memory sample -> suspected_oom.
        let hb = json!({
            "updated_at": "2026-09-04T00:00:30+00:00",
            "mem": {"mem_total_kib": 1_000_000, "mem_available_kib": 1000}
        });
        std::fs::write(heartbeat_path(Some(&home)), hb.to_string()).unwrap();

        let ev = detect_unclean_exit(Some(&home)).unwrap();
        assert_eq!(ev.get("prior_pid"), Some(&json!(2_000_000_000i64)));
        assert_eq!(ev.get("suspected_oom"), Some(&json!(true)));

        // record_startup carries the flags onto the new claim and writes an
        // exit-diag line.
        record_startup(Some(&home));
        let claim = read_json(&get_lifecycle_sentinel_path(Some(&home))).unwrap();
        assert_eq!(claim["phase"], json!("running"));
        assert_eq!(claim["prior_unclean_exit"], json!(true));
        assert_eq!(claim["prior_suspected_oom"], json!(true));
        assert_eq!(claim["pid"], json!(std::process::id()));
        assert!(exit_diag_path(Some(&home)).exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn mark_exited_leaves_foreign_sentinel_alone() {
        let home = temp_home("foreign");
        // A sentinel owned by another PID must not be clobbered.
        write_sentinel(&json!({"phase": "running", "pid": 424242}), Some(&home));
        mark_exited(Some(0), "graceful_shutdown", Some(&home));
        let after = read_json(&get_lifecycle_sentinel_path(Some(&home))).unwrap();
        assert_eq!(
            after["phase"],
            json!("running"),
            "foreign sentinel preserved"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn state_db_integrity_absent_and_ok() {
        let home = temp_home("db");
        assert_eq!(check_state_db_integrity(Some(&home)), "absent");
        // A valid empty SQLite db reports "ok".
        let conn = rusqlite::Connection::open(state_db_path(Some(&home))).unwrap();
        conn.execute_batch("CREATE TABLE t(x);").unwrap();
        drop(conn);
        assert_eq!(check_state_db_integrity(Some(&home)), "ok");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn read_prior_exit_label_unknown_without_sentinel() {
        let home = temp_home("noheader");
        assert_eq!(read_prior_exit_label(&home), "unknown");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn heartbeat_written_is_readable_by_memory_status() {
        let home = temp_home("hb");
        let path = write_loop_heartbeat(None, Some(now_epoch()), Some(&home), None);
        assert!(path.exists());
        let hb = read_json(&path).unwrap();
        assert_eq!(hb["pid"], json!(std::process::id() as i64));
        assert!(hb.get("mem").is_some(), "embeds a memory sample");
        assert!(hb.get("start_time").is_some());
        // The memory-status read side (a sibling module) consumes this file:
        // a just-written heartbeat is fresh, so it yields a pressure verdict.
        let st = crate::memory_status::collect_memory_status(Some(&home), None);
        assert!(["ok", "elevated", "critical"].contains(&st.pressure.as_str()));
        assert!(st.gateway_rss_mb.is_some());
        let _ = std::fs::remove_dir_all(&home);
    }
}
