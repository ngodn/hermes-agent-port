# Session Stale-Route Recovery Integration Map

Audited sources: `hermes_state.py`, `hermes_state_common.py`, `gateway/session.py`
Target files: `rust/crates/hermes-gateway/src/session_db.rs`, `session_routing.rs`

## 1. Exact Missing Columns and Tables (`session_db.rs:363-404`)
The Rust `sessions` DDL lacks columns required to inspect lifecycle, enforce fences, and recover peers:
- `ended_at REAL`: Marks closed sessions; NULL for active sessions (`hermes_state.py:8502`).
- `end_reason TEXT`: Differentiates accidental closures from intentional resets (`hermes_state_common.py:194-227`).
- `user_id TEXT`: Peer participant identifier; required for peer-fallback matching (`hermes_state.py:7849`).
- `profile_name TEXT`: Identifies profile ownership for partition fencing (`hermes_state.py:7853`).
- `origin_json TEXT`: Serialized `SessionSource`; required for Slack workspace/guild fence (`gateway/session.py:2305`).
- `display_name TEXT`: Restored chat title/name on recovered routing entries (`gateway/session.py:2349`).
- `parent_session_id TEXT`: Tracks lineage for compression child repointing (`gateway/session.py:2784-2802`).
- Missing auxiliary table: `system_prompts (hash TEXT PRIMARY KEY, prompt TEXT NOT NULL)` joined by `find_latest_gateway_session_for_peer` (`hermes_state.py:7802`). Minimal DDL is sufficient.

## 2. Selection Gates and Ranking Logic (`hermes_state.py:7750-7875`)
Recovery must evaluate candidates using a strict two-tier gating hierarchy:
1. Exact `session_key` Gate (`hermes_state.py:7801-7820`):
   - Match: `s.session_key = ? AND s.source = ?`.
   - Liveness filter: `s.ended_at IS NULL OR s.end_reason IN ('agent_close', 'ws_orphan_reap', 'superseded_by_resume', 'startup_orphan_reap')`.
   - Reset boundary fence: Reject candidate if another row for the same `session_key` and `source` has `ended_at > COALESCE(s.last_activity_at, s.started_at)` with `end_reason` in the reset set.
   - Ranking: `ORDER BY _has_messages DESC, COALESCE(s.last_activity_at, s.started_at) DESC LIMIT 1`. Empty keyed rows are accepted if no message-bearing candidate exists (#82616).
2. Peer-Tuple Fallback Gate (`hermes_state.py:7824-7874`):
   - Only runs when exact key match yields nothing; requires non-null `chat_id` and `chat_type`.
   - Disabled for Slack sources with `scope_id` unless claiming legacy un-scoped keys (`gateway/session.py:2365-2368`).
   - Match: Same `source`, `user_id`, `chat_id`, `chat_type`, `thread_id` (using SQLite `COALESCE(col, '') = COALESCE(?, '')`).
   - Message requirement: Candidate MUST have messages (`_has_messages = 1`). Empty rows are rejected.
   - Profile fence: Row `profile_name` must match store owner or be NULL.
   - Reset boundary fence: Evaluated across peer tuple instead of session key.
   - Ranking: `ORDER BY COALESCE(s.last_activity_at, s.started_at) DESC LIMIT 1`.

## 3. Reset Boundaries and Partition Fences
- Reset Reason Sets (`hermes_state_common.py:194-227`):
  - Recoverable (accidental): `agent_close`, `ws_orphan_reap`, `superseded_by_resume`, `startup_orphan_reap`.
  - Non-recoverable (reset): `session_reset`, `session_switch`, `idle`, `daily`, `suspended`, `resume_pending_expired`.
- Reset Policy & Promotion Gate (`gateway/session.py:2450-2464`, `hermes_state.py:8539-8586`):
  - If candidate row exceeds idle or daily policy deadlines (`gateway/session.py:2737-2782`), call `promote_to_session_reset(id, reason)` and return None. Do not reopen expired sessions.
  - `promote_to_session_reset` only updates rows where `ended_at IS NULL` or `end_reason` is in the recoverable set. Existing explicit resets are preserved.
  - Valid candidates are reopened via `reopen_session(id)` (`UPDATE sessions SET ended_at = NULL, end_reason = NULL`).
- Workspace / Guild Fence (`gateway/session.py:2285-2310`):
  - For Slack non-DM sources, parse candidate `origin_json`. If `origin.scope_id != source.scope_id`, reject candidate. Missing or corrupt `origin_json` fails closed.
- Profile Fence (`gateway/session.py:2206-2234`):
  - Multiplexed mode: candidate key profile must match requested key profile.
  - Single-profile mode: candidate profile must match active runtime profile.

## 4. Prioritized Implementation Dependencies (Runtime Blockers)
- Priority 1 (Schema & DB Reader/Writer Primitives in `session_db.rs`):
  - Add missing columns to `ensure_schema` and add migration alter/heal statements for legacy DBs.
  - Implement `SessionDb::get_session(id) -> Option<SessionRow>` returning `end_reason`, timestamps, origin, profile.
  - Implement `SessionDb::find_latest_gateway_session_for_peer(...)` implementing exact-key and peer-fallback queries.
  - Implement `SessionDb::reopen_session(id)` and `SessionDb::promote_to_session_reset(id, reason) -> bool`.
- Priority 2 (Pruning and Recovery Orchestration in `session_routing.rs`):
  - Implement `RoutingIndex::prune_stale_sessions(&mut self, db: Option<&SessionDb>, policy: &ResetPolicy)`.
  - Stale detection: for each routing entry, check `db.get_session(entry.session_id)`. If ended, trigger recovery.
  - Error resilience: if recovery query fails/busy, keep current entry (do not prune).
  - Repoint vs Reopen: if recovered `session_id != entry.session_id`, repoint entry and mark dirty for `save()`. If same `session_id`, retain original entry in memory (preserves token counts and overrides) without saving. If unrecoverable, remove entry and persist.
- Priority 3 (Startup & Runtime Integration):
  - Invoke `prune_stale_sessions` inside `RoutingIndex::ensure_loaded` before returning cached entries.

## 5. Existing Python Test Oracles
- `tests/gateway/test_session_store_stale_prune.py`:
  - `test_prunes_multiple_stale_entries`: ended rows without recovery are deleted from routing index.
  - `test_keeps_stale_entry_when_recovery_lookup_raises`: DB query errors preserve entry for later retry.
  - `test_keeps_stale_entry_when_recovery_returns_same_session_id`: same-id recovery preserves live entry state.
  - `test_reset_boundary_does_not_recover_older_session_for_peer`: reset boundary fences recovery.
  - `test_overdue_recovered_session_promoted_to_reset_and_pruned`: expired session promotes to reset boundary.
- `tests/gateway/test_session_continuity_82616.py`:
  - `test_prefers_recent_activity_over_started_at`: recency ranking over start time.
  - `test_empty_keyed_row_returned_not_none`: empty keyed row returned if no messages.
  - `test_rows_with_messages_beat_empty_rows`: message-bearing rows prioritized.
- `tests/gateway/test_session_store_expiry_finalized.py`:
  - `test_live_session_recoverable_before_promotion` and `test_promote_to_session_reset`: promotion state machine.
- `tests/test_hermes_state.py:4612-4679`:
  - `test_gateway_session_recovery_does_not_cross_newer_reset_boundary`: cross-reset fence.
  - `test_peer_fallback_never_adopts_a_sibling_profiles_row`: cross-profile isolation fence.
