//! External drain-control marker contract (dashboard -> gateway).
//!
// Public API is ahead of the drain watcher that consumes it.
#![allow(dead_code)]
//!
//! Port of `gateway/drain_control.py`. There is no HTTP control channel into a
//! running gateway, so begin/cancel-drain is communicated by a marker file the
//! dashboard writes and a gateway watcher reacts to. This module owns that
//! contract so writer and reader can never disagree.
//!
//! Marker: `$HERMES_HOME/.drain_request.json` with
//! `{action, requested_at(RFC3339 UTC), principal, epoch, suppress_notification}`.
//! Presence of a marker stamped with the *current* instantiation epoch and not
//! past its max age means "external drain active".
//!
//! The epoch (boot_id + PID 1 start time) makes "a deliberate machine restart
//! clears the drain" true by construction: HERMES_HOME can be a durable volume,
//! so a begin-drain marker can survive the very restart that ends the drain
//! (NS-570). The max-age bound (#85433) self-heals a same-epoch orphan whose
//! writer never cancelled. Both staleness checks are lenient: only a *definite*
//! mismatch/expiry is ignored; a legacy/corrupt marker still reads drain-active
//! (fail-safe toward quiescing). Reading never fails.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tracing::warn;

const DRAIN_REQUEST_FILENAME: &str = ".drain_request.json";

/// Max-age fallback for a same-epoch orphaned marker (#85433): drain-gated
/// actions finish in minutes, so an hour is comfortably past any legitimate
/// drain. Long drains refresh the marker via `write_drain_request`.
pub const DRAIN_REQUEST_MAX_AGE_SECONDS: f64 = 3600.0;

/// Identity of THIS container/VM instantiation: kernel boot id + PID 1 start
/// time. Stable for the life of PID-1 init (an s6 respawn of just the gateway
/// keeps it), changes when the machine/container is recreated. `""` when neither
/// source is readable (non-Linux / no /proc), which disables the epoch check.
pub fn current_instantiation_epoch() -> &'static str {
    static EPOCH: OnceLock<String> = OnceLock::new();
    EPOCH.get_or_init(|| {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let pid1_start = std::fs::read_to_string("/proc/1/stat")
            .ok()
            .and_then(|stat| {
                // comm (field 2) can contain spaces/parens: split on the last ')'.
                let tail = stat.rsplit_once(')').map(|(_, t)| t)?;
                // starttime is field 22 (1-indexed); tail starts at field 3 -> index 19.
                tail.split_whitespace().nth(19).map(str::to_string)
            })
            .unwrap_or_default();
        if boot_id.is_empty() && pid1_start.is_empty() {
            String::new()
        } else {
            format!("{boot_id}:{pid1_start}")
        }
    })
}

fn hermes_home_or(home: Option<&Path>) -> PathBuf {
    home.map(Path::to_path_buf)
        .unwrap_or_else(crate::config_file::hermes_home)
}

/// Absolute path to the drain-request marker.
pub fn drain_request_path(home: Option<&Path>) -> PathBuf {
    hermes_home_or(home).join(DRAIN_REQUEST_FILENAME)
}

/// Write the begin-drain marker (atomic). Idempotent: re-writing refreshes
/// `requested_at`, the sanctioned keep-alive for a long drain. Returns the
/// payload written.
pub fn write_drain_request(
    principal: &str,
    suppress_notification: bool,
    home: Option<&Path>,
) -> std::io::Result<Value> {
    let payload = json!({
        "action": "drain",
        "requested_at": Utc::now().to_rfc3339(),
        "principal": principal,
        "epoch": current_instantiation_epoch(),
        "suppress_notification": suppress_notification,
    });
    atomic_json_write(&drain_request_path(home), &payload)?;
    Ok(payload)
}

/// Remove the drain marker (cancel-drain). Returns true if one existed.
/// Best-effort: a missing file is not an error (cancel is idempotent).
pub fn clear_drain_request(home: Option<&Path>) -> bool {
    let path = drain_request_path(home);
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "drain-control: failed to remove marker");
            false
        }
    }
}

/// Return the marker payload, or `None` if absent. A present-but-unparseable
/// marker returns `Some({})` so presence is preserved. Never fails.
pub fn read_drain_request(home: Option<&Path>) -> Option<Value> {
    let path = drain_request_path(home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "drain-control: failed to read marker");
            return None;
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Some(Value::Object(map)),
        // Present but not an object (corrupt/contentless): empty dict.
        _ => Some(json!({})),
    }
}

/// True iff the marker's epoch is a *definite* mismatch with this process.
/// Lenient: an uncomputable current epoch or a marker with no epoch -> honour.
fn marker_epoch_is_stale(body: &Value) -> bool {
    let current = current_instantiation_epoch();
    if current.is_empty() {
        return false;
    }
    match body.get("epoch").and_then(Value::as_str) {
        Some(e) if !e.is_empty() => e != current,
        _ => false,
    }
}

