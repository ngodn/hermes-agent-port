//! Port of agent/retry_utils.py.
//!
// Public API is ahead of its callers (the gateway retry loop is not wired to
// these yet), so allow the surface until callers land.
#![allow(dead_code)]
//!
//! Retry utilities: jittered backoff for decorrelated retries.
//!
//! Replaces fixed exponential backoff with jittered delays to prevent
//! thundering-herd retry spikes when multiple sessions hit the same
//! rate-limited provider concurrently.
//!
//! Modeling choices vs the Python original:
//!
//! * `parse_retry_after_seconds` in Python takes "a raw value OR a headers
//!   mapping" in one dynamically-typed argument. Rust has no such duck typing,
//!   so the two shapes are split into two entry points:
//!     - `parse_retry_after_seconds_value` takes a `RetryAfterValue`, an enum
//!       that reproduces Python's `isinstance` ladder exactly (bool rejected,
//!       int/float clamped to >= 0, string parsed, absent -> None).
//!     - `parse_retry_after_seconds_headers` takes a getter closure that mimics
//!       Python's `.get` on a dict-like object, trying "Retry-After" then
//!       "retry-after" (the case-insensitive fallback for plain dicts; real
//!       HTTP header containers are already case-insensitive).
//!     - `parse_retry_after_seconds_header_map` is a convenience over a
//!       `HashMap<String, String>` that does the same two-casing lookup.
//!
//! * `jittered_backoff`'s jitter is NON-security timing jitter, so it does not
//!   use the kernel CSPRNG. Python seeds a Mersenne Twister from
//!   `time_ns ^ counter`; we seed a small splitmix64 from the same shape. The
//!   exact random stream is not reproduced (it does not need to be), but the
//!   base/cap/factor math is exact. Tests drive the deterministic
//!   `jittered_backoff_with(..., rand01)` variant where `rand01` in [0, 1)
//!   stands in for Python's `random.random()`.
//!
//! * HTTP-date parsing uses `chrono::DateTime::parse_from_rfc2822`, which covers
//!   the RFC 7231 IMF-fixdate form (e.g. "Sun, 06 Nov 1994 08:49:37 GMT"), the
//!   common case in the wild. See `parse_http_date_seconds` for the caveat: the
//!   two obsolete forms `parsedate_to_datetime` also accepts (RFC 850
//!   "Sunday, 06-Nov-94 08:49:37 GMT" and asctime "Sun Nov  6 08:49:37 1994")
//!   are not parsed by chrono's rfc2822 parser and yield `None` here. Past
//!   dates clamp to 0.0 in both, so only a future date in one of those two rare
//!   forms would differ.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;

// Z.AI Coding Plan's GLM-5.2 endpoint often returns HTTP 429 code 1305
// ("The service may be temporarily overloaded...") for otherwise valid Hermes
// requests. Short retries tend to hammer the same overloaded window; after a
// few normal retries, progressively widen the wait window. Keep the cap
// interactive-friendly: a simple TUI message should fail visibly in minutes,
// not sit silent for 20+ minutes.
const ZAI_CODING_OVERLOAD_LONG_BACKOFF: [f64; 4] = [30.0, 60.0, 90.0, 120.0];

// Number of initial short retries before the adaptive long-backoff tier kicks
// in. Shared by `adaptive_rate_limit_backoff` (which walks the long table
// starting at attempt `short_attempts + 1`) and
// `zai_coding_overload_retry_ceiling` (which sizes the retry loop so every
// long-tier entry is reachable). Keeping it a single module constant prevents
// the two from silently desyncing if the short-retry count is ever tuned.
const ZAI_CODING_OVERLOAD_SHORT_ATTEMPTS: i64 = 3;

