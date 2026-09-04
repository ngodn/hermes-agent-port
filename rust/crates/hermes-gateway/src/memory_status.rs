//! Port of gateway/memory_status.py.
//!
// Public API is ahead of its callers (the /api/status memory block wires it).
#![allow(dead_code)]
//!
//! Memory status rollup for `/api/status`. The gateway already produces every
//! memory-pressure signal a user would want (the loop heartbeat embeds an RSS +
//! MemAvailable/MemTotal + swap sample every 30s; the lifecycle sentinel records
//! an unclean/suspected-OOM previous death), but all of it dies in log files, so
//! a hosted agent can be OOM-killed hourly while its dashboard looks healthy.
//!
//! This is the read side: it distills the already-persisted heartbeat + sentinel
//! into a compact, public-safe block (coarse MB numbers, enums, booleans) with
//! no new sampling and no IPC, just two small file reads. Best-effort and read-
//! only: a missing/corrupt file degrades to `pressure="unknown"`, never raises.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

// Pressure thresholds on system MemAvailable. `critical` mirrors the lifecycle
// ledger's OOM-suspicion heuristics: if a memory level would make a subsequent
// unclean death "suspected OOM", the user should already have been warned at
// that level while the process was still alive.
const CRITICAL_AVAILABLE_KIB: i64 = 64 * 1024; // < 64 MiB available
const CRITICAL_AVAILABLE_FRACTION: f64 = 0.05; // < 5% of MemTotal
const ELEVATED_AVAILABLE_KIB: i64 = 128 * 1024; // < 128 MiB available
const ELEVATED_AVAILABLE_FRACTION: f64 = 0.15; // < 15% of MemTotal

// A heartbeat older than this no longer describes the present. The writer
// cadence is 30s; 150s of slack tolerates a briefly stalled loop without letting
// a long-dead gateway's last sample masquerade as current pressure.
const HEARTBEAT_FRESH_TTL_S: f64 = 150.0;

const KIB_PER_MB: i64 = 1024;

/// The `memory` block for `/api/status`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryStatus {
    pub pressure: String,
    pub gateway_rss_mb: Option<i64>,
    pub system_total_mb: Option<i64>,
    pub system_available_mb: Option<i64>,
    pub swap_used_mb: Option<i64>,
    pub sampled_at: Option<String>,
    pub last_boot_unclean: bool,
    pub last_boot_suspected_oom: bool,
    /// Identity of the CURRENT gateway life (the sentinel's `started_at`). A
    /// suspected-OOM restart writes a fresh sentinel, so this changes on every
    /// boot; the dashboard keys banner dismissal on it so acknowledging one OOM
    /// restart does not mute the NEXT one (the hourly-restart-loop case).
    pub boot_id: Option<String>,
}

impl MemoryStatus {
    fn unknown() -> Self {
        Self {
            pressure: "unknown".to_string(),
            gateway_rss_mb: None,
            system_total_mb: None,
            system_available_mb: None,
            swap_used_mb: None,
            sampled_at: None,
            last_boot_unclean: false,
            last_boot_suspected_oom: false,
            boot_id: None,
        }
    }
}

/// A non-negative integer KiB value, or `None` (rejecting bools, floats, and
/// negatives exactly like the Python `_mb` guard before the `// 1024`).
fn coerce_kib(value: Option<&Value>) -> Option<i64> {
    let v = value?;
    // Reject floats (a float rss is "not int" in Python) and bools (Value::Bool
    // is not a number). Accept only integer numbers >= 0.
    if v.is_f64() {
        return None;
    }
    let n = v.as_i64()?;
    if n < 0 {
        None
    } else {
        Some(n)
    }
}

fn kib_to_mb(value: Option<&Value>) -> Option<i64> {
    coerce_kib(value).map(|kib| kib / KIB_PER_MB)
}

/// Parse an ISO/RFC3339 timestamp; a naive value is assumed UTC (Python
/// replaces a missing tzinfo with UTC).
fn parse_iso(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = value?.as_str()?;
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // Naive "YYYY-MM-DDTHH:MM:SS[.ffffff]" -> assume UTC.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(DateTime::from_naive_utc_and_offset(naive, Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(DateTime::from_naive_utc_and_offset(naive, Utc));
    }
    None
}

