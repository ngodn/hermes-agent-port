//! SessionStore reset decisions from gateway/session.py. The store supplies
//! local wall-clock times and its background-process liveness result.
#![allow(dead_code)]

use crate::config_types::SessionResetPolicy;
use chrono::{Datelike, NaiveDateTime, Timelike};

/// Resolve the startup bridge plus auto_continue_freshness_window without
/// mutating process env. A present YAML setting replaces the env value even
/// when malformed; Python then falls back to one hour. Booleans and null are
/// stringified by the bridge and rejected by float(), rather than coerced.
pub fn configured_freshness_seconds(config: &serde_json::Value, env: Option<&str>) -> f64 {
    let parse = |text: &str| {
        crate::python_value::numeric_text(text).and_then(|text| text.parse::<f64>().ok())
    };
    match config
        .get("agent")
        .and_then(|agent| agent.get("gateway_auto_continue_freshness"))
    {
        Some(serde_json::Value::Number(number)) => number.as_f64(),
        Some(serde_json::Value::String(text)) => parse(text),
        Some(_) => None,
        None => env.and_then(parse),
    }
    .unwrap_or(3600.0)
}

/// Existing-route policy from get_or_create_session's unlocked checks.
/// Suspension wins first. Resume freshness is a separate gate after ordinary
/// reset policy, and mode=none disables that automatic expiry too.
pub fn existing_reset_reason(
    entry: &crate::session_entry::SessionEntry,
    policy: &SessionResetPolicy,
    now: NaiveDateTime,
    active_processes: bool,
    freshness_seconds: f64,
) -> anyhow::Result<Option<&'static str>> {
    use crate::python_value::truthy;
    if truthy(&entry.fields["suspended"]) {
        return Ok(Some("suspended"));
    }
    if !active_processes
        && ["idle", "daily", "both"]
            .iter()
            .any(|mode| policy.mode == *mode)
    {
        anyhow::ensure!(
            entry.updated_at.offset_micros.is_none(),
            "cannot compare aware and naive session timestamps"
        );
    }
    let reason = reset_reason(policy, entry.updated_at.local, now, active_processes)?;
    if reason.is_some() || !truthy(&entry.fields["resume_pending"]) || policy.mode == "none" {
        return Ok(reason);
    }
    if freshness_seconds > 0.0 {
        let marked = entry
            .fields
            .get("last_resume_marked_at")
            .filter(|value| truthy(value))
            .map(crate::session_entry::EntryTimestamp::parse)
            .transpose()?;
        let reference = marked.as_ref().unwrap_or(&entry.updated_at);
        anyhow::ensure!(
            reference.offset_micros.is_none(),
            "cannot subtract aware and naive session timestamps"
        );
        let elapsed = now.signed_duration_since(reference.local);
        let seconds =
            elapsed.num_seconds() as f64 + elapsed.subsec_nanos() as f64 / 1_000_000_000.0;
        if seconds > freshness_seconds {
            return Ok(Some("resume_pending_expired"));
        }
    }
    Ok(None)
}

pub type ProcessProbe<'a> = dyn FnMut(&str) -> anyhow::Result<bool> + 'a;

/// SessionStore's safe probe boundary. A missing registry does not pin sessions;
/// a failed registry check does, so uncertainty cannot discard active context.
pub fn active_processes_safe(session_key: &str, probe: Option<&mut ProcessProbe<'_>>) -> bool {
    match probe {
        None => false,
        Some(probe) => probe(session_key).unwrap_or_else(|_| {
            tracing::warn!("process liveness check failed; keeping session alive");
            true
        }),
    }
}

/// The process registry evaluates this against its refreshed running entries.
/// At the exact age threshold a process no longer pins the session. A future
/// started_at still counts, matching the source's wall-clock comparison.
pub fn process_pins_session(
    process_session: &str,
    session_key: &str,
    exited: bool,
    started_at: f64,
    now: f64,
    max_active_age: Option<f64>,
) -> bool {
    process_session == session_key
        && !exited
        && max_active_age.is_none_or(|limit| now - started_at < limit)
}

/// GatewayRunner derives the registry threshold from the default reset policy,
/// even when a platform/type override selects the session's actual reset policy.
pub fn process_age_limit(policy: &SessionResetPolicy) -> anyhow::Result<Option<f64>> {
    let value = &policy.bg_process_max_age_hours;
    if !crate::python_value::truthy(value) {
        return Ok(None);
    }
    let hours = value
        .as_f64()
        .or_else(|| value.as_bool().map(|v| if v { 1.0 } else { 0.0 }))
        .ok_or_else(|| anyhow::anyhow!("background process age must be numeric"))?;
    Ok(if hours > 0.0 {
        Some(hours * 3600.0)
    } else {
        None
    })
}

