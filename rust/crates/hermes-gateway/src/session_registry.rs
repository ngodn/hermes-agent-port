//! Port of the GatewayRunner session-map and run-generation accessors from
//! gateway/run.py (`_sessions_map`, `_session_state`, `_peek_session_state`,
//! `_is_session_running`, `_running_agent_items`, `_begin_session_run_generation`,
//! `_invalidate_session_run_generation`, `_is_session_run_current`).
//!
// Public API is ahead of its callers: the turn pipeline that consumes these is
// ported in later tiers.
#![allow(dead_code)]
//!
//! This is Tier 1 of the runner port (see rust/analysis/run-py-map.md): the
//! per-session state container plus the monotonic run-generation tokens that
//! every later tier depends on. It owns the map; the data model itself lives in
//! [`crate::session_state`].
//!
//! RUN GENERATIONS. Every top-level gateway turn claims a monotonically
//! increasing token. If a later command (`/stop`, `/new`) invalidates that token
//! while the old worker is still unwinding, the late result is recognized as
//! stale and dropped instead of bleeding into the fresh session. The counter is
//! monotonic BY DESIGN and is never reset: resetting it would let a stale run
//! compare equal to a fresh one.
//!
//! Python lazily created the map so bare test runners built via
//! `object.__new__` still worked; Rust construction always runs, so the registry
//! simply starts empty. Entries are never evicted, matching the Python dicts.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::session_state::{AgentSlot, SessionState};

