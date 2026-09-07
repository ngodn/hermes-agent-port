# Legacy Rust History Migration to Durable Routing

Maintainer notes (2026-09-07): the proposed read-then-peer-refresh adoption needs
an atomic ownership claim. SessionDb::claim_legacy_gateway_session now supplies
that guarded update. It preserves IDs, message rows and activity timestamps;
it requires the legacy writer's missing ownership fields, exact old source/chat
identity, compatible thread/type, and a live or accidentally closed row. Any
existing destination routing lineage blocks adoption, including closed rows.
Two inline SQLite tests verify transcript preservation, reset/identity guards,
and one winner between independent connections claiming the same row.

The claim is now called by SessionStore inside the per-key flight, after ordinary
recovery misses and before candidate publication. It retries ordinary recovery
so expiry and lineage use the existing transition. HTTP/push supply the exact
old message-derived ID rather than guessing Debug platform spellings. The claim
uses only the resolved owning profile database and force-new bypasses it.
Existing IDs rejected by the path-safety codec return an explicit error before
mutation; safe renaming/copying for those IDs remains unimplemented.
The source map below remains useful, but its suggested
unconditional peer-refresh adoption is superseded by the atomic claim.

## 1. Direct Source Facts

### 1.1 Legacy Rust Temporary IDs & Persistence
- **ID Formation**: [`session_db::message_session_id`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L146-L174) and [`session_db::session_id_for`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L140-L142) derived fallback strings when `resolved_session_id` was unset:
  - Channel / unscoped: `format!("{platform:?}:{channel_id}").to_lowercase()` (e.g. `cli:channel`, `telegram:12345`).
  - Scoped / workspace: `format!("{:?}:workspace:{}", platform, serde_json::to_string(&(team, channel)))`.
  - Thread: `format!("{:?}:thread:{}", platform, serde_json::to_string(&(workspace, channel, thread)))`.
- **Database Row State**: In [`session_db::begin_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L179-L208), [`SessionDb::ensure_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1266-L1290) inserted `sessions` rows with `id = sid`, `source = source`, `session_key = None` (`NULL`), `chat_id = Some(channel_id)`, and `chat_type = msg.chat_type`. Columns `thread_id`, `user_id`, `origin_json`, and `profile_name` remained `NULL`.
- **Message Transcript**: Turns were appended via [`SessionDb::append_message`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1293-L1300) under `messages.session_id = sid`. History retrieval in [`SessionDb::load_history`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1374-L1402) filters strictly by `WHERE session_id = ? AND active = 1`.

### 1.2 Durable Routing Recovery Gap
- **Durable Identity**: [`SessionStore::get_or_create_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L57-L103) constructs canonical keys via [`build_session_key`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session.rs#L318-L372) (`agent:<profile>:<platform>:<chat_type>:<chat_id>`). On cache miss, [`SessionStore::transition`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L105-L335) calls [`SessionRecovery::query_recoverable`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L171-L186).
- **Exact Query Miss**: [`SessionDb::find_latest_gateway_session_for_peer`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L941-L983) runs `EXACT_RECOVERY_SQL` (`WHERE s.session_key = ?`). It misses legacy rows because `sessions.session_key` is `NULL`.
- **Peer Query Misses**: `PEER_RECOVERY_SQL` misses legacy rows due to schema mismatches:
  - Thread messages: checks `COALESCE(s.thread_id, '') = COALESCE(?, '')`; fails because `s.thread_id` is `NULL`.
  - Scoped Slack: `legacy_key` disables fallback; furthermore, [`recovered_scope_matches`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L765-L784) requires `origin_json["scope_id"]`, which is `NULL`.
  - CLI: [`dispatch.rs:301-305`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L301-L305) sets `source.platform = "local"`, whereas earlier rows have `source = "cli"`.
  - User-scoped turns: `peer.user_id` is Some, but `s.user_id` is `NULL`.
- **History Loss**: When recovery returns `None`, [`SessionEntry::new_candidate`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_entry.rs#L99-L125) mints a fresh timestamped UUID ID (`sessions.id`). Prior transcript history under the legacy temporary ID is orphaned.

### 1.3 Reset Boundaries and Profile Isolation
- **Reset Fences**: [`crate::session_reset::reset_reason`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L129-L165) evaluates idle and daily policies against `updated_at`. Predecessors are marked ended via [`SessionDb::promote_to_session_reset`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L898-L920). SQL recovery queries enforce `NOT EXISTS` newer reset records.
- **Profile Partitioning**: Transcripts are isolated per database path via [`SessionDatabases::for_key`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L53-L60), [`store_profile_owner`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L230-L249), and [`recovered_profile_allowed`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L725-L750).

## 2. Design Recommendation: Smallest Migration in `get_or_create`

### 2.1 Integrate Legacy ID Probing in `select_recovery`
- In [`SessionRecovery::select_recovery`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L227-L278), if primary and legacy Slack lookups miss, derive candidate legacy Rust ID(s) directly from `request.source`:
  - If `source.thread_id` is present: `{:?}:thread:[scope, chat_id, thread_id]`.
  - Else if `source.scope()` is present: `{:?}:workspace:[scope, chat_id]`.
  - Else: `format!("{}:{}", source.platform, source.chat_id)` (testing `"cli"` if platform is `"local"`).
- Query the profile database via `db.find_legacy_rust_session(candidate_id, request.key)` using an SQL query that includes the standard reset boundary fence (`WHERE s.id = ? AND s.ended_at IS NULL AND NOT EXISTS newer reset`).

### 2.2 Strict Profile & Reset Boundary Enforcement
- **Profile Boundary**: Probing runs strictly against `database_for_key(request.key)` (resolved by [`SessionDatabases`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L34-L51)). Pass the recovered row through [`recovered_profile_allowed`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L725-L750) to prevent cross-profile adoption.
- **Reset Boundary**: The recovered entry passes through [`crate::session_reset::reset_reason`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L129-L165) in [`SessionStore::transition`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L249-L262):
  - If expired/reset: `context.prev_session_id = Some(legacy_id)`, [`SessionDb::promote_to_session_reset`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L898-L920) closes the legacy row, and a fresh candidate UUID is created. History boundary is preserved.
  - If active: proceed to adoption.

### 2.3 In-Place Adoption Without Message Rewriting
- **Zero Data Copying**: Construct [`SessionEntry::from_recovered_row`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_entry.rs#L130-L158), adopting `legacy_id` as `entry.session_id` while binding `entry.session_key = request.key`.
- **Row Repair**: Call [`SessionDb::reopen_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L921-L937) and [`SessionRecovery::record_recovered_peer`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L280-L299), which invokes [`SessionDb::record_gateway_session_peer`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L751-L859) to populate `session_key`, `thread_id`, `chat_type`, and `origin_json`.
- **Continuity**: [`SessionDb::load_history`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1374-L1402) reads existing messages under `legacy_id` immediately; subsequent turns find the session via exact key routing without re-entering legacy fallback.
