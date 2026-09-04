//! Port of gateway/platforms/signal_rate_limit.py.
//!
// Public API is ahead of its callers (the Signal send path and the send_message
// tool wire acquire/feedback/report on the live adapter). Allow the surface
// until those callers land.
#![allow(dead_code)]
//!
//! Signal attachment rate-limit scheduler.
//!
//! Process-wide token-bucket simulator that mirrors the per-account attachment
//! rate limit signal-cli / Signal-Server enforce. Producers call `acquire(n)`
//! before an attachment send; on a 429 they call `feedback(retry_after, n)` so
//! the model recalibrates from the server's authoritative hint.
//!
//! Faithfulness notes vs the Python original:
//!
//!  * The only internal dependency, `agent.retry_utils.parse_retry_after_seconds`,
//!    is already ported as `crate::retry_utils`. Python passes a raw scalar
//!    (`retryAfterSeconds` field, or a regex-captured numeric string), so this
//!    port calls `parse_retry_after_seconds_value` with a `RetryAfterValue`.
//!    Retry-After parsing is reused, never reimplemented.
//!
//!  * Python takes `err: Any` in the two detection helpers and duck-types a
//!    dict vs an arbitrary object. Rust has no duck typing, so the input is
//!    modeled as [`SignalRpcError`], an enum with a `Dict` shape (code /
//!    message / the `data.response.results[*].retryAfterSeconds` list) and a
//!    `Text` shape (Python's `str(err)` fallback).
//!
//!  * Python's `_format_wait` rounds with `round()`, which is round-half-to-even
//!    (banker's rounding). Rust's `f64::round` is round-half-away-from-zero, so
//!    a dedicated [`py_round`] reproduces the even-rounding to keep `0.5 -> 0`
//!    and `2.5 -> 2` exact.
//!
//!  * The scheduler is process-global. Python guards a module-level `Optional`
//!    singleton and serializes `acquire` / `report_rpc_duration` through an
//!    `asyncio.Lock` that is released across `asyncio.sleep`. The lock only
//!    guards the tiny token-check/refill critical sections, never the sleep, so
//!    this port uses a plain `std::sync::Mutex<Inner>` locked briefly inside
//!    each section (never held across an `.await`) rather than a
//!    `tokio::sync::Mutex`. `acquire` awaits `tokio::time::sleep` with the lock
//!    dropped, matching the interleaving semantics. `estimate_wait`, `feedback`
//!    and `state` are lock-free reads/writes in Python (sync methods); here they
//!    take the same brief lock. The singleton is a `Mutex<Option<Arc<..>>>` so
//!    `get_scheduler` / `reset_scheduler` mirror Python's global + test reset.
//!
//! No adapter coupling: every method here is pure timing / serialization /
//! bookkeeping. The live Signal adapter's send callable is not referenced by
//! this module in Python either, so nothing was deferred.

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fancy_regex::Regex;
use tracing::{debug, info};

use crate::retry_utils::{parse_retry_after_seconds_value, RetryAfterValue};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Per-message attachment cap (source: Signal-{Android,Desktop} source code).
pub const SIGNAL_MAX_ATTACHMENTS_PER_MSG: i64 = 32;
/// Server-side token-bucket capacity for attachments rate limiting.
pub const SIGNAL_RATE_LIMIT_BUCKET_CAPACITY: i64 = 50;
/// Fallback token refill interval for signal-cli < v0.14.3.
pub const SIGNAL_RATE_LIMIT_DEFAULT_RETRY_AFTER: i64 = 4;
/// Initial attempt + 1 retry.
pub const SIGNAL_RATE_LIMIT_MAX_ATTEMPTS: i64 = 2;
/// If estimated waiting time > 10s, notify the user about the delay.
pub const SIGNAL_BATCH_PACING_NOTICE_THRESHOLD: f64 = 10.0;
/// signal-cli (v0.14.3+) JSON-RPC error code for RateLimitException.
pub const SIGNAL_RPC_ERROR_RATELIMIT: i64 = -5;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Raised by `SignalAdapter._rpc` for rate-limit responses when the caller has
/// opted in via `raise_on_rate_limit=True`.
///
/// Carries the server-supplied per-token Retry-After (in seconds) on
/// signal-cli >= v0.14.3. `retry_after` is `None` when the version does not
/// expose it.
#[derive(Debug, Clone)]
pub struct SignalRateLimitError {
    pub message: String,
    pub retry_after: Option<f64>,
}