/// The per-session state map plus its run-generation tokens.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked sessions.
    pub fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.lock().unwrap().is_empty()
    }

    /// Whether a state exists for `session_key` (without creating one).
    pub fn contains(&self, session_key: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(session_key)
    }

    /// Get-or-create the [`SessionState`] for `session_key` and run `f` on it.
    ///
    /// Mirrors `_session_state`. The closure form keeps the state behind the
    /// lock rather than handing out a clone, so callers cannot mutate a copy and
    /// silently lose the write.
    pub fn with_session<R>(&self, session_key: &str, f: impl FnOnce(&mut SessionState) -> R) -> R {
        let mut sessions = self.sessions.lock().unwrap();
        let state = sessions.entry(session_key.to_string()).or_default();
        f(state)
    }

    /// Run `f` on the existing state for `session_key`, or return `None` when
    /// there is none. Mirrors `_peek_session_state` (never creates).
    pub fn peek<R>(&self, session_key: &str, f: impl FnOnce(&SessionState) -> R) -> Option<R> {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(session_key).map(f)
    }

    /// True when the session holds a running-turn slot (agent or sentinel).
    ///
    /// Mirrors `_is_session_running`: Python tests `state.turn.agent is not
    /// None`, and [`AgentSlot::Idle`] is this port's stand-in for that `None`.
    pub fn is_session_running(&self, session_key: &str) -> bool {
        self.peek(session_key, |s| s.turn.agent != AgentSlot::Idle)
            .unwrap_or(false)
    }

    /// Session keys whose turn slot is occupied (including pending sentinels).
    ///
    /// Mirrors `_running_agent_items`, which returns `(key, agent)` pairs; the
    /// agent handle itself is not modelled yet, so this returns the keys.
    pub fn running_agent_keys(&self) -> Vec<String> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .iter()
            .filter(|(_, s)| s.turn.agent != AgentSlot::Idle)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Claim a fresh run-generation token for `session_key`.
    ///
    /// Mirrors `_begin_session_run_generation`. An empty key returns 0 and
    /// creates nothing, exactly as Python's `if not session_key: return 0`.
    pub fn begin_run_generation(&self, session_key: &str) -> u64 {
        if session_key.is_empty() {
            return 0;
        }
        self.with_session(session_key, |s| {
            s.persistent.run_generation += 1;
            s.persistent.run_generation
        })
    }

    /// Invalidate any in-flight run token for `session_key`.
    ///
    /// Mirrors `_invalidate_session_run_generation`: it simply claims the next
    /// generation (so anything holding the old one is now stale) and logs when a
    /// reason is supplied.
    pub fn invalidate_run_generation(&self, session_key: &str, reason: &str) -> u64 {
        let generation = self.begin_run_generation(session_key);
        if !reason.is_empty() {
            tracing::info!(
                session_key,
                generation,
                reason,
                "Invalidated run generation"
            );
        }
        generation
    }

    /// Whether `generation` is still current for `session_key`.
    ///
    /// Mirrors `_is_session_run_current`. Two faithful edge cases: an empty key
    /// is always current (there is nothing to invalidate), and a session with no
    /// state yet has a current generation of 0.
    pub fn is_run_current(&self, session_key: &str, generation: u64) -> bool {
        if session_key.is_empty() {
            return true;
        }
        let current = self
            .peek(session_key, |s| s.persistent.run_generation)
            .unwrap_or(0);
        current == generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_is_created_on_demand_and_reused() {
        let reg = SessionRegistry::new();
        assert!(reg.is_empty());
        assert!(!reg.contains("s1"));

        reg.with_session("s1", |s| s.persistent.run_generation = 7);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("s1"));
        // The same entry is handed back, not a fresh default.
        assert_eq!(reg.with_session("s1", |s| s.persistent.run_generation), 7);
    }

    #[test]
    fn peek_never_creates() {
        let reg = SessionRegistry::new();
        assert!(reg.peek("ghost", |_| ()).is_none());
        assert!(!reg.contains("ghost"));
        assert!(reg.is_empty());
    }

    #[test]
    fn run_generation_is_monotonic_and_never_reset() {
        let reg = SessionRegistry::new();
        assert_eq!(reg.begin_run_generation("s"), 1);
        assert_eq!(reg.begin_run_generation("s"), 2);
        assert_eq!(reg.begin_run_generation("s"), 3);
        // Clearing the conversation scope must not touch the persistent counter.
        reg.with_session("s", |st| st.conversation.clear());
        assert_eq!(reg.begin_run_generation("s"), 4);
    }

    #[test]
    fn empty_key_returns_zero_and_creates_nothing() {
        // Python: `if not session_key: return 0`.
        let reg = SessionRegistry::new();
        assert_eq!(reg.begin_run_generation(""), 0);
        assert_eq!(reg.invalidate_run_generation("", "reason"), 0);
        assert!(reg.is_empty());
    }

    #[test]
    fn is_run_current_edge_cases_match_python() {
        let reg = SessionRegistry::new();
        // Empty key is always current: there is nothing to invalidate.
        assert!(reg.is_run_current("", 0));
        assert!(reg.is_run_current("", 99));
        // A session with no state has a current generation of 0.
        assert!(reg.is_run_current("unknown", 0));
        assert!(!reg.is_run_current("unknown", 1));

        let g = reg.begin_run_generation("s");
        assert!(reg.is_run_current("s", g));
        assert!(!reg.is_run_current("s", g - 1));
    }

    #[test]
    fn invalidating_makes_the_previous_token_stale() {
        // The whole point: a worker holding `g` must be detectable as stale
        // after /stop or /new bumps the generation.
        let reg = SessionRegistry::new();
        let g = reg.begin_run_generation("s");
        assert!(reg.is_run_current("s", g));

        let g2 = reg.invalidate_run_generation("s", "/stop");
        assert_eq!(g2, g + 1);
        assert!(!reg.is_run_current("s", g));
        assert!(reg.is_run_current("s", g2));
    }

    #[test]
    fn running_tracks_the_turn_slot() {
        let reg = SessionRegistry::new();
        assert!(!reg.is_session_running("s"));
        // Creating state alone does not make it running (agent stays Idle).
        reg.with_session("s", |_| {});
        assert!(!reg.is_session_running("s"));
        assert!(reg.running_agent_keys().is_empty());

        reg.with_session("s", |st| st.turn.agent = AgentSlot::Running);
        assert!(reg.is_session_running("s"));
        assert_eq!(reg.running_agent_keys(), vec!["s".to_string()]);

        // A pending sentinel also counts as occupied (Python: `is not None`).
        reg.with_session("t", |st| st.turn.agent = AgentSlot::Pending);
        assert!(reg.is_session_running("t"));
        let mut keys = reg.running_agent_keys();
        keys.sort();
        assert_eq!(keys, vec!["s".to_string(), "t".to_string()]);

        reg.with_session("s", |st| st.turn.clear());
        assert!(!reg.is_session_running("s"));
    }
}
