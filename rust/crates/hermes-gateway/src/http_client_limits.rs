//! Port of gateway/platforms/_http_client_limits.py.
//!
// Public API is ahead of its callers (the persistent-client adapters are not
// all ported yet).
#![allow(dead_code)]
//!
//! Shared connection-pool tuning for long-lived platform adapters. Gateway
//! messaging platforms (QQ Bot, Feishu, WeCom, DingTalk, Signal, BlueBubbles)
//! keep a persistent HTTP client alive for the adapter's lifetime, which
//! amortises TLS/connection setup but makes the process's file-descriptor
//! pressure sensitive to how aggressively the pool recycles idle keep-alive
//! connections.
//!
//! Python returns an `httpx.Limits`. The Rust gateway uses `reqwest`, whose
//! equivalent knobs are `pool_max_idle_per_host` (the keep-alive cap) and
//! `pool_idle_timeout` (the keep-alive expiry). [`platform_http_limits`]
//! resolves the same two values, honoring the same env overrides, so an adapter
//! builder can apply them to its `reqwest::ClientBuilder`.
//!
//! Defaults: `max_keepalive_connections = 10` (plenty for any single adapter;
//! platform APIs rarely parallelise beyond this) and `keepalive_expiry = 2.0s`
//! (close idle sockets aggressively so a proxy's lingering CLOSE_WAIT window
//! can't starve the process's fd budget). Override with
//! `HERMES_GATEWAY_HTTPX_KEEPALIVE_EXPIRY` / `HERMES_GATEWAY_HTTPX_MAX_KEEPALIVE`.

use std::time::Duration;

const DEFAULT_KEEPALIVE_EXPIRY_S: f64 = 2.0;
const DEFAULT_MAX_KEEPALIVE: u64 = 10;

/// Connection-pool limits for a persistent platform-adapter HTTP client.
///
/// Mirrors the two fields the Python helper set on `httpx.Limits`
/// (`max_connections` is left at the client default, as in Python).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlatformHttpLimits {
    /// Max idle keep-alive connections to retain (httpx
    /// `max_keepalive_connections`; reqwest `pool_max_idle_per_host`).
    pub max_keepalive_connections: u64,
    /// How long an idle keep-alive connection may live, in seconds (httpx
    /// `keepalive_expiry`; reqwest `pool_idle_timeout`).
    pub keepalive_expiry_s: f64,
}

impl PlatformHttpLimits {
    /// The keep-alive expiry as a [`Duration`], for `reqwest`'s
    /// `pool_idle_timeout`.
    pub fn keepalive_expiry(&self) -> Duration {
        Duration::from_secs_f64(self.keepalive_expiry_s)
    }
}

/// Read `name` as a positive float, falling back to `default` when the var is
/// absent, empty, unparseable, or not `> 0`. Matches Python's `_env_float`.
fn env_float(name: &str, default: f64) -> f64 {
    let raw = std::env::var(name).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    // Python's float() accepts inf and rejects a bare "nan" as a value here only
    // through the `> 0` gate; a NaN parse compares false to 0 and falls back.
    match raw.parse::<f64>() {
        Ok(val) if val > 0.0 => val,
        _ => default,
    }
}

/// Read `name` as a positive integer, falling back to `default` when the var is
/// absent, empty, unparseable, or not `> 0`. Matches Python's `_env_int`.
///
/// Python's `int("3.5")` raises, so a non-integer string falls back to the
/// default (unlike `float`); this uses an integer parse to preserve that.
fn env_int(name: &str, default: u64) -> u64 {
    let raw = std::env::var(name).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<i64>() {
        Ok(val) if val > 0 => val as u64,
        _ => default,
    }
}

/// Connection-pool limits tuned for persistent platform-adapter clients.
///
/// Unlike Python (which returns `None` when `httpx` is not importable so callers
/// fall back to the httpx default), `reqwest` is always available here, so this
/// always returns concrete limits.
pub fn platform_http_limits() -> PlatformHttpLimits {
    PlatformHttpLimits {
        max_keepalive_connections: env_int(
            "HERMES_GATEWAY_HTTPX_MAX_KEEPALIVE",
            DEFAULT_MAX_KEEPALIVE,
        ),
        keepalive_expiry_s: env_float(
            "HERMES_GATEWAY_HTTPX_KEEPALIVE_EXPIRY",
            DEFAULT_KEEPALIVE_EXPIRY_S,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // env is process-global; serialize the tests that mutate it. The guard is
    // held for the whole body of each test (never nested), so a single
    // non-reentrant Mutex is enough.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set (Some) or clear (None) a var for the duration of `f`, restoring it
    /// after. The caller must already hold ENV_LOCK (do not lock inside here, or
    /// nested calls on one thread would deadlock).
    fn scoped_var<T>(name: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var(name).ok();
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
        let out = f();
        match prev {
            Some(p) => std::env::set_var(name, p),
            None => std::env::remove_var(name),
        }
        out
    }

    #[test]
    fn defaults_when_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        scoped_var("HERMES_GATEWAY_HTTPX_MAX_KEEPALIVE", None, || {
            scoped_var("HERMES_GATEWAY_HTTPX_KEEPALIVE_EXPIRY", None, || {
                let l = platform_http_limits();
                assert_eq!(l.max_keepalive_connections, 10);
                assert_eq!(l.keepalive_expiry_s, 2.0);
            });
        });
    }

    #[test]
    fn env_float_parsing_matches_python() {
        let _g = ENV_LOCK.lock().unwrap();
        // Present and positive -> used.
        scoped_var("X_TEST_F", Some("3.5"), || {
            assert_eq!(env_float("X_TEST_F", 2.0), 3.5)
        });
        // Whitespace trimmed.
        scoped_var("X_TEST_F", Some("  4.0  "), || {
            assert_eq!(env_float("X_TEST_F", 2.0), 4.0)
        });
        // Empty, unparseable, zero, negative -> default.
        scoped_var("X_TEST_F", Some(""), || {
            assert_eq!(env_float("X_TEST_F", 2.0), 2.0)
        });
        scoped_var("X_TEST_F", Some("abc"), || {
            assert_eq!(env_float("X_TEST_F", 2.0), 2.0)
        });
        scoped_var("X_TEST_F", Some("0"), || {
            assert_eq!(env_float("X_TEST_F", 2.0), 2.0)
        });
        scoped_var("X_TEST_F", Some("-1.5"), || {
            assert_eq!(env_float("X_TEST_F", 2.0), 2.0)
        });
    }

    #[test]
    fn env_int_parsing_matches_python() {
        let _g = ENV_LOCK.lock().unwrap();
        scoped_var("X_TEST_I", Some("25"), || {
            assert_eq!(env_int("X_TEST_I", 10), 25)
        });
        // Python int("3.5") raises -> falls back to default.
        scoped_var("X_TEST_I", Some("3.5"), || {
            assert_eq!(env_int("X_TEST_I", 10), 10)
        });
        scoped_var("X_TEST_I", Some(""), || {
            assert_eq!(env_int("X_TEST_I", 10), 10)
        });
        scoped_var("X_TEST_I", Some("0"), || {
            assert_eq!(env_int("X_TEST_I", 10), 10)
        });
        scoped_var("X_TEST_I", Some("-4"), || {
            assert_eq!(env_int("X_TEST_I", 10), 10)
        });
    }

    #[test]
    fn keepalive_expiry_duration() {
        let l = PlatformHttpLimits {
            max_keepalive_connections: 10,
            keepalive_expiry_s: 2.0,
        };
        assert_eq!(l.keepalive_expiry(), Duration::from_millis(2000));
    }
}