/// Map a MemAvailable/MemTotal pair to `ok`/`elevated`/`critical`, or `unknown`
/// when the available sample is missing/malformed. The caller must not read "we
/// could not read it" as "memory is fine".
pub fn classify_pressure(available_kib: Option<i64>, total_kib: Option<i64>) -> String {
    let Some(available) = available_kib else {
        return "unknown".to_string();
    };
    if available < 0 {
        return "unknown".to_string();
    }
    let fraction = match total_kib {
        Some(total) if total > 0 => Some(available as f64 / total as f64),
        _ => None,
    };
    if available < CRITICAL_AVAILABLE_KIB
        || fraction.is_some_and(|f| f < CRITICAL_AVAILABLE_FRACTION)
    {
        return "critical".to_string();
    }
    if available < ELEVATED_AVAILABLE_KIB
        || fraction.is_some_and(|f| f < ELEVATED_AVAILABLE_FRACTION)
    {
        return "elevated".to_string();
    }
    "ok".to_string()
}

fn state_dir(home: Option<&Path>) -> PathBuf {
    let base = match home {
        Some(h) => h.to_path_buf(),
        None => crate::config_file::hermes_home(),
    };
    base.join("state")
}

/// `<HERMES_HOME>/state/gateway.heartbeat`.
fn heartbeat_path(home: Option<&Path>) -> PathBuf {
    state_dir(home).join("gateway.heartbeat")
}

