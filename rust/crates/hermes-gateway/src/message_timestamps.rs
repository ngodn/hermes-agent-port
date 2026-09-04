//! Port of gateway/message_timestamps.py.
//!
// Public API is ahead of its callers (the context-building path wires it).
#![allow(dead_code)]
//!
//! Render gateway message timestamps exactly once. Messages need a timestamp in
//! the LLM context for temporal awareness, but persisted content must stay clean
//! so replay does not accumulate `[timestamp] [timestamp] ...` prefixes across
//! turns.
//!
//! `tz` follows Python's `tzinfo` parameter: `Some(offset)` interprets/formats
//! in that fixed offset, `None` uses the system local zone. Formatting matches
//! `[Tue 2026-04-28 13:40:53 CEST]` (`%a %Y-%m-%d %H:%M:%S %Z`); the `%Z` field
//! is the local zone's abbreviation (system-dependent) or the numeric offset for
//! a fixed offset, mirroring what the platform can render.

use std::sync::OnceLock;

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone, Utc};
use fancy_regex::Regex;
use serde_json::Value;

// Current gateway format: [Tue 2026-04-28 13:40:53 CEST]
fn human_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\[(?P<dow>[A-Z][a-z]{2}) (?P<date>\d{4}-\d{2}-\d{2}) (?P<time>\d{2}:\d{2}:\d{2})(?: (?P<tz>[A-Za-z0-9_+\-/:]+))?\]\s*",
        )
        .unwrap()
    })
}

// Older gateway format: [2026-04-13T17:02:06+0200] or [+02:00]
fn iso_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[(?P<iso>\d{4}-\d{2}-\d{2}T[^\]]+)\]\s*").unwrap())
}

fn epoch_of<Tz: TimeZone>(dt: DateTime<Tz>) -> f64 {
    dt.timestamp_micros() as f64 / 1_000_000.0
}

/// Interpret a naive datetime in `tz` (or local when `None`) as epoch seconds.
fn naive_to_epoch(naive: NaiveDateTime, tz: Option<FixedOffset>) -> Option<f64> {
    match tz {
        Some(off) => off.from_local_datetime(&naive).single().map(epoch_of),
        None => Local.from_local_datetime(&naive).single().map(epoch_of),
    }
}

/// Parse an ISO string the way Python `datetime.fromisoformat` does for the
/// gateway's data: RFC3339 with `Z`/`±HH:MM`, the older `±HHMM` (no colon), or a
/// naive value (assigned `tz`/local).
fn parse_iso_string(text: &str, tz: Option<FixedOffset>) -> Option<f64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(epoch_of(dt.with_timezone(&Utc)));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%z"] {
        if let Ok(dt) = DateTime::parse_from_str(text, fmt) {
            return Some(epoch_of(dt.with_timezone(&Utc)));
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, fmt) {
            return naive_to_epoch(naive, tz);
        }
    }
    None
}

/// Coerce a timestamp-like value to Unix epoch seconds. Accepts epoch numbers,
/// ISO strings, and the bracketed human-readable prefix. `None` when
/// uninterpretable.
pub fn coerce_message_timestamp(ts_value: &Value, tz: Option<FixedOffset>) -> Option<f64> {
    match ts_value {
        Value::Null => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let text = s.trim();
            if text.is_empty() {
                return None;
            }
            if let Some(parsed) = parse_timestamp_prefix(text, tz) {
                return Some(parsed);
            }
            if let Ok(f) = text.parse::<f64>() {
                return Some(f);
            }
            parse_iso_string(text, tz)
        }
        _ => None,
    }
}

/// Format a timestamp value as `[Tue 2026-04-28 13:40:53 CEST]`, or `""`.
pub fn format_message_timestamp(ts_value: &Value, tz: Option<FixedOffset>) -> String {
    let Some(epoch) = coerce_message_timestamp(ts_value, tz) else {
        return String::new();
    };
    let secs = epoch.floor() as i64;
    let nanos = ((epoch - epoch.floor()) * 1e9) as u32;
    let Some(utc) = Utc.timestamp_opt(secs, nanos).single() else {
        return String::new();
    };
    match tz {
        Some(off) => format!(
            "[{}]",
            utc.with_timezone(&off).format("%a %Y-%m-%d %H:%M:%S %Z")
        ),
        None => format!(
            "[{}]",
            utc.with_timezone(&Local).format("%a %Y-%m-%d %H:%M:%S %Z")
        ),
    }
}

/// Convenience: format from an epoch directly.
pub fn format_epoch(epoch: f64, tz: Option<FixedOffset>) -> String {
    format_message_timestamp(&Value::from(epoch), tz)
}

/// Strip one or more leading gateway timestamp prefixes from `content`.
/// Returns `(clean_content, embedded_epoch)`; when multiple prefixes are
/// present the one closest to the message text wins (preserving the original
/// platform-send time on legacy contaminated rows).
pub fn strip_leading_message_timestamps(
    content: &str,
    tz: Option<FixedOffset>,
) -> (String, Option<f64>) {
    if content.is_empty() {
        return (content.to_string(), None);
    }
    let mut text = content.to_string();
    let mut embedded: Option<f64> = None;
    loop {
        let m = match_prefix(&text);
        let Some((end, parsed)) = m else { break };
        if let Some(p) = parsed_epoch(&text, end, parsed, tz) {
            embedded = Some(p);
        }
        text = text[end..].to_string();
    }
    (text, embedded)
}