// Monotonic counter for jitter seed uniqueness within the same process.
// Python guards a module global with a threading.Lock; an atomic gives the
// same "each call sees a distinct tick" guarantee without a lock.
static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The scalar shapes Python accepts as a raw `Retry-After` value. Reproduces
/// the `isinstance` ladder in `parse_retry_after_seconds`:
/// `Absent` is Python `None`, `Bool` is rejected, `Int`/`Float` are clamped,
/// `Text` is parsed (numeric string, then HTTP-date).
#[derive(Debug, Clone, PartialEq)]
pub enum RetryAfterValue {
    Absent,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

/// Parse a raw `Retry-After` value into non-negative seconds.
///
/// Returns seconds as `f64` (negative deltas clamped to 0.0), or `None` when
/// the value is absent or unparseable. Mirrors Python's scalar branch: bool is
/// rejected, int/float are clamped to `>= 0.0`, a string is stripped and parsed
/// as a number first, then as an HTTP-date.
pub fn parse_retry_after_seconds_value(value: &RetryAfterValue) -> Option<f64> {
    match value {
        RetryAfterValue::Absent => None,
        // Python: `isinstance(raw, bool)` returns None. bool is a subtype of
        // int in Python, so this check runs before the numeric branch.
        RetryAfterValue::Bool(_) => None,
        RetryAfterValue::Int(n) => Some((*n as f64).max(0.0)),
        RetryAfterValue::Float(f) => Some(f.max(0.0)),
        RetryAfterValue::Text(s) => {
            let text = s.trim();
            if text.is_empty() {
                return None;
            }
            // Python `float(text)`: accepts "30", "2.5", "1e3", "inf", "nan".
            if let Ok(v) = text.parse::<f64>() {
                return Some(v.max(0.0));
            }
            parse_http_date_seconds(text)
        }
    }
}

/// Headers-mapping variant. `get` mimics Python's `.get` on a dict-like object:
/// it is called with "Retry-After" first and, only if that yields `None`, with
/// "retry-after". Whatever value comes back is parsed as a scalar, exactly as
/// Python falls through to its scalar logic after pulling the header out.
pub fn parse_retry_after_seconds_headers<F>(get: F) -> Option<f64>
where
    F: Fn(&str) -> Option<RetryAfterValue>,
{
    let value = get("Retry-After").or_else(|| get("retry-after"));
    parse_retry_after_seconds_value(&value.unwrap_or(RetryAfterValue::Absent))
}

/// Convenience over a `HashMap<String, String>`: does the same two-casing
/// lookup Python's dict fallback does, then parses the string value.
pub fn parse_retry_after_seconds_header_map(headers: &HashMap<String, String>) -> Option<f64> {
    parse_retry_after_seconds_headers(|key| {
        headers.get(key).map(|v| RetryAfterValue::Text(v.clone()))
    })
}

/// HTTP-date form (RFC 7231): seconds until that instant, clamped at 0.
///
/// Only the IMF-fixdate form is handled (chrono's rfc2822 parser). The two
/// obsolete forms `parsedate_to_datetime` also accepts are not; see the module
/// doc for the caveat. chrono always yields a concrete offset (GMT -> +0000),
/// so there is no naive/tz-less branch to mirror.
fn parse_http_date_seconds(text: &str) -> Option<f64> {
    let when = chrono::DateTime::parse_from_rfc2822(text).ok()?;
    let delta = when.with_timezone(&Utc) - Utc::now();
    // total_seconds() equivalent, fractional. num_milliseconds keeps sub-second
    // precision without depending on now's nanosecond tail.
    let secs = delta.num_milliseconds() as f64 / 1000.0;
    Some(secs.max(0.0))
}

/// Compute the (un-jittered) backoff delay component.
///
/// `min(base_delay * 2^(attempt-1), max_delay)`, with the same two short
/// circuits Python takes: exponent capped at 0 for `attempt <= 1`, and a jump
/// straight to `max_delay` when the exponent would overflow (>= 63) or the base
/// is non-positive.
fn backoff_delay(attempt: i64, base_delay: f64, max_delay: f64) -> f64 {
    let exponent = (attempt - 1).max(0);
    if exponent >= 63 || base_delay <= 0.0 {
        max_delay
    } else {
        (base_delay * 2f64.powi(exponent as i32)).min(max_delay)
    }
}

/// Deterministic core of `jittered_backoff`. `rand01` in [0, 1) plays the role
/// of Python's `random.random()`: Python's `random.uniform(0, jitter_ratio *
/// delay)` equals `rand01 * jitter_ratio * delay`.
pub fn jittered_backoff_with(
    attempt: i64,
    base_delay: f64,
    max_delay: f64,
    jitter_ratio: f64,
    rand01: f64,
) -> f64 {
    let delay = backoff_delay(attempt, base_delay, max_delay);
    let jitter = rand01 * jitter_ratio * delay;
    delay + jitter
}

/// Compute a jittered exponential backoff delay.
///
/// `attempt` is the 1-based retry number. Python's keyword defaults are
/// `base_delay = 5.0`, `max_delay = 120.0`, `jitter_ratio = 0.5`; Rust has no
/// default arguments, so callers pass them explicitly. Returns
/// `min(base * 2^(attempt-1), max_delay) + jitter`, where jitter is uniform in
/// `[0, jitter_ratio * delay]`. The jitter decorrelates concurrent retries so
/// multiple sessions hitting the same provider do not all retry at once.
pub fn jittered_backoff(attempt: i64, base_delay: f64, max_delay: f64, jitter_ratio: f64) -> f64 {
    let tick = JITTER_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Same seed shape as Python (time ^ counter, masked to 32 bits); the PRNG
    // itself differs, which is fine for non-security backoff jitter.
    let seed = (nanos ^ tick.wrapping_mul(0x9E37_79B9)) & 0xFFFF_FFFF;
    let rand01 = splitmix01(seed);
    jittered_backoff_with(attempt, base_delay, max_delay, jitter_ratio, rand01)
}

/// Small non-cryptographic PRNG: one splitmix64 step, top 53 bits mapped to a
/// float in [0, 1). Used only for backoff timing jitter.
fn splitmix01(seed: u64) -> f64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    ((z >> 11) as f64) / ((1u64 << 53) as f64)
}

