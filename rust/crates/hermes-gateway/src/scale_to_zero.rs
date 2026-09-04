//! Port of the decision layer of gateway/scale_to_zero.py.
//!
// Public API is ahead of its callers (the idle watcher wires it).
#![allow(dead_code)]
//!
//! Scale-to-zero idle detection: the gateway-side decision to go dormant when a
//! hosted instance is relay-only and quiet. This is the pure/observable decision
//! layer (idle predicate, arming precondition, idle-timeout parsing, dashboard-
//! client liveness marker). The Fly Machines self-suspend ACTION (the flaps-
//! socket POST) is deployment I/O and lands with the deploy layer;
//! `self_suspend_available` (an env/socket capability check) is included.

use std::path::{Path, PathBuf};

use serde_json::Value;

const SCALE_TO_ZERO_ENV: &str = "HERMES_SCALE_TO_ZERO";
const DEFAULT_IDLE_TIMEOUT_MINUTES: f64 = 2.0;
const FLY_APP_NAME_ENV: &str = "FLY_APP_NAME";
const FLY_MACHINE_ID_ENV: &str = "FLY_MACHINE_ID";
const FLY_API_SOCKET: &str = "/.fly/api";
const TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];

fn env_truthy(name: &str) -> bool {
    let v = std::env::var(name).unwrap_or_default();
    TRUTHY.contains(&v.trim().to_lowercase().as_str())
}

/// Whether the per-instance Labs toggle (`HERMES_SCALE_TO_ZERO`) is on. Absent /
/// blank / falsey is disabled (fail-safe default off).
pub fn scale_to_zero_enabled() -> bool {
    env_truthy(SCALE_TO_ZERO_ENV)
}

/// Coerce `scale_to_zero.idle_timeout_minutes` to seconds. Degrades to the
/// default on any non-numeric / non-positive value; never returns <= 0.
pub fn parse_idle_timeout_seconds(cfg_value: Option<&Value>, default_minutes: f64) -> f64 {
    let minutes = cfg_value
        .and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|m| *m > 0.0)
        .unwrap_or(default_minutes);
    minutes * 60.0
}

/// True iff the only connected messaging platform is `relay`, or there is none
/// (a Chronos-only / no-platform agent). A directly-connected platform holds a
/// live socket and disarms the feature. Compared by lowercased platform name.
pub fn messaging_is_relay_only_or_absent<I, S>(platforms: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut names: std::collections::HashSet<String> = platforms
        .into_iter()
        .map(|p| p.as_ref().trim().to_lowercase())
        .collect();
    names.remove("relay");
    names.is_empty()
}

/// Whether to start the idle watcher at all: the Labs flag is on, messaging is
/// relay-only/absent, and a wakeUrl is registered (a suspended instance with no
/// reachable wake target is a black hole). Any unmet -> the watcher never starts.
pub fn should_arm(enabled: bool, relay_only_or_absent: bool, wake_url: Option<&str>) -> bool {
    enabled && relay_only_or_absent && wake_url.map(|u| !u.is_empty()).unwrap_or(false)
}

/// The idle predicate: no counted active work, no live background work, and no
/// inbound within the timeout window. Any active work keeps the gateway awake.
/// `active_work_count` is the BROAD aggregate (agent turns + cron + API runs);
/// a caller that cannot read a work source must fail AWAKE (a positive count).
pub fn is_idle(
    active_work_count: i64,
    seconds_since_last_inbound: f64,
    idle_timeout_seconds: f64,
    has_live_background_work: bool,
) -> bool {
    if active_work_count > 0 {
        return false;
    }
    if has_live_background_work {
        return false;
    }
    seconds_since_last_inbound >= idle_timeout_seconds
}

const DASHBOARD_CLIENT_HEARTBEAT_REL: &str = "state/dashboard_clients.heartbeat";

/// Path of the dashboard-client liveness marker under HERMES_HOME.
pub fn dashboard_client_heartbeat_path(hermes_home: Option<&Path>) -> PathBuf {
    let base = hermes_home
        .map(|h| h.to_path_buf())
        .unwrap_or_else(crate::config_file::hermes_home);
    base.join(DASHBOARD_CLIENT_HEARTBEAT_REL)
}

/// Mark "a dashboard client is attached right now" (touch the marker's mtime).
/// Best-effort, never raises.
pub fn touch_dashboard_client_heartbeat(path: Option<&Path>) -> bool {
    let owned;
    let p = match path {
        Some(p) => p,
        None => {
            owned = dashboard_client_heartbeat_path(None);
            &owned
        }
    };
    if let Some(parent) = p.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    // Create if absent, then bump mtime to now.
    if std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .is_err()
    {
        return false;
    }
    set_mtime_now(p)
}

