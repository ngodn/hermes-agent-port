//! Port of gateway/restart_loop_guard.py.
//!
// Public API is ahead of its callers (the boot/auto-resume path wires it).
#![allow(dead_code)]
//!
//! Auto-resume restart-loop breaker (#30719, #81642). A repeated crash can
//! drive the supervisor into a respawn loop where each boot auto-resumes the
//! restart-interrupted session, whose next turn re-runs the offending logic.
//! This records a timestamp each time the gateway boots with restart-interrupted
//! sessions pending, chains consecutive such boots while the inter-boot gap
//! stays within `max_gap_seconds` (so slow crash cycles trip too), and reports
//! the loop as tripped once `max_restarts` boots chain together. When tripped,
//! the caller SKIPS auto-resume, breaking the cycle while still serving real
//! traffic.
//!
//! State lives in `<HERMES_HOME>/gateway/restart_loop.json`, profile-scoped and
//! surviving process death. It is best-effort: any read/write failure fails OPEN
//! (no false trip) because a broken breaker must never wedge a healthy gateway.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Defaults: a legitimate operator restart or two never trips; the documented
/// ~10s respawn loop does within a few cycles.
pub const DEFAULT_MAX_RESTARTS: usize = 3;
pub const DEFAULT_WINDOW_SECONDS: i64 = 60;
/// Longest gap between two consecutive restart-interrupted boots that still
/// counts them as the same loop (#81642), making the breaker period-agnostic.
pub const DEFAULT_MAX_GAP_SECONDS: i64 = 300;
/// Cap the persisted chain so a long loop cannot grow the file without bound.
const MAX_STORED_BOOTS: usize = 50;

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    boots: Vec<f64>,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Effective inter-boot gap that still links two boots into one loop, floored by
/// `window_seconds` so widening the window never makes the breaker less sensitive.
pub fn chain_gap(window_seconds: i64, max_gap_seconds: i64) -> f64 {
    window_seconds.max(max_gap_seconds).max(1) as f64
}

/// The unbroken chain of boots leading up to `ts`. Walks backwards from `ts`,
/// keeping boots while each successive gap stays within `gap`; the first wider
/// gap ends the chain. A future-dated boot (clock moved backwards) is treated as
/// adjacent rather than dropping the whole chain.
pub fn chain_ending_at(boots: &[f64], ts: f64, gap: f64) -> Vec<f64> {
    let mut sorted: Vec<f64> = boots.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let mut chain = Vec::new();
    let mut prev = ts;
    for &t in &sorted {
        if t > ts {
            // Clock skew: treat the future entry as adjacent (don't move prev).
            chain.push(t);
            continue;
        }
        if prev - t > gap {
            break;
        }
        chain.push(t);
        prev = t;
    }
    chain.reverse();
    chain
}

/// The restart-loop breaker over its state file.
pub struct RestartLoopGuard {
    path: PathBuf,
}

impl RestartLoopGuard {
    /// Open at `<HERMES_HOME>/gateway/restart_loop.json`.
    pub fn open_default() -> Self {
        let path = crate::config_file::hermes_home()
            .join("gateway")
            .join("restart_loop.json");
        Self { path }
    }

    pub fn open(path: PathBuf) -> Self {
        Self { path }
    }