/// The provider error shape the retry classifier inspects. Python reads
/// attributes off an arbitrary exception via `getattr`; here the relevant bits
/// are captured explicitly. `repr` is Python's `str(error)`, always folded into
/// the flattened text.
#[derive(Debug, Clone, Default)]
pub struct ProviderError {
    pub repr: String,
    pub message: Option<String>,
    pub body: Option<String>,
    pub response: Option<String>,
    pub status_code: Option<i64>,
}

/// Best-effort flattened provider error text for retry classification.
///
/// Joins `str(error)` and the message/body/response attributes (skipping
/// absent ones) with single spaces, lowercased. `repr` stands in for
/// `str(error)`, which Python always includes.
fn error_text(error: &ProviderError) -> String {
    let mut parts: Vec<&str> = vec![error.repr.as_str()];
    for s in [&error.message, &error.body, &error.response]
        .into_iter()
        .flatten()
    {
        parts.push(s.as_str());
    }
    parts.join(" ").to_lowercase()
}

/// Return true for Z.AI Coding Plan transient overload 429s.
///
/// The coding-plan endpoint reports overload as HTTP 429 with body code 1305
/// and message "The service may be temporarily overloaded...". Only that narrow
/// shape is treated specially so ordinary quota/billing 429s still fail fast.
pub fn is_zai_coding_overload_error(
    base_url: Option<&str>,
    model: Option<&str>,
    error: &ProviderError,
) -> bool {
    let base = base_url.unwrap_or("").to_lowercase();
    let model_name = model.unwrap_or("").to_lowercase();
    let text = error_text(error);
    error.status_code == Some(429)
        && base.contains("api.z.ai/api/coding/paas/v4")
        && model_name.contains("glm-5.2")
        && (text.contains("1305") || text.contains("temporarily overloaded"))
}

