//! Per-session gateway state consolidated into one container.
//!
// Public state model is ahead of its callers while gateway/run.py is ported.
#![allow(dead_code)]
//!
//! Port of the data model in `gateway/session_state.py`. GatewayRunner used to
//! carry ~19 separate per-session dicts, each with its own ad-hoc lifecycle,
//! which bred boundary-drift, turn-release-drift and wholesale-reset races.
//! The fix groups all per-session state into one [`SessionState`] with three
//! lifecycle scopes, each with a single `clear()`:
//!
//! - [`TurnState`]        reset at the end of every running turn.
//! - [`ConversationState`] reset at conversation boundaries (/new, /resume,
//!   auto-reset, expiry, compression-exhausted reset).
//! - [`PersistentState`]  own lifecycles; `run_generation` is monotonic and
//!   NEVER reset (#28686).
//!
//! NOT ported: the Python file's second half (SessionFieldView,
//! TurnLeaseTokenView, LEGACY_FIELD_SPECS, legacy_*_property). Those are a
//! backward-compat shim that presents the new SessionState as the old loose
//! dict attributes so pre-refactor Python tests keep working. The Rust rewrite
//! has no such legacy attributes or tests, so reproducing that shim would be
//! porting dead scaffolding. Production code reaches state via the scopes
//! directly, which is what the Rust code does too.

use serde_json::Value;

/// Whether a turn's agent slot is idle, reserved, or actively running. Replaces
/// the Python tri-state `agent` field (None / _AGENT_PENDING_SENTINEL / live
/// AIAgent). The live agent handle is attached once the agent core is ported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentSlot {
    #[default]
    Idle,
    Pending,
    Running,
}

/// `/fast` service-tier override. Key PRESENCE, not value truthiness, decides
/// whether the override applies: [`ServiceTier::Unset`] means no override was
/// recorded, [`ServiceTier::Normal`] is an explicit normal choice, and
/// [`ServiceTier::Priority`] is `/fast`. Mirrors the Python `_UNSET_TIER`
/// sentinel vs `None` vs `"priority"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceTier {
    #[default]
    Unset,
    Normal,
    Priority,
}

/// State scoped to one running gateway turn.
///
/// `clear()` mirrors the exact reset set of the old `_release_running_agent_state`
/// (agent / started_ts / active-slot lease / busy-ack). The turn-lease token is
/// deliberately NOT cleared here: in Rust the token is an RAII guard held in the
/// turn's own scope (see [`crate::turn_lease`]) rather than stored in shared
/// state, so it is released exactly once when that scope ends. Only the
/// generation is tracked here for diagnostics.
#[derive(Debug, Default)]
pub struct TurnState {
    /// Running-agent slot; `Idle` = not running.
    pub agent: AgentSlot,
    /// Turn start timestamp (0.0 = not running).
    pub started_ts: f64,
    /// Whether a cross-process active-session slot lease is held. Stands in for
    /// the opaque Python lease handle until that subsystem is ported.
    // TODO(port): carry the real active-slot lease handle once it exists.
    pub holds_active_slot_lease: bool,
    /// Last busy-ack timestamp (debounce; 0.0 = never acked).
    pub busy_ack_ts: f64,
    /// Run generation that acquired the currently held turn lease, if any.
    pub lease_generation: Option<u64>,
}

impl TurnState {
    /// Reset the per-turn slot. Does not touch `lease_generation` (owned by the
    /// lease-release path), matching the Python clear set.
    pub fn clear(&mut self) {
        self.agent = AgentSlot::Idle;
        self.started_ts = 0.0;
        self.holds_active_slot_lease = false;
        self.busy_ack_ts = 0.0;
    }
}

/// State scoped to one conversation (survives turns, not boundaries).
#[derive(Debug, Default)]
pub struct ConversationState {
    /// `/model` per-session override (model/provider/api_key/base_url/api_mode).
    // TODO(port): give this a typed struct once model resolution is ported.
    pub model_override: Option<Value>,
    /// `/model --once` restore snapshot.
    pub one_turn_restore: Option<Value>,
    /// `/reasoning` per-session override.
    pub reasoning_override: Option<Value>,
    /// `/fast` per-session override.
    pub service_tier_override: ServiceTier,
    /// Last successfully-resolved non-empty model (#35314 recovery).
    pub last_resolved_model: String,
    /// `/queue` overflow FIFO (the adapter slot holds the head).
    pub queued_events: Vec<Value>,
    /// Per-turn must-deliver sidecar notes (one-shot).
    pub sidecar_notes: Vec<String>,
    /// Pinned session-context bytes: (change_key, text).
    pub ephemeral_pin: Option<(Value, String)>,
    /// Last voice-channel context delivered (None = never delivered).
    pub vc_last: Option<String>,
}

