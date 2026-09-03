//! Gateway session stall notification policy (#72016 item 2).
//!
// Public API is ahead of its callers while the turn loop is ported.
#![allow(dead_code)]
//!
//! Port of `gateway/session_stall.py`. Owns only the notify-once policy for
//! "pending inbound + stale progress". It consumes a shared activity snapshot
//! as the single progress source and never invents a parallel progress clock
//! from turn-start or inbound-event timestamps.

use serde_json::Value;

/// True when a stall warning should be sent for this session.
pub fn should_emit_session_stall_notification(
    timeout_seconds: f64,
    idle_seconds: Option<f64>,
    has_pending_inbound: bool,
    already_notified: bool,
) -> bool {
    if timeout_seconds <= 0.0 {
        return false;
    }
    if !has_pending_inbound {
        return false;
    }
    if already_notified {
        return false;
    }
    match idle_seconds {
        None => false,
        Some(idle) => idle >= timeout_seconds,
    }
}

/// True when a prior stall notice may be cleared (the episode ended).
pub fn should_clear_session_stall_notification(
    timeout_seconds: f64,
    idle_seconds: Option<f64>,
    has_pending_inbound: bool,
) -> bool {
    if !has_pending_inbound {
        return true;
    }
    if timeout_seconds <= 0.0 {
        return true;
    }
    // Unknown progress: hold the latch. Do not treat observation gaps as recovery.
    match idle_seconds {
        None => false,
        Some(idle) => idle < timeout_seconds,
    }
}

/// User-facing stall warning (ASCII minutes; matches issue #72016 copy).
pub fn format_session_stall_notification(idle_seconds: f64) -> String {
    let mins = ((idle_seconds / 60.0).floor() as i64).max(1);
    format!("⚠️ Agent session appears stalled (last activity {mins} min ago). Try /new to reset.")
}

/// Idle seconds from a shared activity snapshot only (#72039 contract).
///
/// Prefers `seconds_since_activity` when present and finite; otherwise derives
/// from `last_activity_at` / `last_activity_ts`. Returns `None` when there is no
/// usable progress timestamp; callers must not fall back to turn-start or
/// pending-inbound clocks. `now` overrides the wall clock (seconds since epoch).
pub fn resolve_session_idle_seconds_from_activity(
    activity: Option<&Value>,
    now: Option<f64>,
) -> Option<f64> {
    let activity = match activity {
        Some(Value::Object(map)) if !map.is_empty() => map,
        // A null/absent/empty snapshot is no snapshot (matches Python `if not activity`).
        _ => return None,
    };

    // seconds_since_activity: a finite, non-bool number wins outright (clamped
    // at 0). Anything else (bool, unparseable, non-finite) falls through to the
    // timestamp keys.
    if let Some(idle) = activity.get("seconds_since_activity").and_then(finite_f64) {
        return Some(idle.max(0.0));
    }

    // last_activity_at, falling back to last_activity_ts only when the first is
    // absent or explicit null. A present bool yields None with no fallback.
    let ts = match activity.get("last_activity_at") {
        None | Some(Value::Null) => activity.get("last_activity_ts"),
        other => other,
    };
    let when = match ts {
        None | Some(Value::Null) => return None,
        Some(Value::Bool(_)) => return None,
        Some(v) => finite_f64(v)?,
    };

    let clock = now.unwrap_or_else(unix_now_seconds);
    let idle = clock - when;
    Some(if idle < 0.0 { 0.0 } else { idle })
}

/// Parse a JSON value as a finite f64, matching Python's `float(x)` for numbers
/// and numeric strings while treating booleans as not-a-number (the Python code
/// explicitly nulls bools before `float()`).
fn finite_f64(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Bool(_) => return None,
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

fn unix_now_seconds() -> f64 {
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
    fn emit_requires_pending_stale_and_unnotified() {
        assert!(should_emit_session_stall_notification(
            60.0,
            Some(90.0),
            true,
            false
        ));
        // not stale yet
        assert!(!should_emit_session_stall_notification(
            60.0,
            Some(30.0),
            true,
            false
        ));
        // no pending inbound
        assert!(!should_emit_session_stall_notification(
            60.0,
            Some(90.0),
            false,
            false
        ));
        // already notified
        assert!(!should_emit_session_stall_notification(
            60.0,
            Some(90.0),
            true,
            true
        ));
        // unknown idle
        assert!(!should_emit_session_stall_notification(
            60.0, None, true, false
        ));
        // disabled
        assert!(!should_emit_session_stall_notification(
            0.0,
            Some(90.0),
            true,
            false
        ));
    }

    #[test]
    fn clear_holds_latch_on_unknown_progress() {
        // episode ended: no pending inbound
        assert!(should_clear_session_stall_notification(60.0, None, false));
        // disabled clears
        assert!(should_clear_session_stall_notification(
            0.0,
            Some(90.0),
            true
        ));
        // unknown idle while pending: hold the latch
        assert!(!should_clear_session_stall_notification(60.0, None, true));
        // recovered
        assert!(should_clear_session_stall_notification(
            60.0,
            Some(30.0),
            true
        ));
        // still stalled
        assert!(!should_clear_session_stall_notification(
            60.0,
            Some(90.0),
            true
        ));
    }

    #[test]
    fn format_floors_to_at_least_one_minute() {
        assert!(format_session_stall_notification(30.0).contains("1 min"));
        assert!(format_session_stall_notification(125.0).contains("2 min"));
        assert!(format_session_stall_notification(0.0).contains("1 min"));
    }

    #[test]
    fn idle_prefers_seconds_since_activity() {
        let a = json!({"seconds_since_activity": 42.5});
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&a), None),
            Some(42.5)
        );
        // negative clamps to zero
        let a = json!({"seconds_since_activity": -5});
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&a), None),
            Some(0.0)
        );
        // numeric string is accepted
        let a = json!({"seconds_since_activity": "10"});
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&a), None),
            Some(10.0)
        );
    }

    #[test]
    fn idle_bool_seconds_falls_through_to_timestamp() {
        // bool seconds_since_activity is ignored; derive from last_activity_at.
        let a = json!({"seconds_since_activity": true, "last_activity_at": 100.0});
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&a), Some(160.0)),
            Some(60.0)
        );
    }

    #[test]
    fn idle_falls_back_to_last_activity_ts() {
        let a = json!({"last_activity_ts": 100.0});
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&a), Some(150.0)),
            Some(50.0)
        );
    }

    #[test]
    fn idle_none_cases() {
        assert_eq!(resolve_session_idle_seconds_from_activity(None, None), None);
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&json!({})), None),
            None
        );
        // present bool timestamp yields None with no fallback
        let a = json!({"last_activity_at": false, "last_activity_ts": 100.0});
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&a), Some(200.0)),
            None
        );
    }
}