/// The source checks active processes before reading policy fields. Keep that
/// short circuit, and the strict deadline comparisons and idle-before-daily
/// precedence. Invalid policy values are errors, not permission to keep a session.
pub fn reset_reason(
    policy: &SessionResetPolicy,
    updated: NaiveDateTime,
    now: NaiveDateTime,
    active_processes: bool,
) -> anyhow::Result<Option<&'static str>> {
    if active_processes || policy.mode == "none" {
        return Ok(None);
    }
    anyhow::ensure!(
        !policy.mode.is_array() && !policy.mode.is_object(),
        "reset mode is not hashable"
    );
    if policy.mode == "idle" || policy.mode == "both" {
        let minutes = policy
            .idle_minutes
            .as_f64()
            .or_else(|| {
                policy
                    .idle_minutes
                    .as_bool()
                    .map(|v| if v { 1.0 } else { 0.0 })
            })
            .ok_or_else(|| anyhow::anyhow!("idle_minutes must be numeric"))?;
        let micros = (minutes * 60_000_000.0).round_ties_even();
        anyhow::ensure!(
            micros.is_finite() && (i64::MIN as f64..i64::MAX as f64).contains(&micros),
            "idle duration out of range"
        );
        let deadline = updated
            .checked_add_signed(chrono::TimeDelta::microseconds(micros as i64))
            .ok_or_else(|| anyhow::anyhow!("idle deadline out of range"))?;
        anyhow::ensure!(
            (1..=9999).contains(&deadline.year()),
            "idle deadline outside Python datetime range"
        );
        if now > deadline {
            return Ok(Some("idle"));
        }
    }
    if policy.mode == "daily" || policy.mode == "both" {
        let hour = policy
            .at_hour
            .as_u64()
            .or_else(|| policy.at_hour.as_bool().map(u64::from))
            .filter(|hour| *hour < 24)
            .ok_or_else(|| anyhow::anyhow!("at_hour must be an integer from 0 to 23"))?
            as u32;
        let mut boundary = now.date().and_hms_opt(hour, 0, 0).unwrap();
        if now.hour() < hour {
            boundary = boundary
                .checked_sub_signed(chrono::TimeDelta::days(1))
                .ok_or_else(|| anyhow::anyhow!("daily boundary out of range"))?;
        }
        anyhow::ensure!(
            (1..=9999).contains(&boundary.year()),
            "daily boundary outside Python datetime range"
        );
        if updated < boundary {
            return Ok(Some("daily"));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_bridge_overrides_env_and_preserves_python_float_rules() {
        use serde_json::json;
        assert_eq!(configured_freshness_seconds(&json!({}), Some("90")), 90.0);
        for value in [
            json!(null),
            json!(true),
            json!([]),
            json!({}),
            json!("invalid"),
            json!(""),
        ] {
            assert_eq!(
                configured_freshness_seconds(
                    &json!({"agent":{"gateway_auto_continue_freshness":value}}),
                    Some("90")
                ),
                3600.0
            );
        }
        for (value, expected) in [
            (json!(0), 0.0),
            (json!(-1), -1.0),
            (json!(2.5), 2.5),
            (json!(" 1_200.5 "), 1200.5),
            (json!("١٢.٥"), 12.5),
            (json!("infinity"), f64::INFINITY),
            (json!("-Infinity"), f64::NEG_INFINITY),
            (json!("1e309"), f64::INFINITY),
        ] {
            assert_eq!(
                configured_freshness_seconds(
                    &json!({"agent":{"gateway_auto_continue_freshness":value}}),
                    Some("90")
                ),
                expected
            );
        }
        assert!(configured_freshness_seconds(&json!({}), Some("NaN")).is_nan());
        for env in [None, Some(""), Some("True"), Some("1__2")] {
            assert_eq!(configured_freshness_seconds(&json!({}), env), 3600.0);
        }
    }
    use serde_json::{json, Value};

    #[test]
    fn existing_route_gates_preserve_none_mode_and_freshness_boundaries() {
        use crate::session_entry::SessionEntry;
        use serde_json::json;
        let mut entry = SessionEntry::from_dict(&json!({
            "session_id":"s", "session_key":"key",
            "created_at":"2026-09-06T12:00:00", "updated_at":"2026-09-06T12:00:00",
            "resume_pending":true, "last_resume_marked_at":"2026-09-06T12:01:00"
        }))
        .unwrap();
        let now =
            NaiveDateTime::parse_from_str("2026-09-06T12:02:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let mut policy = SessionResetPolicy::default();
        assert_eq!(
            existing_reset_reason(&entry, &policy, now, false, 1.0).unwrap(),
            None
        );
        policy.mode = json!("idle");
        policy.idle_minutes = json!(30);
        assert_eq!(
            existing_reset_reason(&entry, &policy, now, false, 60.0).unwrap(),
            None
        );
        assert_eq!(
            existing_reset_reason(&entry, &policy, now, false, 59.0).unwrap(),
            Some("resume_pending_expired")
        );
        // Active processes suppress normal reset, not the separate stale marker gate.
        assert_eq!(
            existing_reset_reason(&entry, &policy, now, true, 59.0).unwrap(),
            Some("resume_pending_expired")
        );
        for disabled in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                existing_reset_reason(&entry, &policy, now, false, disabled).unwrap(),
                None
            );
        }
        entry
            .fields
            .insert("last_resume_marked_at".into(), json!(null));
        assert_eq!(
            existing_reset_reason(&entry, &policy, now, false, 60.0).unwrap(),
            Some("resume_pending_expired")
        );
        policy.idle_minutes = json!(1);
        assert_eq!(
            existing_reset_reason(&entry, &policy, now, false, 60.0).unwrap(),
            Some("idle")
        );
        entry.fields.insert("suspended".into(), json!(true));
        policy.mode = json!("none");
        assert_eq!(
            existing_reset_reason(&entry, &policy, now, true, 0.0).unwrap(),
            Some("suspended")
        );
        entry.fields.insert("suspended".into(), json!(false));
        entry.fields.insert(
            "last_resume_marked_at".into(),
            json!("2026-09-06T12:00:00+08:00"),
        );
        policy.mode = json!("idle");
        assert!(existing_reset_reason(&entry, &policy, now, true, 60.0).is_err());
    }

    #[test]
    fn process_probe_errors_preserve_sessions_without_hiding_reset_errors() {
        let now =
            NaiveDateTime::parse_from_str("2026-09-06T04:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let policy = SessionResetPolicy {
            mode: json!("idle"),
            idle_minutes: json!(0),
            ..Default::default()
        };
        let updated = now - chrono::TimeDelta::seconds(1);
        assert_eq!(
            reset_reason(&policy, updated, now, active_processes_safe("key", None)).unwrap(),
            Some("idle")
        );
        let mut calls = 0;
        let mut failed = |key: &str| -> anyhow::Result<bool> {
            assert_eq!(key, "key");
            calls += 1;
            anyhow::bail!("registry unavailable")
        };
        assert_eq!(
            reset_reason(
                &policy,
                updated,
                now,
                active_processes_safe("key", Some(&mut failed))
            )
            .unwrap(),
            None
        );
        assert_eq!(calls, 1);
        let mut idle = |_: &str| Ok(false);
        assert!(!active_processes_safe("key", Some(&mut idle)));
        let mut running = |_: &str| Ok(true);
        assert!(active_processes_safe("key", Some(&mut running)));
        let invalid = SessionResetPolicy {
            idle_minutes: json!("bad"),
            ..policy
        };
        assert!(reset_reason(&invalid, updated, now, false).is_err());
        assert_eq!(reset_reason(&invalid, updated, now, true).unwrap(), None);
    }

    #[test]
    fn process_age_matches_python() {
        let rows: Value = serde_json::from_str(include_str!(
            "../../../tools/session-process-age-goldens.json"
        ))
        .unwrap();
        for row in rows["processes"].as_array().unwrap() {
            assert_eq!(
                process_pins_session(
                    row["process_session"].as_str().unwrap(),
                    "key",
                    row["exited"].as_bool().unwrap(),
                    row["started"].as_f64().unwrap(),
                    100.0,
                    row["limit"].as_f64()
                ),
                row["result"].as_bool().unwrap(),
                "{row}"
            );
        }
        for row in rows["settings"].as_array().unwrap() {
            let policy = SessionResetPolicy {
                bg_process_max_age_hours: row["hours"].clone(),
                ..Default::default()
            };
            let result = process_age_limit(&policy);
            if row["error"] == true {
                assert!(result.is_err(), "{row}");
            } else {
                assert_eq!(result.unwrap(), row["result"].as_f64(), "{row}");
            }
        }
    }

    #[test]
    fn reset_decisions_match_python() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/session-reset-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let date = |key: &str| {
                NaiveDateTime::parse_from_str(row[key].as_str().unwrap(), "%Y-%m-%dT%H:%M:%S%.f")
                    .unwrap()
            };
            let policy = SessionResetPolicy::from_dict(&row["policy"]);
            let result = reset_reason(
                &policy,
                date("updated"),
                date("now"),
                row["active"].as_bool().unwrap(),
            );
            if row["error"] == true {
                assert!(result.is_err(), "{row}");
            } else {
                assert_eq!(json!(result.unwrap()), row["result"], "{row}");
            }
        }
    }
}