/// Provider-aware rate-limit backoff.
///
/// For most providers this returns `default_wait` unchanged with no reason
/// label. For Z.AI Coding Plan GLM-5.2 overloads it keeps the first
/// `short_attempts` retries on the normal short schedule (labeled
/// "zai_coding_overload_short"), then switches to progressively longer waits
/// (30/60/90/120s, capped) plus light jitter (labeled
/// "zai_coding_overload_long"). `attempt` is 1-based.
pub fn adaptive_rate_limit_backoff(
    attempt: i64,
    base_url: Option<&str>,
    model: Option<&str>,
    error: &ProviderError,
    default_wait: f64,
    short_attempts: i64,
) -> (f64, Option<&'static str>) {
    if !is_zai_coding_overload_error(base_url, model, error) {
        return (default_wait, None);
    }
    if attempt <= short_attempts {
        return (default_wait, Some("zai_coding_overload_short"));
    }
    let last = (ZAI_CODING_OVERLOAD_LONG_BACKOFF.len() as i64) - 1;
    let idx = (attempt - short_attempts - 1).min(last).max(0) as usize;
    let base_delay = ZAI_CODING_OVERLOAD_LONG_BACKOFF[idx];
    // A smaller jitter ratio keeps long waits readable while still avoiding
    // synchronized retry storms across concurrent Hermes sessions.
    (
        jittered_backoff(1, base_delay, base_delay, 0.2),
        Some("zai_coding_overload_long"),
    )
}

/// Retry-loop ceiling needed for the full Z.AI overload backoff schedule.
///
/// The adaptive policy runs `short_attempts` short retries, then walks the
/// long-backoff table one entry per subsequent attempt. The retry loop gives up
/// as soon as `retry_count >= ceiling`, and that check runs before the
/// attempt's backoff is computed, so the ceiling must sit one past the final
/// long-backoff entry for every long tier to actually execute.
pub fn zai_coding_overload_retry_ceiling(short_attempts: i64) -> i64 {
    short_attempts + ZAI_CODING_OVERLOAD_LONG_BACKOFF.len() as i64 + 1
}