    fn load_boots(&self) -> Vec<f64> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        serde_json::from_str::<State>(&text)
            .map(|s| s.boots.into_iter().filter(|t| t.is_finite()).collect())
            .unwrap_or_default()
    }

    fn save_boots(&self, boots: &[f64]) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        if let Ok(json) = serde_json::to_string(&State {
            boots: boots.to_vec(),
        }) {
            let _ = std::fs::write(&self.path, json);
        }
    }

    /// Record that the gateway just booted with restart-interrupted sessions.
    /// Drops boots from an earlier already-broken chain and appends `now`.
    /// Returns the pruned+appended list (most recent last).
    pub fn record_boot(&self, window_seconds: i64, max_gap_seconds: i64, now: f64) -> Vec<f64> {
        let gap = chain_gap(window_seconds, max_gap_seconds);
        let mut boots = chain_ending_at(&self.load_boots(), now, gap);
        boots.push(now);
        let stored = if boots.len() > MAX_STORED_BOOTS {
            &boots[boots.len() - MAX_STORED_BOOTS..]
        } else {
            &boots[..]
        };
        self.save_boots(stored);
        boots
    }

    /// True if the chain ending at `now` has reached `max_restarts`. Fails OPEN.
    pub fn is_tripped(
        &self,
        max_restarts: usize,
        window_seconds: i64,
        max_gap_seconds: i64,
        now: f64,
    ) -> bool {
        if max_restarts == 0 {
            return false;
        }
        let gap = chain_gap(window_seconds, max_gap_seconds);
        chain_ending_at(&self.load_boots(), now, gap).len() >= max_restarts
    }

    /// Remove the persisted boot log (used on clean shutdown / by tests).
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// Record this boot and report whether the loop is now tripped. The single
    /// entry point: True means auto-resume should be SKIPPED to break the loop.
    pub fn check_and_record(
        &self,
        max_restarts: usize,
        window_seconds: i64,
        max_gap_seconds: i64,
        now: f64,
    ) -> bool {
        let boots = self.record_boot(window_seconds, max_gap_seconds, now);
        let tripped = max_restarts > 0 && boots.len() >= max_restarts;
        if tripped {
            tracing::warn!(
                chained = boots.len(),
                gap_s = chain_gap(window_seconds, max_gap_seconds) as i64,
                threshold = max_restarts,
                path = %self.path.display(),
                "restart-loop breaker TRIPPED: skipping auto-resume to break a suspected respawn loop"
            );
        }
        tripped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_rlg_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        p.push("gateway");
        p.push("restart_loop.json");
        p
    }

    #[test]
    fn chain_gap_is_floored_by_window() {
        assert_eq!(chain_gap(60, 300), 300.0);
        assert_eq!(chain_gap(600, 300), 600.0); // wider window wins
        assert_eq!(chain_gap(0, 0), 1.0); // floored at 1
    }

    #[test]
    fn chain_breaks_on_a_wide_gap() {
        // Boots at 0, 5, 10 then a big jump to 1000, 1005. Ending at 1005 with
        // gap 300: only 1000 and 1005 chain; the earlier cluster is dropped.
        let boots = [0.0, 5.0, 10.0, 1000.0, 1005.0];
        let chain = chain_ending_at(&boots, 1005.0, 300.0);
        assert_eq!(chain, vec![1000.0, 1005.0]);
    }

    #[test]
    fn contiguous_boots_all_chain() {
        let boots = [0.0, 100.0, 200.0, 300.0];
        let chain = chain_ending_at(&boots, 300.0, 300.0);
        assert_eq!(chain, vec![0.0, 100.0, 200.0, 300.0]);
    }

    #[test]
    fn future_boot_is_treated_as_adjacent() {
        // A boot after `ts` (clock stepped back) is kept, not dropped.
        let boots = [10.0, 20.0, 999.0];
        let chain = chain_ending_at(&boots, 20.0, 300.0);
        assert!(chain.contains(&999.0));
        assert!(chain.contains(&10.0) && chain.contains(&20.0));
    }

    #[test]
    fn trips_after_max_restarts_chained_boots() {
        let path = temp_path("trip");
        let g = RestartLoopGuard::open(path.clone());
        // Three fast boots within the gap -> tripped at the third.
        assert!(!g.check_and_record(3, 60, 300, 1000.0));
        assert!(!g.check_and_record(3, 60, 300, 1010.0));
        assert!(g.check_and_record(3, 60, 300, 1020.0));
        assert!(g.is_tripped(3, 60, 300, 1025.0));
        g.clear();
        assert!(!g.is_tripped(3, 60, 300, 1025.0));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_quiet_gap_resets_the_chain() {
        let path = temp_path("reset");
        let g = RestartLoopGuard::open(path.clone());
        g.check_and_record(3, 60, 300, 1000.0);
        g.check_and_record(3, 60, 300, 1010.0);
        // A long quiet period, then a single boot: the old chain is dropped, so
        // only this one boot chains -> not tripped.
        assert!(!g.check_and_record(3, 60, 300, 5000.0));
        assert_eq!(g.load_boots(), vec![5000.0]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn max_restarts_zero_never_trips() {
        let g = RestartLoopGuard::open(temp_path("zero"));
        assert!(!g.check_and_record(0, 60, 300, 1.0));
        assert!(!g.is_tripped(0, 60, 300, 1.0));
    }
}
