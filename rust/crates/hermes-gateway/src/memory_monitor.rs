//! Port of gateway/memory_monitor.py.
//!
// Public API is ahead of its callers (startup wires the periodic monitor).
#![allow(dead_code)]
//!
//! Periodic process memory logging. The gateway is long-lived and accumulates
//! memory (cached agents, transcripts, tool schemas, MCP connections); a slow
//! leak is invisible in a single log line. This emits one grep-friendly
//! `[MEMORY] ...` line every N minutes (default 5) plus a baseline at start and
//! a final snapshot at shutdown, so a maintainer can grep a time series of RSS.
//!
//! Rust differences from the Python original: RSS comes from `getrusage`
//! `ru_maxrss` (the same high-water-mark source Python uses), and the Python-
//! only `gc`/thread-count fields are dropped (no Rust GC).

use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Current process RSS in MB (`getrusage` `ru_maxrss` high-water mark), or
/// `None` when unavailable.
#[cfg(unix)]
pub fn rss_mb() -> Option<i64> {
    // SAFETY: getrusage fills a zeroed struct we own.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    let maxrss = usage.ru_maxrss as i64;
    if maxrss <= 0 {
        return None;
    }
    // ru_maxrss is KB on Linux, bytes on macOS.
    #[cfg(target_os = "macos")]
    {
        Some(maxrss / (1024 * 1024))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(maxrss / 1024)
    }
}

#[cfg(not(unix))]
pub fn rss_mb() -> Option<i64> {
    None
}

/// Log current memory usage as a grep-friendly `[MEMORY] ...` line. `prefix` is
/// an optional tag (e.g. `baseline`, `shutdown`). `uptime_s` is the process
/// uptime in seconds.
pub fn log_memory_usage(prefix: &str, uptime_s: u64) {
    let tag = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix} ")
    };
    match rss_mb() {
        Some(rss) => {
            tracing::info!(target: "hermes_gateway", "[MEMORY] {tag}rss={rss}MB uptime={uptime_s}s")
        }
        None => {
            tracing::info!(target: "hermes_gateway", "[MEMORY] {tag}rss=unavailable uptime={uptime_s}s")
        }
    }
}

/// Start periodic memory logging on a background task: a baseline immediately,
/// then every `interval` until `shutdown` is cancelled (which logs a final
/// snapshot). Returns `false` without spawning when RSS can't be read at all.
pub fn start_memory_monitoring(interval: Duration, shutdown: CancellationToken) -> bool {
    if rss_mb().is_none() {
        tracing::warn!(
            "[MEMORY] memory monitoring unavailable: getrusage could not read RSS; skipping"
        );
        return false;
    }
    let start = std::time::Instant::now();
    log_memory_usage("baseline", 0);
    tracing::info!(
        "[MEMORY] periodic memory monitoring started (interval: {}s)",
        interval.as_secs()
    );

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Skip the immediate first tick; we already logged the baseline.
        tick.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    log_memory_usage("shutdown", start.elapsed().as_secs());
                    tracing::info!("[MEMORY] periodic memory monitoring stopped");
                    break;
                }
                _ = tick.tick() => {
                    log_memory_usage("", start.elapsed().as_secs());
                }
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn rss_is_readable() {
        // This test process has a real RSS.
        assert!(rss_mb().map(|v| v > 0).unwrap_or(false));
    }

    #[test]
    fn log_does_not_panic() {
        log_memory_usage("baseline", 0);
        log_memory_usage("", 42);
    }

    #[tokio::test]
    async fn monitor_starts_and_stops() {
        let token = CancellationToken::new();
        let started = start_memory_monitoring(Duration::from_secs(300), token.clone());
        // On a unix CI host RSS is readable, so it starts.
        #[cfg(unix)]
        assert!(started);
        // Cancelling lets the task log its shutdown snapshot and exit.
        token.cancel();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = started;
    }
}