impl ConversationState {
    /// Reset every conversation-scoped field to its default. The structural
    /// successor of the Python `_CONVERSATION_SCOPED_STATE` pop-loop: adding a
    /// field here means every boundary clears it automatically.
    pub fn clear(&mut self) {
        *self = ConversationState::default();
    }
}

/// State with its own lifecycle — not cleared wholesale by turn or boundary
/// resets. (Approvals / update prompts ARE cleared by the boundary security
/// funnel, but individually, matching the old behavior.)
#[derive(Debug, Default)]
pub struct PersistentState {
    /// Pending exec approval (`{"command": ..., "pattern_key": ...}`).
    pub approvals: Option<Value>,
    /// `/update` prompt awaiting a user response.
    pub update_prompt_pending: bool,
    /// Image paths staged for native (inline) attachment; consumed one-shot.
    pub native_image_paths: Vec<String>,
    /// Legacy runner-level pending message text (flushed to disk on shutdown,
    /// #72680). Distinct from the adapter-level pending-message store.
    pub pending_command_text: Option<String>,
    /// Monotonic run-generation counter (#28686). NEVER reset: clearing it
    /// would break stale-run detection.
    pub run_generation: u64,
    /// Consecutive session-hygiene compression failures (#79624). Process-local
    /// by design; reset on a successful compression, not by turn/boundary
    /// resets. gateway/run.py mirrors it to the DB keyed by session_key so the
    /// escalation ladder survives restarts.
    pub hygiene_failure_streak: u32,
}

/// All per-session gateway state, grouped by lifecycle scope. Entries in the
/// runner's session map are never evicted (matching the old dicts); eviction of
/// fully-default states is possible follow-up work.
#[derive(Debug, Default)]
pub struct SessionState {
    pub turn: TurnState,
    pub conversation: ConversationState,
    pub persistent: PersistentState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_are_idle_and_unset() {
        let s = SessionState::default();
        assert_eq!(s.turn.agent, AgentSlot::Idle);
        assert_eq!(s.conversation.service_tier_override, ServiceTier::Unset);
        assert_eq!(s.persistent.run_generation, 0);
        assert!(s.conversation.queued_events.is_empty());
    }

    #[test]
    fn turn_clear_resets_only_turn_fields() {
        let mut s = SessionState::default();
        s.turn.agent = AgentSlot::Running;
        s.turn.started_ts = 123.0;
        s.turn.holds_active_slot_lease = true;
        s.turn.busy_ack_ts = 5.0;
        s.turn.lease_generation = Some(7);

        s.turn.clear();

        assert_eq!(s.turn.agent, AgentSlot::Idle);
        assert_eq!(s.turn.started_ts, 0.0);
        assert!(!s.turn.holds_active_slot_lease);
        assert_eq!(s.turn.busy_ack_ts, 0.0);
        // lease_generation is owned by the release path, not cleared here.
        assert_eq!(s.turn.lease_generation, Some(7));
    }

    #[test]
    fn conversation_clear_resets_all_but_persistent_untouched() {
        let mut s = SessionState::default();
        s.conversation.model_override = Some(json!({"model": "x"}));
        s.conversation.service_tier_override = ServiceTier::Priority;
        s.conversation.last_resolved_model = "m".into();
        s.conversation.queued_events.push(json!({"e": 1}));
        s.persistent.run_generation = 4;

        s.conversation.clear();

        assert!(s.conversation.model_override.is_none());
        assert_eq!(s.conversation.service_tier_override, ServiceTier::Unset);
        assert_eq!(s.conversation.last_resolved_model, "");
        assert!(s.conversation.queued_events.is_empty());
        // Persistent scope is independent of a conversation boundary.
        assert_eq!(s.persistent.run_generation, 4);
    }
}