/// True iff the marker's `requested_at` is *definitely* too old (#85433).
/// Lenient: missing/unparseable timestamp, or a future timestamp -> honour.
fn marker_is_expired(body: &Value) -> bool {
    let Some(raw) = body.get("requested_at").and_then(Value::as_str) else {
        return false;
    };
    let Ok(requested_at) = DateTime::parse_from_rfc3339(raw) else {
        return false;
    };
    let age = (Utc::now() - requested_at.with_timezone(&Utc)).num_seconds() as f64;
    if age <= DRAIN_REQUEST_MAX_AGE_SECONDS {
        return false;
    }
    let principal = body
        .get("principal")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    warn!(
        requested_at = raw,
        age_secs = age,
        max_secs = DRAIN_REQUEST_MAX_AGE_SECONDS,
        principal,
        "drain-control: ignoring expired drain marker; treating as stale"
    );
    true
}

/// True iff the marker is definitely from a drain that is already over: an
/// epoch mismatch (survived a machine restart) OR expiry (same-epoch orphan).
fn marker_is_stale(body: &Value) -> bool {
    marker_epoch_is_stale(body) || marker_is_expired(body)
}

/// True iff a begin-drain marker for THIS instantiation is present and fresh.
pub fn drain_requested(home: Option<&Path>) -> bool {
    match read_drain_request(home) {
        None => false,
        Some(body) => !marker_is_stale(&body),
    }
}

/// True iff an ACTIVE (present, current-epoch, unexpired) marker asks to
/// suppress the shutdown broadcast. A stale marker or a missing flag -> false.
pub fn drain_notification_suppressed(home: Option<&Path>) -> bool {
    match read_drain_request(home) {
        None => false,
        Some(body) if marker_is_stale(&body) => false,
        Some(body) => body
            .get("suppress_notification")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Atomic JSON write: temp file then rename, so a reader never sees a
/// half-written marker.
fn atomic_json_write(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_drain_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn write_then_read_then_clear() {
        let home = temp_home("rt");
        assert!(!drain_requested(Some(&home)));
        let payload = write_drain_request("dashboard", false, Some(&home)).unwrap();
        assert_eq!(payload["action"], "drain");
        assert!(drain_requested(Some(&home)));
        assert!(clear_drain_request(Some(&home)));
        assert!(!clear_drain_request(Some(&home))); // idempotent
        assert!(!drain_requested(Some(&home)));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn suppress_notification_flag() {
        let home = temp_home("suppress");
        write_drain_request("nas", true, Some(&home)).unwrap();
        assert!(drain_notification_suppressed(Some(&home)));
        clear_drain_request(Some(&home));
        write_drain_request("op", false, Some(&home)).unwrap();
        assert!(!drain_notification_suppressed(Some(&home)));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn corrupt_marker_reads_as_active() {
        let home = temp_home("corrupt");
        std::fs::write(home.join(DRAIN_REQUEST_FILENAME), "{not json").unwrap();
        // Fail-safe toward quiescing: a corrupt marker still means drain-active.
        assert!(drain_requested(Some(&home)));
        // But with no flag it does not suppress.
        assert!(!drain_notification_suppressed(Some(&home)));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn stale_epoch_marker_is_ignored() {
        // Only meaningful where an epoch can be computed (Linux /proc).
        if current_instantiation_epoch().is_empty() {
            return;
        }
        let home = temp_home("epoch");
        let body = json!({
            "action": "drain",
            "requested_at": Utc::now().to_rfc3339(),
            "epoch": "some-other-boot-id:12345",
            "suppress_notification": true,
        });
        atomic_json_write(&drain_request_path(Some(&home)), &body).unwrap();
        // Different epoch -> treated as absent (survived a restart).
        assert!(!drain_requested(Some(&home)));
        assert!(!drain_notification_suppressed(Some(&home)));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn expired_same_epoch_marker_is_ignored() {
        let home = temp_home("expired");
        let old = Utc::now() - chrono::Duration::seconds(DRAIN_REQUEST_MAX_AGE_SECONDS as i64 + 60);
        let body = json!({
            "action": "drain",
            "requested_at": old.to_rfc3339(),
            "epoch": current_instantiation_epoch(),
        });
        atomic_json_write(&drain_request_path(Some(&home)), &body).unwrap();
        assert!(!drain_requested(Some(&home)));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fresh_same_epoch_marker_is_active() {
        let home = temp_home("fresh");
        let body = json!({
            "action": "drain",
            "requested_at": Utc::now().to_rfc3339(),
            "epoch": current_instantiation_epoch(),
        });
        atomic_json_write(&drain_request_path(Some(&home)), &body).unwrap();
        assert!(drain_requested(Some(&home)));
        let _ = std::fs::remove_dir_all(&home);
    }
}