#[cfg(unix)]
fn set_mtime_now(p: &Path) -> bool {
    // `utimensat(UTIME_NOW)` via libc; std has no stable set-mtime API.
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
    ];
    // SAFETY: c is a valid NUL-terminated path; times is a 2-element array.
    unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) == 0 }
}

#[cfg(not(unix))]
fn set_mtime_now(_p: &Path) -> bool {
    false
}

/// Epoch seconds a dashboard client last sent a WS frame, or `None` if never.
/// A missing marker is `None` (steady state on a box nobody has the dashboard
/// open on); an unreadable marker fails AWAKE (returns `now`). The mtime is
/// clamped to `now` so an NTP step-back can't push idle out.
pub fn dashboard_client_last_seen(path: Option<&Path>, now: Option<f64>) -> Option<f64> {
    let current = now.unwrap_or_else(now_epoch);
    let owned;
    let p = match path {
        Some(p) => p,
        None => {
            owned = dashboard_client_heartbeat_path(None);
            &owned
        }
    };
    match std::fs::metadata(p) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(current);
            Some(mtime.min(current))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(current), // unreadable -> fail awake
    }
}

/// Whether this process can suspend its own machine via the Fly flaps socket:
/// the Fly machine identity env is present AND the local API socket exists.
/// Off-Fly this is false (the platform owns the freeze).
pub fn self_suspend_available() -> bool {
    !std::env::var(FLY_APP_NAME_ENV)
        .unwrap_or_default()
        .trim()
        .is_empty()
        && !std::env::var(FLY_MACHINE_ID_ENV)
            .unwrap_or_default()
            .trim()
            .is_empty()
        && Path::new(FLY_API_SOCKET).exists()
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
    use serde_json::json;

    #[test]
    fn idle_timeout_parsing() {
        assert_eq!(parse_idle_timeout_seconds(Some(&json!(5)), 2.0), 300.0);
        assert_eq!(parse_idle_timeout_seconds(Some(&json!("3")), 2.0), 180.0);
        // Non-positive / garbage -> default.
        assert_eq!(parse_idle_timeout_seconds(Some(&json!(0)), 2.0), 120.0);
        assert_eq!(parse_idle_timeout_seconds(Some(&json!(-4)), 2.0), 120.0);
        assert_eq!(parse_idle_timeout_seconds(Some(&json!("x")), 2.0), 120.0);
        assert_eq!(parse_idle_timeout_seconds(None, 2.0), 120.0);
    }

    #[test]
    fn relay_only_or_absent() {
        assert!(messaging_is_relay_only_or_absent(Vec::<&str>::new()));
        assert!(messaging_is_relay_only_or_absent(["relay"]));
        assert!(messaging_is_relay_only_or_absent(["Relay"]));
        assert!(!messaging_is_relay_only_or_absent(["relay", "telegram"]));
        assert!(!messaging_is_relay_only_or_absent(["discord"]));
    }

    #[test]
    fn arming_requires_all_three() {
        assert!(should_arm(true, true, Some("https://wake")));
        assert!(!should_arm(false, true, Some("https://wake")));
        assert!(!should_arm(true, false, Some("https://wake")));
        assert!(!should_arm(true, true, None));
        assert!(!should_arm(true, true, Some("")));
    }

    #[test]
    fn idle_predicate() {
        // Idle: no work, past the window.
        assert!(is_idle(0, 200.0, 120.0, false));
        // Active work keeps it awake.
        assert!(!is_idle(1, 999.0, 120.0, false));
        // Background work keeps it awake.
        assert!(!is_idle(0, 999.0, 120.0, true));
        // Within the window -> not yet idle.
        assert!(!is_idle(0, 60.0, 120.0, false));
    }

    #[test]
    fn dashboard_heartbeat_touch_and_read() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "hermes_s2z_{}_{}",
            std::process::id(),
            now_epoch() as u64
        ));
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let path = dir.join("state/dashboard_clients.heartbeat");
        // Missing -> None.
        assert_eq!(dashboard_client_last_seen(Some(&path), Some(1000.0)), None);
        // Touch, then a recent last-seen appears (clamped to `now`).
        assert!(touch_dashboard_client_heartbeat(Some(&path)));
        let seen = dashboard_client_last_seen(Some(&path), None);
        assert!(seen.is_some());
        // Clamp: a far-future `now` still returns the (older) real mtime.
        let clamped = dashboard_client_last_seen(Some(&path), Some(now_epoch() + 1e9)).unwrap();
        assert!(clamped <= now_epoch() + 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