/// The default short-attempts count, exposed so callers match Python's default
/// argument without hardcoding the literal.
pub fn zai_coding_overload_short_attempts_default() -> i64 {
    ZAI_CODING_OVERLOAD_SHORT_ATTEMPTS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zai_error() -> ProviderError {
        ProviderError {
            repr: "The service may be temporarily overloaded, code 1305".to_string(),
            status_code: Some(429),
            ..Default::default()
        }
    }

    // Golden values captured from the Python module (see the port task notes):
    //   p('30')=30.0, p('  ')=None, p(-5)=0.0, p(True)=None, p(False)=None,
    //   p(None)=None, p(42)=42.0, p(2.5)=2.5, p(-1.5)=0.0, p('')=None,
    //   p('abc')=None.
    #[test]
    fn parse_value_numeric_and_string() {
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Text("30".into())),
            Some(30.0)
        );
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Int(42)),
            Some(42.0)
        );
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Float(2.5)),
            Some(2.5)
        );
    }

    #[test]
    fn parse_value_negative_clamps_to_zero() {
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Int(-5)),
            Some(0.0)
        );
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Float(-1.5)),
            Some(0.0)
        );
    }

    #[test]
    fn parse_value_bool_rejected() {
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Bool(true)),
            None
        );
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Bool(false)),
            None
        );
    }

    #[test]
    fn parse_value_absent_and_blank_and_garbage() {
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Absent),
            None
        );
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Text("  ".into())),
            None
        );
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Text("".into())),
            None
        );
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Text("abc".into())),
            None
        );
    }

    #[test]
    fn parse_headers_casing_and_missing() {
        // Golden: p({'Retry-After':'30'})=30.0, p({'retry-after':'45'})=45.0,
        // p({'X':'1'})=None.
        let mut upper = HashMap::new();
        upper.insert("Retry-After".to_string(), "30".to_string());
        assert_eq!(parse_retry_after_seconds_header_map(&upper), Some(30.0));

        let mut lower = HashMap::new();
        lower.insert("retry-after".to_string(), "45".to_string());
        assert_eq!(parse_retry_after_seconds_header_map(&lower), Some(45.0));

        let mut other = HashMap::new();
        other.insert("X".to_string(), "1".to_string());
        assert_eq!(parse_retry_after_seconds_header_map(&other), None);

        // A getter that never finds the key -> None (Python's absent header).
        assert_eq!(parse_retry_after_seconds_headers(|_| None), None);
    }

    #[test]
    fn parse_http_date_past_clamps_and_future_positive() {
        // Golden: p('Sun, 06 Nov 1994 08:49:37 GMT')=0.0 (past, clamped).
        assert_eq!(
            parse_retry_after_seconds_value(&RetryAfterValue::Text(
                "Sun, 06 Nov 1994 08:49:37 GMT".into()
            )),
            Some(0.0)
        );
        // Golden: a far-future IMF-fixdate is a large positive number.
        let secs = parse_retry_after_seconds_value(&RetryAfterValue::Text(
            "Wed, 21 Oct 2099 07:28:00 GMT".into(),
        ))
        .expect("future date should parse");
        assert!(secs > 0.0);
    }

    #[test]
    fn backoff_delay_math_matches_python() {
        // Python delay component, base 5.0 / cap 120.0:
        //   attempt 1->5, 2->10, 3->20, 4->40, 5->80, 6->120, 63->120, 64->120.
        // rand01 = 0.0 isolates the delay component (no jitter added).
        assert_eq!(jittered_backoff_with(1, 5.0, 120.0, 0.5, 0.0), 5.0);
        assert_eq!(jittered_backoff_with(2, 5.0, 120.0, 0.5, 0.0), 10.0);
        assert_eq!(jittered_backoff_with(3, 5.0, 120.0, 0.5, 0.0), 20.0);
        assert_eq!(jittered_backoff_with(4, 5.0, 120.0, 0.5, 0.0), 40.0);
        assert_eq!(jittered_backoff_with(5, 5.0, 120.0, 0.5, 0.0), 80.0);
        assert_eq!(jittered_backoff_with(6, 5.0, 120.0, 0.5, 0.0), 120.0);
        assert_eq!(jittered_backoff_with(63, 5.0, 120.0, 0.5, 0.0), 120.0);
        assert_eq!(jittered_backoff_with(64, 5.0, 120.0, 0.5, 0.0), 120.0);
        // attempt <= 1 pins the exponent at 0.
        assert_eq!(jittered_backoff_with(0, 5.0, 120.0, 0.5, 0.0), 5.0);
        assert_eq!(jittered_backoff_with(-1, 5.0, 120.0, 0.5, 0.0), 5.0);
        // base_delay <= 0 jumps straight to the cap.
        assert_eq!(jittered_backoff_with(1, 0.0, 120.0, 0.5, 0.0), 120.0);
    }

    #[test]
    fn backoff_jitter_is_added_linearly() {
        // jitter = rand01 * jitter_ratio * delay.
        // attempt 1, delay 5, ratio 0.5, rand 1.0 -> 5 + 2.5 = 7.5.
        assert_eq!(jittered_backoff_with(1, 5.0, 120.0, 0.5, 1.0), 7.5);
        // attempt 3, delay 20, ratio 0.5, rand 0.5 -> 20 + 5 = 25.
        assert_eq!(jittered_backoff_with(3, 5.0, 120.0, 0.5, 0.5), 25.0);
    }

    #[test]
    fn jittered_backoff_stays_in_bounds() {
        // The production path uses a real PRNG; result must land in
        // [delay, delay + jitter_ratio*delay].
        for _ in 0..1000 {
            let v = jittered_backoff(3, 5.0, 120.0, 0.5);
            assert!((20.0..=30.0).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn zai_overload_detection_true() {
        assert!(is_zai_coding_overload_error(
            Some("https://api.z.ai/api/coding/paas/v4"),
            Some("glm-5.2"),
            &zai_error()
        ));
        // "temporarily overloaded" alone (no 1305) also matches.
        let err = ProviderError {
            repr: "The service may be temporarily overloaded".to_string(),
            status_code: Some(429),
            ..Default::default()
        };
        assert!(is_zai_coding_overload_error(
            Some("https://API.Z.AI/api/coding/paas/v4"),
            Some("GLM-5.2"),
            &err
        ));
    }

    #[test]
    fn zai_overload_detection_false() {
        // Wrong status.
        let mut err = zai_error();
        err.status_code = Some(500);
        assert!(!is_zai_coding_overload_error(
            Some("https://api.z.ai/api/coding/paas/v4"),
            Some("glm-5.2"),
            &err
        ));
        // Wrong base url.
        assert!(!is_zai_coding_overload_error(
            Some("https://api.openai.com/v1"),
            Some("glm-5.2"),
            &zai_error()
        ));
        // Wrong model.
        assert!(!is_zai_coding_overload_error(
            Some("https://api.z.ai/api/coding/paas/v4"),
            Some("glm-4.6"),
            &zai_error()
        ));
        // Right shape but no overload marker in the text.
        let quota = ProviderError {
            repr: "quota exceeded for this billing period".to_string(),
            status_code: Some(429),
            ..Default::default()
        };
        assert!(!is_zai_coding_overload_error(
            Some("https://api.z.ai/api/coding/paas/v4"),
            Some("glm-5.2"),
            &quota
        ));
        // None url/model.
        assert!(!is_zai_coding_overload_error(None, None, &zai_error()));
    }

    #[test]
    fn adaptive_non_zai_returns_default() {
        let plain = ProviderError {
            repr: "some other error".to_string(),
            status_code: Some(500),
            ..Default::default()
        };
        assert_eq!(
            adaptive_rate_limit_backoff(
                5,
                Some("https://api.openai.com"),
                Some("gpt"),
                &plain,
                7.5,
                3
            ),
            (7.5, None)
        );
    }

    #[test]
    fn adaptive_short_tier_keeps_default_wait() {
        let base = Some("https://api.z.ai/api/coding/paas/v4");
        for attempt in 1..=3 {
            let (wait, label) =
                adaptive_rate_limit_backoff(attempt, base, Some("glm-5.2"), &zai_error(), 4.0, 3);
            assert_eq!(wait, 4.0);
            assert_eq!(label, Some("zai_coding_overload_short"));
        }
    }

    #[test]
    fn adaptive_long_tier_walks_table() {
        let base = Some("https://api.z.ai/api/coding/paas/v4");
        // attempt short+1..short+len maps to 30/60/90/120; beyond that stays at
        // the last entry (120). delay == base == cap, jitter_ratio 0.2, so the
        // result lands in [base, base*1.2].
        let expected = [30.0, 60.0, 90.0, 120.0, 120.0, 120.0];
        for (i, &b) in expected.iter().enumerate() {
            let attempt = 4 + i as i64;
            let (wait, label) =
                adaptive_rate_limit_backoff(attempt, base, Some("glm-5.2"), &zai_error(), 4.0, 3);
            assert_eq!(label, Some("zai_coding_overload_long"));
            assert!(
                (b..=b * 1.2).contains(&wait),
                "attempt {attempt}: {wait} not in [{b}, {}]",
                b * 1.2
            );
        }
    }

    #[test]
    fn retry_ceiling_golden() {
        // Golden: zai_coding_overload_retry_ceiling() == 8 with default 3.
        assert_eq!(zai_coding_overload_retry_ceiling(3), 8);
        assert_eq!(
            zai_coding_overload_retry_ceiling(ZAI_CODING_OVERLOAD_SHORT_ATTEMPTS),
            8
        );
        assert_eq!(zai_coding_overload_retry_ceiling(5), 10);
    }
}