impl SignalRateLimitError {
    pub fn new(message: impl Into<String>, retry_after: Option<f64>) -> Self {
        Self {
            message: message.into(),
            retry_after,
        }
    }
}

impl fmt::Display for SignalRateLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SignalRateLimitError {}

/// Raised when a caller requests more tokens than the bucket can ever hold.
#[derive(Debug, Clone)]
pub struct SignalSchedulerError {
    pub message: String,
}

impl SignalSchedulerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SignalSchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SignalSchedulerError {}

// ---------------------------------------------------------------------------
// Error input shape for the detection helpers
// ---------------------------------------------------------------------------

/// The signal-cli RPC error shapes the detection helpers inspect. Python takes
/// `err: Any` and branches on `isinstance(err, dict)`; this enum splits the two
/// shapes it actually reads.
#[derive(Debug, Clone)]
pub enum SignalRpcError {
    /// The dict-shaped JSON-RPC error. `code` is the top-level error code,
    /// `message` the top-level message, and `results` mirrors
    /// `data.response.results[*].retryAfterSeconds` (one entry per result, in
    /// order; `Absent` where the field is missing or null).
    Dict {
        code: Option<i64>,
        message: Option<String>,
        results: Vec<RetryAfterValue>,
    },
    /// An arbitrary non-dict error, Python's `str(err)`.
    Text(String),
}

// ---------------------------------------------------------------------------
// Detection helpers
// ---------------------------------------------------------------------------

// "Retry after 4 seconds" / "retry after 4 second" - libsignal-net's
// RetryLaterException string form, surfaced when 429s hit during attachment
// upload (signal-cli wraps these as AttachmentInvalidException rather than
// RateLimitException, so the typed path does not fire).
fn retry_after_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)Retry after (\d+(?:\.\d+)?)\s*second").expect("valid retry-after regex")
    })
}

// Python truthiness of the retryAfterSeconds value before it is parsed:
// `if isinstance(r, dict) and r.get("retryAfterSeconds")` skips None / 0 / 0.0
// / "" / False.
fn is_truthy(value: &RetryAfterValue) -> bool {
    match value {
        RetryAfterValue::Absent => false,
        RetryAfterValue::Bool(b) => *b,
        RetryAfterValue::Int(n) => *n != 0,
        RetryAfterValue::Float(f) => *f != 0.0,
        RetryAfterValue::Text(s) => !s.is_empty(),
    }
}

fn search_retry_after(msg: &str) -> Option<f64> {
    let caps = retry_after_regex().captures(msg).ok()??;
    let group = caps.get(1)?.as_str();
    parse_retry_after_seconds_value(&RetryAfterValue::Text(group.to_string()))
}

/// Pull the per-token Retry-After window from a signal-cli rate-limit error.
///
/// Tries two sources, in order:
/// 1. `error.data.response.results[*].retryAfterSeconds` (the structured field
///    signal-cli >= v0.14.3 surfaces for plain RateLimitException) - takes the
///    max of the parsed candidates.
/// 2. `"Retry after N seconds"` parsed out of the message (libsignal-net's
///    RetryLaterException wrapped as AttachmentInvalidException, where the
///    structured field stays null).
///
/// Numeric parsing delegates to `crate::retry_utils`. Returns `None` when
/// neither source yields a value.
pub fn extract_retry_after_seconds(err: &SignalRpcError) -> Option<f64> {
    let msg: String = match err {
        SignalRpcError::Dict {
            message, results, ..
        } => {
            let mut candidates: Vec<f64> = Vec::new();
            for value in results {
                if is_truthy(value) {
                    if let Some(c) = parse_retry_after_seconds_value(value) {
                        candidates.push(c);
                    }
                }
            }
            if !candidates.is_empty() {
                return Some(candidates.into_iter().fold(f64::NEG_INFINITY, f64::max));
            }
            message.clone().unwrap_or_default()
        }
        SignalRpcError::Text(s) => s.clone(),
    };
    search_retry_after(&msg)
}

fn contains_rate_limit_substring(message: &str) -> bool {
    let lower = message.to_lowercase();
    message.contains("[429]")
        || lower.contains("ratelimit")
        || lower.contains("retrylaterexception")
        || lower.contains("retry after")
}