/// Render a user message for LLM context with exactly one timestamp prefix. An
/// existing prefix's time wins over `ts_value`; if neither yields a time the
/// cleaned content is returned unchanged.
pub fn render_user_content_with_timestamp(
    content: &str,
    ts_value: &Value,
    tz: Option<FixedOffset>,
) -> String {
    let (clean, embedded) = strip_leading_message_timestamps(content, tz);
    let effective = match embedded {
        Some(e) => Value::from(e),
        None => ts_value.clone(),
    };
    let prefix = format_message_timestamp(&effective, tz);
    if prefix.is_empty() {
        return clean;
    }
    if clean.is_empty() {
        prefix
    } else {
        format!("{prefix} {clean}")
    }
}

/// Which prefix pattern (if any) matches the start; returns the byte end of the
/// match and whether it was the ISO form.
enum PrefixKind {
    Human,
    Iso,
}

fn match_prefix(text: &str) -> Option<(usize, PrefixKind)> {
    if let Ok(Some(m)) = human_re().find(text) {
        if m.start() == 0 {
            return Some((m.end(), PrefixKind::Human));
        }
    }
    if let Ok(Some(m)) = iso_re().find(text) {
        if m.start() == 0 {
            return Some((m.end(), PrefixKind::Iso));
        }
    }
    None
}

fn parse_timestamp_prefix(text: &str, tz: Option<FixedOffset>) -> Option<f64> {
    let (end, kind) = match_prefix(text)?;
    parsed_epoch(text, end, kind, tz)
}

fn parsed_epoch(text: &str, _end: usize, kind: PrefixKind, tz: Option<FixedOffset>) -> Option<f64> {
    match kind {
        PrefixKind::Iso => {
            let caps = iso_re().captures(text).ok().flatten()?;
            let iso = caps.name("iso")?.as_str();
            parse_iso_string(iso, tz)
        }
        PrefixKind::Human => {
            let caps = human_re().captures(text).ok().flatten()?;
            let date = caps.name("date")?.as_str();
            let time = caps.name("time")?.as_str();
            let naive =
                NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S")
                    .ok()?;
            naive_to_epoch(naive, tz)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_epoch_number_and_string() {
        assert_eq!(
            coerce_message_timestamp(&Value::from(1000.5), None),
            Some(1000.5)
        );
        assert_eq!(
            coerce_message_timestamp(&Value::from("1234"), None),
            Some(1234.0)
        );
        assert_eq!(coerce_message_timestamp(&Value::Null, None), None);
        assert_eq!(coerce_message_timestamp(&Value::from("   "), None), None);
    }

    #[test]
    fn coerce_iso_with_offset() {
        // Both offset spellings resolve to the same instant, equal to the same
        // wall time expressed in UTC two hours earlier.
        let want = DateTime::parse_from_rfc3339("2026-04-13T17:02:06+02:00")
            .unwrap()
            .timestamp();
        let colon =
            coerce_message_timestamp(&Value::from("2026-04-13T17:02:06+02:00"), None).unwrap();
        let no_colon =
            coerce_message_timestamp(&Value::from("2026-04-13T17:02:06+0200"), None).unwrap();
        assert_eq!(colon as i64, want);
        assert_eq!(no_colon as i64, want);
        let utc =
            coerce_message_timestamp(&Value::from("2026-04-13T15:02:06+00:00"), None).unwrap();
        assert_eq!(colon as i64, utc as i64);
    }

    #[test]
    fn format_roundtrips_with_fixed_offset() {
        let off = FixedOffset::east_opt(2 * 3600).unwrap();
        let epoch = DateTime::parse_from_rfc3339("2026-04-13T17:02:06+02:00")
            .unwrap()
            .timestamp() as f64;
        let want = Utc
            .timestamp_opt(epoch as i64, 0)
            .single()
            .unwrap()
            .with_timezone(&off)
            .format("%a %Y-%m-%d %H:%M:%S %Z")
            .to_string();
        assert_eq!(format_epoch(epoch, Some(off)), format!("[{want}]"));
        assert!(want.starts_with("Mon 2026-04-13 17:02:06 "), "got {want}");
    }

    #[test]
    fn strip_takes_closest_timestamp() {
        let off = FixedOffset::east_opt(0).unwrap();
        // Two stacked prefixes: [processing time] [platform time] message.
        // The one closest to the text (second) wins.
        let content = "[Mon 2026-04-13 10:00:00 UTC] [Mon 2026-04-13 17:02:06 UTC] hello";
        let (clean, embedded) = strip_leading_message_timestamps(content, Some(off));
        assert_eq!(clean, "hello");
        // The winner is 17:02:06 UTC, not the earlier 10:00:00 UTC.
        let want =
            coerce_message_timestamp(&Value::from("2026-04-13T17:02:06+00:00"), None).unwrap();
        assert_eq!(embedded.unwrap() as i64, want as i64);
    }

    #[test]
    fn render_adds_single_prefix_and_prefers_embedded() {
        let off = FixedOffset::east_opt(0).unwrap();
        // Content already carries a timestamp: its time wins over ts_value.
        let out = render_user_content_with_timestamp(
            "[Mon 2026-04-13 17:02:06 UTC] hi there",
            &Value::from(0.0),
            Some(off),
        );
        assert!(out.starts_with("[Mon 2026-04-13 17:02:06 "), "got {out}");
        assert!(out.ends_with("hi there"));
        // Exactly one prefix (no doubling).
        assert_eq!(out.matches("2026-04-13").count(), 1);
    }

    #[test]
    fn render_without_timestamp_returns_clean() {
        // No ts available -> content unchanged.
        let out = render_user_content_with_timestamp("plain", &Value::Null, None);
        assert_eq!(out, "plain");
    }
}
