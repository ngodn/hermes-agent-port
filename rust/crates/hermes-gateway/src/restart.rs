//! Port of gateway/restart.py.
//!
// Public API is ahead of its callers (the restart / shutdown-timing paths).
#![allow(dead_code)]
//!
//! Shared gateway restart constants, supervisor detection, and shutdown-timing
//! budgets. Pure logic plus a couple of environment probes; the config defaults
//! that Python pulls from `DEFAULT_CONFIG` are inlined here.

use serde_json::Value;

/// EX_TEMPFAIL: ask the service manager to restart after a graceful drain.
pub const GATEWAY_SERVICE_RESTART_EXIT_CODE: i32 = 75;
/// EX_CONFIG: fatal configuration error (the supervisor should stop restarting).
pub const GATEWAY_FATAL_CONFIG_EXIT_CODE: i32 = 78;

pub const EXTERNAL_GATEWAY_SUPERVISOR_ENV: &str = "HERMES_GATEWAY_EXTERNAL_SUPERVISOR";

// Defaults from DEFAULT_CONFIG (hermes_cli/config.py).
pub const DEFAULT_GATEWAY_RESTART_DRAIN_TIMEOUT: f64 = 0.0;
pub const DEFAULT_GATEWAY_SIGNAL_INTERRUPT_GRACE_TIMEOUT: f64 = 1.0;
pub const DEFAULT_GATEWAY_POST_INTERRUPT_GRACE_TIMEOUT: f64 = 5.0;
pub const DEFAULT_GATEWAY_RESTART_AFTER_TURN_TIMEOUT: f64 = 1800.0;
pub const DEFAULT_GATEWAY_CRON_DRAIN_TIMEOUT: f64 = 30.0;

/// Seconds of the watchdog leash held back for post-drain teardown.
pub const CRON_DRAIN_CLEANUP_RESERVE_S: f64 = 10.0;
/// systemd TimeoutStopSec headroom + floor. Keep in lockstep with the unit gen.
pub const SYSTEMD_STOP_HEADROOM_S: f64 = 30.0;
pub const SYSTEMD_TIMEOUT_STOP_SEC_FLOOR: f64 = 60.0;

/// True when an adapter's fatal error is a single-writer ownership conflict
/// (matched by error CODE only: `lock_conflict` or a `*_lock` family).
pub fn is_global_startup_conflict(error_code: Option<&str>) -> bool {
    let code = error_code.unwrap_or("").trim().to_lowercase();
    if code.is_empty() {
        return false;
    }
    code == "lock_conflict" || code.ends_with("_lock")
}