/// True if a signal-cli RPC error reflects a rate-limit failure.
///
/// Matches three layers:
/// - typed `RATELIMIT_ERROR` code (signal-cli >= v0.14.3, plain
///   RateLimitException),
/// - legacy `[429]` / `RateLimitException` substrings,
/// - libsignal-net's `RetryLaterException` / `Retry after N seconds` surfaced
///   inside AttachmentInvalidException during attachment upload.
pub fn is_signal_rate_limit_error(err: &SignalRpcError) -> bool {
    match err {
        SignalRpcError::Dict { code, message, .. } => {
            if *code == Some(SIGNAL_RPC_ERROR_RATELIMIT) {
                return true;
            }
            contains_rate_limit_substring(&message.clone().unwrap_or_default())
        }
        SignalRpcError::Text(s) => contains_rate_limit_substring(s),
    }
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

/// Round half to even (Python's `round()` semantics), for non-negative inputs.
///
/// `_format_wait` clamps its argument to `>= 0`, so only the non-negative case
/// is needed. Rust's `f64::round` rounds half away from zero, which would give
/// `0.5 -> 1` and `2.5 -> 3`; Python gives `0` and `2`.
fn py_round(x: f64) -> f64 {
    let floor = x.floor();
    let diff = x - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else if (floor as i64) % 2 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

/// Human-friendly wait label for user-facing pacing notices.
pub fn format_wait(seconds: f64) -> String {
    let s = seconds.max(0.0);
    if s < 90.0 {
        format!("{}s", py_round(s) as i64)
    } else {
        let mins = (py_round(s / 60.0) as i64).max(1);
        format!("{} min", mins)
    }
}

/// HTTP timeout (seconds) for a Signal `send` RPC.
///
/// signal-cli uploads attachments serially during the call, so server-side time
/// scales with batch size. Default 30s is fine for text-only sends but truncates
/// large attachment batches mid-upload. Scale at 5s/attachment with a 60s floor.
pub fn signal_send_timeout(num_attachments: i64) -> f64 {
    if num_attachments <= 0 {
        return 30.0;
    }
    (5.0 * num_attachments as f64).max(60.0)
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// Read-only snapshot of scheduler state for diagnostic logging.
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulerStateSnapshot {
    pub tokens: f64,
    pub capacity: i64,
    pub refill_rate: f64,
    pub refill_seconds_per_token: f64,
}

struct Inner {
    tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl Inner {
    // Mirrors Python `_refill`: credit elapsed*rate, capped at capacity, and
    // advance last_refill to now.
    fn refill(&mut self, capacity: f64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 && self.tokens < capacity {
            self.tokens = capacity.min(self.tokens + elapsed * self.refill_rate);
        }
        self.last_refill = now;
    }
}

/// Process-wide token-bucket simulator for Signal attachment sends.
///
/// The bucket holds up to `capacity` tokens (default 50). Each attachment
/// consumes one token. Tokens refill at `refill_rate` tokens/second, calibrated
/// from the per-token Retry-After hint the server returns on a 429. Until one is
/// observed the documented default (1 token / 4 seconds) is used.
pub struct SignalAttachmentScheduler {
    capacity: f64,
    inner: Mutex<Inner>,
}

impl SignalAttachmentScheduler {
    pub fn new(capacity: f64, default_retry_after: f64) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner {
                tokens: capacity,
                refill_rate: 1.0 / default_retry_after,
                last_refill: Instant::now(),
            }),
        }
    }

    pub fn new_default() -> Self {
        Self::new(
            SIGNAL_RATE_LIMIT_BUCKET_CAPACITY as f64,
            SIGNAL_RATE_LIMIT_DEFAULT_RETRY_AFTER as f64,
        )
    }

    /// Best-effort estimate of the seconds until `n` tokens would be available.
    ///
    /// Used to decide whether to emit a user-facing pacing notice before an
    /// `acquire` that may block silently. Small races vs concurrent acquires are
    /// benign for an informational notice.
    pub fn estimate_wait(&self, n: i64) -> f64 {
        let inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last_refill).as_secs_f64();
        let mut projected = inner.tokens;
        if elapsed > 0.0 && projected < self.capacity {
            projected = self.capacity.min(projected + elapsed * inner.refill_rate);
        }
        let deficit = n as f64 - projected;
        if deficit <= 0.0 {
            return 0.0;
        }
        deficit / inner.refill_rate
    }

    /// Block until at least `n` tokens are available, return the seconds slept.
    ///
    /// Does not deduct tokens - the bucket is a read-only model of server-side
    /// capacity. Call `report_rpc_duration` after the RPC to synchronise the
    /// model with the server timeline. The lock is released during the sleep so
    /// other callers interleave; the loop re-checks after each sleep.
    pub async fn acquire(&self, n: i64) -> Result<f64, SignalSchedulerError> {
        if n <= 0 {
            return Ok(0.0);
        }
        if n as f64 > self.capacity {
            return Err(SignalSchedulerError::new(format!(
                "Signal scheduler was called requesting {} tokens (max is {})",
                n, self.capacity
            )));
        }

        let mut total_slept = 0.0;
        let mut first_pass = true;
        loop {
            let wait;
            {
                let mut inner = self.inner.lock().unwrap();
                inner.refill(self.capacity);
                if inner.tokens >= n as f64 {
                    if !first_pass || total_slept > 0.0 {
                        debug!(
                            "Signal scheduler: tokens sufficient for {} (remaining={:.1}, total_slept={:.1}s)",
                            n, inner.tokens, total_slept,
                        );
                    }
                    return Ok(total_slept);
                }
                let deficit = n as f64 - inner.tokens;
                wait = deficit / inner.refill_rate;
                if first_pass {
                    info!(
                        "Signal scheduler: pausing {:.1}s for {} tokens (available={:.1}, deficit={:.1}, refill={:.4}/s \u{2248} {:.1}s/token)",
                        wait, n, inner.tokens, deficit, inner.refill_rate, 1.0 / inner.refill_rate,
                    );
                    first_pass = false;
                }
            }
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            total_slept += wait;
        }
    }

    /// Record an attachment-send RPC that just completed.
    ///
    /// Deducts `n_attachments` tokens without crediting refill during the upload
    /// window (Signal checks the bucket at RPC start and does not refill during
    /// request processing). Advances `last_refill` so the next acquire/refill
    /// counts from here.
    ///
    /// Async to match the Python signature (which awaits an asyncio.Lock); the
    /// critical section itself is synchronous here.
    pub async fn report_rpc_duration(&self, rpc_duration: f64, n_attachments: i64) {
        if n_attachments <= 0 {
            return;
        }

        let token_before;
        let token_after;
        let refill_rate;
        {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            token_before = inner.tokens;
            inner.tokens = (token_before - n_attachments as f64).max(0.0);
            inner.last_refill = now;
            token_after = inner.tokens;
            refill_rate = inner.refill_rate;
        }
        let msg = format!(
            "Signal scheduler: RPC for {} att took {:.1}s - tokens {:.1} \u{2192} {:.1} (deducted={}, no upload refill credited, refill={:.4}s\u{207b}\u{00b9})",
            n_attachments, rpc_duration, token_before, token_after, n_attachments, refill_rate,
        );
        if rpc_duration > 10.0 && n_attachments > 5 {
            info!("{}", msg);
        } else {
            debug!("{}", msg);
        }
    }

    /// Apply server feedback after a 429.
    ///
    /// `retry_after` is the per-token refill window the server reports (`None`
    /// when signal-cli is older than v0.14.3). When present and positive it
    /// recalibrates `refill_rate`; the bucket is then drained to zero.
    pub fn feedback(&self, retry_after: Option<f64>, _n_attempted: i64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ra) = retry_after {
            if ra > 0.0 {
                let new_rate = 1.0 / ra;
                if new_rate != inner.refill_rate {
                    info!(
                        "Signal scheduler: calibrating refill_rate to {:.4} tokens/sec (server retry_after={:.1}s per token)",
                        new_rate, ra,
                    );
                    inner.refill_rate = new_rate;
                }
            }
        }
        inner.tokens = 0.0;
        inner.last_refill = Instant::now();
    }

    /// Current scheduler state for diagnostic logging (read-only).
    ///
    /// Does not advance `last_refill`, so it is safe to call from logging paths.
    pub fn state(&self) -> SchedulerStateSnapshot {
        let inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last_refill).as_secs_f64();
        let mut projected = inner.tokens;
        if elapsed > 0.0 && projected < self.capacity {
            projected = self.capacity.min(projected + elapsed * inner.refill_rate);
        }
        let refill_seconds_per_token = if inner.refill_rate > 0.0 {
            round1(1.0 / inner.refill_rate)
        } else {
            f64::INFINITY
        };
        SchedulerStateSnapshot {
            tokens: round1(projected),
            capacity: self.capacity as i64,
            refill_rate: round4(inner.refill_rate),
            refill_seconds_per_token,
        }
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