/// `<HERMES_HOME>/state/gateway.lifecycle.json`.
fn sentinel_path(home: Option<&Path>) -> PathBuf {
    state_dir(home).join("gateway.lifecycle.json")
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Python truthiness of a JSON value for `bool(sentinel.get(...))`.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Build the `memory` block. `home` scopes the read to a profile's HERMES_HOME;
/// `None` means the active profile. `now` is injectable for tests. Always
/// returns a value; a down gateway or corrupt files yield `pressure="unknown"`
/// with whatever fields could still be recovered.
pub fn collect_memory_status(home: Option<&Path>, now: Option<DateTime<Utc>>) -> MemoryStatus {
    let moment = now.unwrap_or_else(Utc::now);
    let mut status = MemoryStatus::unknown();

    if let Some(heartbeat) = read_json(&heartbeat_path(home)) {
        let sampled_at = parse_iso(heartbeat.get("updated_at"));
        if let Some(Value::Object(mem)) = heartbeat.get("mem") {
            status.gateway_rss_mb = kib_to_mb(mem.get("rss_kib"));
            status.system_total_mb = kib_to_mb(mem.get("mem_total_kib"));
            status.system_available_mb = kib_to_mb(mem.get("mem_available_kib"));
            status.swap_used_mb = kib_to_mb(mem.get("swap_used_kib"));
            if let Some(sampled_at) = sampled_at {
                status.sampled_at = Some(sampled_at.to_rfc3339());
                let age_s = (moment - sampled_at).num_milliseconds() as f64 / 1000.0;
                if (0.0..=HEARTBEAT_FRESH_TTL_S).contains(&age_s) {
                    status.pressure = classify_pressure(
                        coerce_kib(mem.get("mem_available_kib")),
                        coerce_kib(mem.get("mem_total_kib")),
                    );
                }
                // else: stale sample. Numbers are still reported (honest about
                // *when* via sampled_at) but pressure stays "unknown" so a dead
                // gateway's final gasp cannot render a live "critical" forever.
            }
        }
    }

    if let Some(sentinel) = read_json(&sentinel_path(home)) {
        status.last_boot_unclean = truthy(sentinel.get("prior_unclean_exit"));
        status.last_boot_suspected_oom = truthy(sentinel.get("prior_suspected_oom"));
        if let Some(started_at) = sentinel.get("started_at").and_then(Value::as_str) {
            if !started_at.is_empty() {
                status.boot_id = Some(started_at.to_string());
            }
        }
    }

    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_home(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_memstatus_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(p.join("state")).unwrap();
        p
    }

    #[test]
    fn classify_thresholds() {
        // Plenty free.
        assert_eq!(
            classify_pressure(Some(4 * 1024 * 1024), Some(8 * 1024 * 1024)),
            "ok"
        );
        // < 64 MiB available -> critical on the absolute floor.
        assert_eq!(
            classify_pressure(Some(32 * 1024), Some(8 * 1024 * 1024)),
            "critical"
        );
        // 4% of total -> critical by fraction.
        assert_eq!(
            classify_pressure(Some(400 * 1024), Some(10 * 1024 * 1024)),
            "critical"
        );
        // 100 MiB available (< 128 MiB elevated floor) of 1 GiB: ~9.8% free, so
        // above the 5% critical fraction -> elevated via the absolute floor.
        // (Of 8 GiB the same 100 MiB would be < 5% -> critical.)
        assert_eq!(
            classify_pressure(Some(100 * 1024), Some(1024 * 1024)),
            "elevated"
        );
        // 10% of total -> elevated by fraction.
        assert_eq!(
            classify_pressure(Some(1024 * 1024), Some(10 * 1024 * 1024)),
            "elevated"
        );
        // Missing available -> unknown.
        assert_eq!(classify_pressure(None, Some(1024)), "unknown");
    }

    #[test]
    fn coerce_rejects_floats_and_negatives() {
        assert_eq!(coerce_kib(Some(&serde_json::json!(2048))), Some(2048));
        assert_eq!(coerce_kib(Some(&serde_json::json!(2048.5))), None);
        assert_eq!(coerce_kib(Some(&serde_json::json!(-1))), None);
        assert_eq!(coerce_kib(Some(&serde_json::json!(true))), None);
        assert_eq!(coerce_kib(None), None);
        assert_eq!(kib_to_mb(Some(&serde_json::json!(4096))), Some(4));
    }

    #[test]
    fn fresh_heartbeat_yields_pressure() {
        let home = temp_home("fresh");
        let sampled = Utc::now();
        let hb = serde_json::json!({
            "updated_at": sampled.to_rfc3339(),
            "mem": {
                "rss_kib": 500 * 1024,
                "mem_total_kib": 8 * 1024 * 1024,
                "mem_available_kib": 32 * 1024, // < 64 MiB -> critical
                "swap_used_kib": 0
            }
        });
        std::fs::write(home.join("state/gateway.heartbeat"), hb.to_string()).unwrap();

        let st = collect_memory_status(Some(&home), Some(sampled));
        assert_eq!(st.pressure, "critical");
        assert_eq!(st.gateway_rss_mb, Some(500));
        assert_eq!(st.system_total_mb, Some(8192));
        assert_eq!(st.system_available_mb, Some(32));
        assert!(st.sampled_at.is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn stale_heartbeat_reports_numbers_but_unknown_pressure() {
        let home = temp_home("stale");
        let sampled = Utc::now() - chrono::Duration::seconds(1000);
        let hb = serde_json::json!({
            "updated_at": sampled.to_rfc3339(),
            "mem": {"rss_kib": 100 * 1024, "mem_total_kib": 1024, "mem_available_kib": 1}
        });
        std::fs::write(home.join("state/gateway.heartbeat"), hb.to_string()).unwrap();

        let st = collect_memory_status(Some(&home), None);
        assert_eq!(st.pressure, "unknown");
        assert_eq!(st.gateway_rss_mb, Some(100));
        assert!(st.sampled_at.is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sentinel_flags_and_boot_id() {
        let home = temp_home("sentinel");
        let sent = serde_json::json!({
            "prior_unclean_exit": true,
            "prior_suspected_oom": true,
            "started_at": "2026-09-04T10:00:00+00:00"
        });
        std::fs::write(home.join("state/gateway.lifecycle.json"), sent.to_string()).unwrap();

        let st = collect_memory_status(Some(&home), None);
        assert!(st.last_boot_unclean);
        assert!(st.last_boot_suspected_oom);
        assert_eq!(st.boot_id.as_deref(), Some("2026-09-04T10:00:00+00:00"));
        // No heartbeat -> pressure unknown.
        assert_eq!(st.pressure, "unknown");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_files_are_unknown() {
        let home = temp_home("missing");
        let st = collect_memory_status(Some(&home), None);
        assert_eq!(st, MemoryStatus::unknown());
        let _ = std::fs::remove_dir_all(&home);
    }
}