/// Whether this gateway process is owned by a supervisor (systemd INVOCATION_ID,
/// s6, launchd XPC, or the explicit external-supervisor env).
pub fn is_gateway_supervisor_process() -> bool {
    let get = |k: &str| std::env::var(k).unwrap_or_default();
    if !get("INVOCATION_ID").is_empty() {
        return true;
    }
    if !get("HERMES_S6_SUPERVISED_CHILD").is_empty() {
        return true;
    }
    let xpc = get("XPC_SERVICE_NAME");
    if !xpc.is_empty() && xpc != "0" {
        return true;
    }
    matches!(
        get(EXTERNAL_GATEWAY_SUPERVISOR_ENV)
            .trim()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether the gateway runs inside a container (restart routing must use the
/// exit-75 service path, since a detached setsid dies with the cgroup).
pub fn is_container_restart_context() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
}

// ── config-value coercion (Python float()/truthiness) ────────────────────────

fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `float(raw)` for a config value: numbers as-is, numeric strings
/// parsed, bools as 1.0/0.0; anything else `None`.
fn coerce_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// String form used by Python's `str(raw or "")` emptiness guard.
fn str_form(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        _ => String::new(),
    }
}

/// Parse a configured drain timeout, falling back to the shared default. Mirrors
/// `float(raw) if str(raw or "").strip() else DEFAULT` (a falsy `raw` — 0, "",
/// None — takes the default; the default is 0.0 anyway).
pub fn parse_restart_drain_timeout(raw: Option<&Value>) -> f64 {
    let guard = raw
        .map(|v| py_truthy(v) && !str_form(v).trim().is_empty())
        .unwrap_or(false);
    if !guard {
        return DEFAULT_GATEWAY_RESTART_DRAIN_TIMEOUT;
    }
    match raw.and_then(coerce_float) {
        Some(v) => v.max(0.0),
        None => DEFAULT_GATEWAY_RESTART_DRAIN_TIMEOUT,
    }
}

/// Parse the after-turn wait cap for in-band restart. `0` is a deliberate
/// disable and is kept; only None / blank string fall back to the default.
pub fn parse_restart_after_turn_timeout(raw: Option<&Value>) -> f64 {
    parse_kept_zero(raw, DEFAULT_GATEWAY_RESTART_AFTER_TURN_TIMEOUT)
}

/// Parse the cron-only drain floor. `0` is a deliberate opt-out and is kept;
/// only None / blank string fall back to the default.
pub fn parse_cron_drain_timeout(raw: Option<&Value>) -> f64 {
    parse_kept_zero(raw, DEFAULT_GATEWAY_CRON_DRAIN_TIMEOUT)
}

fn parse_kept_zero(raw: Option<&Value>, default: f64) -> f64 {
    match raw {
        None | Some(Value::Null) => return default,
        Some(Value::String(s)) if s.trim().is_empty() => return default,
        _ => {}
    }
    match raw.and_then(coerce_float) {
        Some(v) => v.max(0.0),
        None => default,
    }
}

/// Parse the unexpected-signal post-interrupt grace timeout.
pub fn parse_signal_interrupt_grace_timeout(raw: Option<&Value>) -> f64 {
    let value = match raw {
        None | Some(Value::Null) => return DEFAULT_GATEWAY_SIGNAL_INTERRUPT_GRACE_TIMEOUT,
        Some(Value::String(s)) if s.trim().is_empty() => {
            return DEFAULT_GATEWAY_SIGNAL_INTERRUPT_GRACE_TIMEOUT
        }
        Some(v) => coerce_float(v),
    };
    match value {
        Some(v) if v.is_finite() => v.max(0.0),
        _ => DEFAULT_GATEWAY_SIGNAL_INTERRUPT_GRACE_TIMEOUT,
    }
}

fn seconds(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// Seconds the shutdown drain may spend waiting on in-flight cron work: the
/// configured floor clamped to what this process can honour (never below
/// `drain_timeout`).
pub fn resolve_cron_drain_budget(
    drain_timeout: f64,
    cron_drain_timeout: f64,
    watchdog_delay: f64,
    elapsed: f64,
    cleanup_reserve_s: f64,
) -> f64 {
    let drain = seconds(drain_timeout);
    let floor = seconds(cron_drain_timeout);
    if floor <= 0.0 {
        return drain;
    }
    let reserve = if cleanup_reserve_s.is_finite() {
        cleanup_reserve_s.max(0.0)
    } else {
        CRON_DRAIN_CLEANUP_RESERVE_S
    };
    let ceiling = seconds(watchdog_delay) - seconds(elapsed) - reserve;
    drain.max(floor.min(ceiling))
}

/// Seconds systemd `TimeoutStopSec` must cover the full stop budget. A zero
/// `cron_drain_timeout` is a deliberate opt-out and does not extend the budget.
pub fn resolve_systemd_timeout_stop_sec(
    drain_timeout: f64,
    cron_drain_timeout: f64,
    cleanup_reserve_s: f64,
    headroom_s: f64,
    floor_s: f64,
) -> i64 {
    let drain = seconds(drain_timeout);
    let cron = seconds(cron_drain_timeout);
    let reserve = seconds(cleanup_reserve_s);
    let headroom = seconds(headroom_s);
    let floor = seconds(floor_s);
    let cron_budget = if cron > 0.0 { cron + reserve } else { 0.0 };
    let stop_budget = drain.max(cron_budget);
    floor.max(stop_budget + headroom) as i64
}

/// Seconds a CLI should wait for the gateway PID to exit after SIGUSR1 (covers
/// both the after-turn wait and the in-`stop()` drain, plus headroom).
pub fn resolve_restart_exit_wait_budget(
    drain_timeout: f64,
    after_turn_timeout: f64,
    headroom: f64,
) -> f64 {
    seconds(drain_timeout) + seconds(after_turn_timeout) + seconds(headroom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn startup_conflict_by_code() {
        assert!(is_global_startup_conflict(Some("lock_conflict")));
        assert!(is_global_startup_conflict(Some("telegram_lock")));
        assert!(is_global_startup_conflict(Some("  SLACK_LOCK ")));
        assert!(!is_global_startup_conflict(Some("rate_limited")));
        assert!(!is_global_startup_conflict(None));
        assert!(!is_global_startup_conflict(Some("")));
    }

    #[test]
    fn drain_timeout_parse() {
        assert_eq!(parse_restart_drain_timeout(None), 0.0);
        assert_eq!(parse_restart_drain_timeout(Some(&json!(5))), 5.0);
        assert_eq!(parse_restart_drain_timeout(Some(&json!("2.5"))), 2.5);
        assert_eq!(parse_restart_drain_timeout(Some(&json!("  "))), 0.0);
        assert_eq!(parse_restart_drain_timeout(Some(&json!(-3))), 0.0);
    }

    #[test]
    fn after_turn_and_cron_keep_zero() {
        // None / blank -> default; explicit 0 -> kept.
        assert_eq!(
            parse_restart_after_turn_timeout(None),
            DEFAULT_GATEWAY_RESTART_AFTER_TURN_TIMEOUT
        );
        assert_eq!(parse_restart_after_turn_timeout(Some(&json!(0))), 0.0);
        assert_eq!(parse_restart_after_turn_timeout(Some(&json!(60))), 60.0);
        assert_eq!(
            parse_cron_drain_timeout(Some(&json!(""))),
            DEFAULT_GATEWAY_CRON_DRAIN_TIMEOUT
        );
        assert_eq!(parse_cron_drain_timeout(Some(&json!(0))), 0.0);
    }

    #[test]
    fn signal_grace_parse() {
        assert_eq!(
            parse_signal_interrupt_grace_timeout(None),
            DEFAULT_GATEWAY_SIGNAL_INTERRUPT_GRACE_TIMEOUT
        );
        assert_eq!(parse_signal_interrupt_grace_timeout(Some(&json!(3))), 3.0);
        assert_eq!(parse_signal_interrupt_grace_timeout(Some(&json!(-1))), 0.0);
    }

    #[test]
    fn systemd_timeout_sizing() {
        // Default chat drain (0) + default cron (30) + reserve (10) + headroom
        // (30) = 70, above the 60 floor.
        let t = resolve_systemd_timeout_stop_sec(
            0.0,
            DEFAULT_GATEWAY_CRON_DRAIN_TIMEOUT,
            CRON_DRAIN_CLEANUP_RESERVE_S,
            SYSTEMD_STOP_HEADROOM_S,
            SYSTEMD_TIMEOUT_STOP_SEC_FLOOR,
        );
        assert_eq!(t, 70);
        // Cron opt-out (0) -> only drain + headroom, clamped to the floor.
        let t2 = resolve_systemd_timeout_stop_sec(0.0, 0.0, 10.0, 30.0, 60.0);
        assert_eq!(t2, 60);
        // A long drain dominates.
        let t3 = resolve_systemd_timeout_stop_sec(120.0, 30.0, 10.0, 30.0, 60.0);
        assert_eq!(t3, 150);
    }

    #[test]
    fn cron_budget_clamps_to_watchdog_leash() {
        // floor 30, watchdog 60, reserve 10 -> ceiling 50, so min(30,50)=30 >= drain 0.
        assert_eq!(resolve_cron_drain_budget(0.0, 30.0, 60.0, 0.0, 10.0), 30.0);
        // Tight leash: watchdog 25, reserve 10 -> ceiling 15, floor 30 -> min=15.
        assert_eq!(resolve_cron_drain_budget(0.0, 30.0, 25.0, 0.0, 10.0), 15.0);
        // A configured long drain is never shortened.
        assert_eq!(resolve_cron_drain_budget(45.0, 30.0, 25.0, 0.0, 10.0), 45.0);
        // Cron opt-out returns the drain.
        assert_eq!(resolve_cron_drain_budget(5.0, 0.0, 60.0, 0.0, 10.0), 5.0);
    }

    #[test]
    fn exit_wait_budget_sums_phases() {
        assert_eq!(resolve_restart_exit_wait_budget(10.0, 1800.0, 15.0), 1825.0);
        assert_eq!(resolve_restart_exit_wait_budget(-1.0, -2.0, -3.0), 0.0);
    }
}