// ---------------------------------------------------------------------------
// Process-wide singleton
// ---------------------------------------------------------------------------

// Python holds `_scheduler: Optional[SignalAttachmentScheduler]` as a module
// global. A `Mutex<Option<Arc<..>>>` gives the same lazy-create + test-reset
// shape (a bare OnceLock could not be reset).
static SCHEDULER: Mutex<Option<Arc<SignalAttachmentScheduler>>> = Mutex::new(None);

/// Return the process-wide scheduler, creating it on first access.
pub fn get_scheduler() -> Arc<SignalAttachmentScheduler> {
    let mut guard = SCHEDULER.lock().unwrap();
    if guard.is_none() {
        let scheduler = Arc::new(SignalAttachmentScheduler::new_default());
        let (capacity, refill_rate) = {
            let inner = scheduler.inner.lock().unwrap();
            (scheduler.capacity, inner.refill_rate)
        };
        info!(
            "Signal scheduler: created (capacity={} tokens, refill={:.4}/s \u{2248} {:.1}s/token)",
            capacity as i64,
            refill_rate,
            1.0 / refill_rate,
        );
        *guard = Some(scheduler);
    }
    guard.as_ref().unwrap().clone()
}

/// Drop the cached scheduler so the next `get_scheduler` builds a fresh one.
/// Test-only - never call from production paths.
pub fn reset_scheduler() {
    *SCHEDULER.lock().unwrap() = None;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Golden values captured from the Python module:
    //   _format_wait: 0.0->0s, -5.0->0s, 10.0->10s, 45.4->45s, 45.5->46s,
    //     89.0->89s, 89.9->90s, 90.0->2 min, 91.0->2 min, 120.0->2 min,
    //     150.0->2 min, 3600.0->60 min, 29.5->30s, 0.5->0s.
    #[test]
    fn format_wait_golden() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0s"),
            (-5.0, "0s"),
            (10.0, "10s"),
            (45.4, "45s"),
            (45.5, "46s"),
            (89.0, "89s"),
            (89.9, "90s"),
            (90.0, "2 min"),
            (90.4, "2 min"),
            (91.0, "2 min"),
            (120.0, "2 min"),
            (150.0, "2 min"),
            (3600.0, "60 min"),
            (29.5, "30s"),
            (0.5, "0s"),
        ];
        for (input, expected) in cases {
            assert_eq!(format_wait(*input), *expected, "format_wait({input})");
        }
    }

    // Golden: -1->30, 0->30, 1->60, 3->60, 5->60, 11->60, 12->60, 13->65,
    //   100->500.
    #[test]
    fn signal_send_timeout_golden() {
        let cases: &[(i64, f64)] = &[
            (-1, 30.0),
            (0, 30.0),
            (1, 60.0),
            (3, 60.0),
            (5, 60.0),
            (11, 60.0),
            (12, 60.0),
            (13, 65.0),
            (100, 500.0),
        ];
        for (n, expected) in cases {
            assert_eq!(
                signal_send_timeout(*n),
                *expected,
                "signal_send_timeout({n})"
            );
        }
    }

    fn dict(
        code: Option<i64>,
        message: Option<&str>,
        results: Vec<RetryAfterValue>,
    ) -> SignalRpcError {
        SignalRpcError::Dict {
            code,
            message: message.map(|s| s.to_string()),
            results,
        }
    }

    // Golden truth table from Python's _is_signal_rate_limit_error.
    #[test]
    fn is_signal_rate_limit_error_golden() {
        assert!(is_signal_rate_limit_error(&dict(Some(-5), None, vec![])));
        assert!(is_signal_rate_limit_error(&dict(
            Some(-5),
            Some("x"),
            vec![]
        )));
        assert!(is_signal_rate_limit_error(&dict(
            None,
            Some("[429] too many"),
            vec![]
        )));
        assert!(is_signal_rate_limit_error(&dict(
            None,
            Some("RateLimitException"),
            vec![]
        )));
        assert!(is_signal_rate_limit_error(&dict(
            None,
            Some("RetryLaterException"),
            vec![]
        )));
        assert!(is_signal_rate_limit_error(&dict(
            None,
            Some("retry after 4 seconds"),
            vec![]
        )));
        assert!(!is_signal_rate_limit_error(&dict(
            None,
            Some("nothing"),
            vec![]
        )));
        assert!(!is_signal_rate_limit_error(&dict(
            Some(-1),
            Some("ok"),
            vec![]
        )));
        assert!(is_signal_rate_limit_error(&SignalRpcError::Text(
            "[429]".into()
        )));
        assert!(!is_signal_rate_limit_error(&SignalRpcError::Text(
            "plain string".into()
        )));
        assert!(is_signal_rate_limit_error(&SignalRpcError::Text(
            "RATELIMIT here".into()
        )));
    }

    // Golden from Python's _extract_retry_after_seconds.
    #[test]
    fn extract_retry_after_seconds_golden() {
        // results with two structured values -> max.
        assert_eq!(
            extract_retry_after_seconds(&dict(
                None,
                None,
                vec![RetryAfterValue::Int(4), RetryAfterValue::Int(6)]
            )),
            Some(6.0)
        );
        // empty results, message regex -> 8.0.
        assert_eq!(
            extract_retry_after_seconds(&dict(None, Some("Retry after 8 seconds"), vec![])),
            Some(8.0)
        );
        // message-only dict, fractional -> 2.5.
        assert_eq!(
            extract_retry_after_seconds(&dict(None, Some("Retry after 2.5 second"), vec![])),
            Some(2.5)
        );
        // plain text -> 10.0.
        assert_eq!(
            extract_retry_after_seconds(&SignalRpcError::Text("Retry after 10 seconds".into())),
            Some(10.0)
        );
        // no retry info -> None.
        assert_eq!(
            extract_retry_after_seconds(&SignalRpcError::Text("no retry info".into())),
            None
        );
        // null structured value is skipped, falls through to message regex -> 3.0.
        assert_eq!(
            extract_retry_after_seconds(&dict(
                None,
                Some("Retry after 3 seconds"),
                vec![RetryAfterValue::Absent]
            )),
            Some(3.0)
        );
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn acquire_on_full_bucket_does_not_sleep() {
        let sched = SignalAttachmentScheduler::new_default();
        // Full bucket (50 tokens) has enough for 5, so no sleep.
        let slept = block_on(sched.acquire(5)).unwrap();
        assert_eq!(slept, 0.0);
        // n <= 0 short-circuits.
        assert_eq!(block_on(sched.acquire(0)).unwrap(), 0.0);
    }

    #[test]
    fn acquire_over_capacity_errors() {
        let sched = SignalAttachmentScheduler::new_default();
        let err = block_on(sched.acquire(51)).unwrap_err();
        assert!(err.message.contains("requesting 51 tokens"));
        assert!(err.message.contains("max is 50"));
    }

    #[test]
    fn report_rpc_duration_deducts_tokens() {
        let sched = SignalAttachmentScheduler::new_default();
        block_on(sched.report_rpc_duration(1.0, 10));
        // Started at 50, deducted 10; tiny refill may creep in, so bound it.
        let state = sched.state();
        assert!(
            state.tokens >= 40.0 && state.tokens < 41.0,
            "tokens={}",
            state.tokens
        );
        assert_eq!(state.capacity, 50);
    }

    #[test]
    fn feedback_calibrates_and_drains() {
        let sched = SignalAttachmentScheduler::new_default();
        // Default refill_rate is 1/4 = 0.25.
        assert_eq!(sched.state().refill_rate, 0.25);
        // Server says 2s per token -> rate 0.5, bucket drained to 0.
        sched.feedback(Some(2.0), 5);
        assert_eq!(sched.state().refill_rate, 0.5);
        // Just drained, so ~1 token needs ~2s (allow slack for elapsed time).
        let wait = sched.estimate_wait(1);
        assert!(wait > 1.5 && wait <= 2.0, "estimate_wait(1)={wait}");
        // Falsy retry_after (0.0) does not recalibrate.
        sched.feedback(Some(0.0), 1);
        assert_eq!(sched.state().refill_rate, 0.5);
        // None does not recalibrate either.
        sched.feedback(None, 1);
        assert_eq!(sched.state().refill_rate, 0.5);
    }

    #[test]
    fn get_scheduler_is_singleton() {
        reset_scheduler();
        let a = get_scheduler();
        let b = get_scheduler();
        assert!(Arc::ptr_eq(&a, &b));
        reset_scheduler();
        let c = get_scheduler();
        assert!(!Arc::ptr_eq(&a, &c));
        reset_scheduler();
    }
}
