Codex follow-up: this Gemini report inspected an earlier worktree. Recovery and
pruning methods now exist in RoutingIndex, but startup and inbound wiring remain
pending. Treat suggested struct names as proposals, not requirements. In
particular, do not implement synchronous recovery by directly calling a query-only
method that records migrated peers: Python synchronous recovery records migration
only after reset evaluation and reopen, while query-only recovery records it before
returning the candidate. Sharing selection is appropriate; sharing that side-effect
order would change behavior. Verify profile path resolution against source before
implementing the suggested manager.

# Gateway Session Recovery Orchestration & Routing Architecture Review

This document provides a technical audit and comparative analysis of session recovery orchestration between the reference Python implementation ([`gateway/session.py`](../../gateway/session.py)) and the native Rust crates ([`session_routing.rs`](../crates/hermes-gateway/src/session_routing.rs), [`session_db.rs`](../crates/hermes-gateway/src/session_db.rs), and [`session_reset.rs`](../crates/hermes-gateway/src/session_reset.rs)).

---

## 1. Architectural Overview & Current Status

In Python, session lifecycle and routing are governed by a monolithic [`SessionStore`](../../gateway/session.py#L1262-L3640). Recovery orchestration ensures that crash-left sessions, compression-rotated lineage, and legacy platform keys are safely resolved to active database rows without resurrecting overdue conversations or leaking across workspace/profile boundaries.

In the Rust codebase, foundation layers have been ported into modular components:
- **Database Primitives** ([`session_db.rs`](../crates/hermes-gateway/src/session_db.rs)): Low-level SQL queries, recovery queries, peer recording, and lifecycle mutations.
- **Routing Index & Serialization** ([`session_routing.rs`](../crates/hermes-gateway/src/session_routing.rs)): In-memory routing map, generation tracking, candidate fast-path saves, full snapshot persistence, and scope/profile isolation predicates.
- **Reset Decisions** ([`session_reset.rs`](../crates/hermes-gateway/src/session_reset.rs)): Idle and daily reset evaluation with background process probing.
- **Data Models** ([`session_entry.rs`](../crates/hermes-gateway/src/session_entry.rs)): `SessionEntry` parsing and `from_recovered_row` reconstruction.
- **Handle Caching** ([`session_db_recovery.rs`](../crates/hermes-gateway/src/session_db_recovery.rs)): Resilient handle caching with exponential backoff.

> [!WARNING]
> **Current Integration Gap**: While individual primitives and predicate functions exist in Rust, the **orchestration logic** that coordinates them, stale-route pruning, single-flight locking, legacy Slack key migration, and two-tier recovery, **has not been integrated into a runtime `SessionStore`**.

---

## 2. The Two Python Recovery Paths

Python exposes two distinct recovery entry points that serve fundamentally different lifecycle phases. Conflating them would introduce either startup deadlocks or conversation lineage loss.

### 2.1 Side-by-Side Comparison

| Dimension | Path A: `_recover_session_from_db` ([`gateway/session.py:2394-2476`](../../gateway/session.py#L2394-L2476)) | Path B: `_query_recoverable_session` ([`gateway/session.py:2478-2533`](../../gateway/session.py#L2478-L2533)) |
| :--- | :--- | :--- |
| **Invocation Context** | **Startup Pruning**: Called exclusively from `_prune_stale_sessions_locked` ([line 1798](../../gateway/session.py#L1798)) inside `_ensure_loaded_locked`. | **Live Message Routing**: Called in Phase 3 of `_get_or_create_session_impl` ([line 3113](../../gateway/session.py#L3113)) on inbound turns. |
| **Lock Environment** | **Under `_lock`**: Invoked while holding `self._lock`. Synchronous blocking DB queries execute during startup. | **Lock-Free**: Invoked outside `self._lock`. Lock is released before I/O and reacquired only to publish candidates. |
| **Error Handling** | Sets `raise_on_lookup_error=True` ([line 1802](../../gateway/session.py#L1802)). Lookup exceptions bubble up to caller to prevent pruning on transient errors ([lines 1804-1815](../../gateway/session.py#L1804-L1815)). | Uses default `raise_on_lookup_error=False` ([line 2488](../../gateway/session.py#L2488)). Lookup exceptions are swallowed, returning `None`. |
| **Reset Policy Evaluation** | **Internal**: Directly invokes `self._should_reset(entry, source)` ([line 2450](../../gateway/session.py#L2450)). | **External**: Does not evaluate reset policy. Returns raw entry; caller evaluates `_should_reset` ([line 3117](../../gateway/session.py#L3117)). |
| **Database Lifecycle Side Effects** | **Eagerly Mutates DB**: Calls `promote_to_session_reset` / `end_session` ([lines 2453-2457](../../gateway/session.py#L2453-L2457)) if overdue; calls `reopen_session` ([line 2466](../../gateway/session.py#L2466)) if active. | **No DB Lifecycle Mutations**: Leaves session ended/open status untouched in database. Does not call `reopen` or `promote`. |
| **Return Value on Reset** | **Returns `None`**: Overdue sessions are promoted to reset boundaries and dropped ([line 2464](../../gateway/session.py#L2464)). Pruner removes key from routing index. | **Returns `SessionEntry`**: Even when overdue under policy, the entry is returned to provide predecessor lineage. |
| **Lineage Continuation** | **Terminates**: Drops expired routing keys at startup. No successor session is created until a message arrives. | **Links Lineage**: Caller marks predecessor `prev_session_id = recovered.session_id` ([line 3123](../../gateway/session.py#L3123)), sets `parent_session_id`, and stamps `_reset_from` ([lines 3191-3195](../../gateway/session.py#L3191-L3195)). |
| **Index Mutation / Publishing** | **Caller Repoints/Retains/Prunes**: If `recovered.session_id != entry.session_id` (compression child), repoints `_entries[key]` ([line 1833](../../gateway/session.py#L1833)); if same ID, keeps original entry object ([line 1847](../../gateway/session.py#L1847)); if `None`, deletes key ([line 1869](../../gateway/session.py#L1869)). | **Double-Checked Publish**: Caller reacquires `_lock` and inserts `_entries[session_key] = recovered` only if slot remains empty ([lines 3133-3138](../../gateway/session.py#L3133-L3138)). |

### 2.2 Deep Dive: Reset Decision Differences
- In `_recover_session_from_db`, an expired candidate row is a dead end. Because this runs during startup cleanup, the system must not create active sessions in memory for idle chats. The row is finalized via `promote_to_session_reset` ([line 2455](../../gateway/session.py#L2455)), and `None` is returned, causing `_prune_stale_sessions_locked` to purge the routing entry ([line 1869](../../gateway/session.py#L1869)).
- In `_query_recoverable_session`, the lookup occurs in response to an active inbound turn. If the candidate is expired under `_should_reset` ([line 3117](../../gateway/session.py#L3117)), returning the entry allows the routing coordinator to:
  1. Capture `prev_session_id = recovered.session_id` ([line 3123](../../gateway/session.py#L3123)).
  2. Mark `was_auto_reset = True` and capture `auto_reset_reason` ([lines 3119-3120](../../gateway/session.py#L3119-L3120)).
  3. Create a fresh session candidate linked to its predecessor via `parent_session_id` and `model_config: {"_reset_from": prev_session_id}` ([lines 3191-3195](../../gateway/session.py#L3191-L3195)).
  4. Demote the expired row via `promote_to_session_reset(db_end_session_id, auto_reset_reason)` ([lines 3217-3221](../../gateway/session.py#L3217-L3221)).

---

## 3. Detailed Inspection of Recovery Primitives

### 3.1 `_find_gateway_session_row`
**Reference: [`gateway/session.py:2355-2393`](../../gateway/session.py#L2355-L2393)**
- **Signature**:
  ```python
  def _find_gateway_session_row(
      self,
      *,
      session_key: str,
      source: SessionSource,
      allow_peer_fallback: bool,
      raise_on_lookup_error: bool = False,
  ) -> Optional[Dict[str, Any]]
  ```
- **Store Resolution**: Derives `db = self._db_for_key(session_key)` ([line 2370](../../gateway/session.py#L2370)).
- **Fallback Gating**: Pass `chat_id` and `chat_type` only when `allow_peer_fallback=True` ([lines 2380-2381](../../gateway/session.py#L2380-L2381)).
  > [!IMPORTANT]
  > **Scoped Slack Peer Fallback Disabling**: When recovering a workspace-scoped Slack session (`source.platform == Platform.SLACK and source.scope_id`), `allow_peer_fallback` MUST be `False` on the primary key lookup ([line 2413](../../gateway/session.py#L2413)). The legacy peer fallback tuple `(platform, user_id, chat_id, chat_type, thread_id)` contains no workspace/tenant ID; allowing fallback could match a channel ID in a different workspace.
- **Rust Status**:
  - Implemented in [`SessionDb::find_latest_gateway_session_for_peer`](../crates/hermes-gateway/src/session_db.rs#L623-L665).
  - Exact SQL query: [`EXACT_RECOVERY_SQL`](../crates/hermes-gateway/src/session_db.rs#L641).
  - Fallback logic checks `if exact.is_some() || peer.chat_id.is_none() || peer.chat_type.is_none() { return Ok(exact); }` ([lines 646-648](../crates/hermes-gateway/src/session_db.rs#L646-L648)).
  - **Missing**: A routing-level wrapper that converts `SessionSource` and `session_key` into `GatewayPeer`, manages `allow_peer_fallback` by omitting `chat_id`/`chat_type`, and routes to the per-key database handle.

### 3.2 Legacy Slack Claims & Workspace Fencing
**Reference: [`gateway/session.py:2244-2311`](../../gateway/session.py#L2244-L2311)**
- **Key Generation** ([`_legacy_slack_session_key`, lines 2244-2264](../../gateway/session.py#L2244-L2264)):
  - Applies strictly to `source.platform == Platform.SLACK and source.scope_id`.
  - Creates `legacy_source = replace(source, scope_id=None, guild_id=None)`.
  - Formats pre-migration key (e.g., `agent:main:slack:channel:C123` instead of `agent:main:slack:T01:channel:C123`).
- **Atomic Claim Tracking** ([`_claim_legacy_slack_key`, lines 2266-2283](../../gateway/session.py#L2266-L2283)):
  - Guarded by dedicated lock `self._legacy_slack_claim_lock = threading.Lock()` ([lines 1288, 2272](../../gateway/session.py#L1288)).
  - Backed by `self._claimed_legacy_slack_keys: set[str]` ([lines 1289, 2277](../../gateway/session.py#L1289)).
  - Atomic test-and-set ensures at most one workspace claims an ambiguous legacy unscoped key per process run.
- **Workspace Validation** ([`_recovered_row_matches_source_scope`, lines 2284-2311](../../gateway/session.py#L2284-L2311)):
  - Bypasses check for non-Slack, DM chats (`chat_type == "dm"`), or missing `scope_id` ([lines 2298-2303](../../gateway/session.py#L2298-L2303)).
  - For Slack channels/groups, parses `origin_json` from DB row.
  - Matches `origin.scope_id == source.scope_id` (with fallback to `origin.guild_id`). Fails closed on missing or unparseable origin ([lines 2304-2310](../../gateway/session.py#L2304-L2310)).
- **In-Memory Migration** ([`_get_or_create_session_impl`, lines 2933-2963](../../gateway/session.py#L2933-L2963)):
  - Evaluates prior to Phase 0.
  - If `legacy_key` exists in `_entries` and `session_key` is missing:
    - Adopts if recorded `origin_scope == source.scope_id`, OR if `origin_scope is None` and `chat_type == "dm"`.
    - Refuses adoption for scope-less channels/groups.
    - Moves entry in `_entries`, persists index, and updates DB via `_record_gateway_session_peer` ([lines 2956-2962](../../gateway/session.py#L2956-L2962)).
- **Rust Status**:
  - `recovered_scope_matches` is ported in [`session_routing.rs:354-373`](../crates/hermes-gateway/src/session_routing.rs#L354-L373).
  - `record_gateway_session_peer` is ported in [`session_db.rs:433-542`](../crates/hermes-gateway/src/session_db.rs#L433-L542).
  - **Missing**: `_legacy_slack_session_key`, `_claim_legacy_slack_key`, claim tracking set, and in-memory adoption.

### 3.3 Profile Isolation Fence
**Reference: [`gateway/session.py:2206-2234`](../../gateway/session.py#L2206-L2234)**
- Validates that a recovered DB row belongs to the active or requested profile namespace.
- Prevents cross-profile leakage in multiplexed mode (`multiplex_profiles=True`).
- **Rust Status**: Faithfully ported in [`session_routing.rs:314-339`](../crates/hermes-gateway/src/session_routing.rs#L314-L339) (`recovered_profile_allowed`) and verified by golden tests ([lines 408-435](../crates/hermes-gateway/src/session_routing.rs#L408-L435)).

### 3.4 Stale-Route Pruning
**Reference: [`gateway/session.py:1765-1870`](../../gateway/session.py#L1765-L1870)**
- **Startup Pruning (`_prune_stale_sessions_locked`)**:
  1. Inspects each `(key, entry)` in `self._entries`.
  2. Looks up row in DB via `db.get_session(entry.session_id)` ([line 1789](../../gateway/session.py#L1789)).
  3. If `row is None` (pre-SQLite legacy) or `row["end_reason"] is None` (live), entry is preserved ([lines 1790-1792](../../gateway/session.py#L1790-L1792)).
  4. If `row["end_reason"] is not None`: session ended in DB.
     - Calls `_recover_session_from_db(key, entry.origin, now, raise_on_lookup_error=True)` ([lines 1798-1803](../../gateway/session.py#L1798-L1803)).
     - If DB lookup errors, logs and skips pruning for that entry (`recovery_lookup_failed=True`) ([lines 1814-1815](../../gateway/session.py#L1814-L1815)).
     - If recovered entry has different `session_id`: updates route `self._entries[key] = recovered_entry` (handles compression child rotation where parent ended) ([lines 1824-1835](../../gateway/session.py#L1824-L1835)).
     - If recovered entry has same `session_id`: keeps original entry in `self._entries` to preserve in-memory token counters, model overrides, and pending queues ([lines 1847-1853](../../gateway/session.py#L1847-L1853)).
     - If `recovered_entry is None`: adds `key` to `stale_keys` for deletion ([lines 1855-1860](../../gateway/session.py#L1855-L1860)).
- **Live Routing Self-Heal (`_is_session_ended_in_db`)**:
  - Defined at [`gateway/session.py:2706-2736`](../../gateway/session.py#L2706-L2736).
  - Evaluated during `get_or_create_session` Phase 1b ([line 2999](../../gateway/session.py#L2999)) without lock.
  - If ended in DB, dropped from `_entries` in Phase 2 under lock ([line 3067](../../gateway/session.py#L3067)) and routed to recovery/create in Phase 3 ([line 3078](../../gateway/session.py#L3078)).
- **Rust Status**:
  - `SessionDb::get_session` is implemented in [`session_db.rs:670-682`](../crates/hermes-gateway/src/session_db.rs#L670-L682).
  - **Missing**: Neither `RoutingIndex::ensure_loaded` nor any other Rust routine performs startup pruning or live routing staleness checks.

---

## 4. Per-Key Database Resolution & Lock Ordering

### 4.1 Multiplexed Per-Key Database Resolution
In Python, multiple profiles may operate concurrently within a single gateway process. Sessions are stored in distinct SQLite databases:
- Default/ambient store: `$HERMES_HOME/state.db`.
- Profile stores: `$HERMES_HOME/profiles/<profile>/state.db`.

**Resolution Contract ([`_db_for_key`, gateway/session.py:1516-1563](../../gateway/session.py#L1516-L1563))**:
1. Extracts profile from session key (`agent:<profile>:...`).
2. If no profile, returns ambient `self._db`.
3. If profile named: resolves home directory `$HERMES_HOME/profiles/<profile>`.
4. **Fail-Closed Guarantee**: If profile home cannot be resolved, logs a warning and returns `None` ([lines 1551-1556](../../gateway/session.py#L1551-L1556)). It **never falls back to ambient DB**, preventing split-brain session records.
5. Caches open handles via `RecoverableHandleCache` with bounded exponential backoff.
6. `_db_for_session_id` ([lines 1585-1597](../../gateway/session.py#L1585-L1597)) looks up the owning key from `_entries` or `_session_owner_hints` and delegates to `_db_for_key`.

**Rust Status**:
- `RecoverableHandleCache` is implemented in [`session_db_recovery.rs:63-200`](../crates/hermes-gateway/src/session_db_recovery.rs#L63-L200).
- Profile extraction is implemented in [`session_routing.rs:341-350`](../crates/hermes-gateway/src/session_routing.rs#L341-L350) (`profile_from_key`).
- **Missing**: A routing database registry that maps keys to `SessionDb` handles via `RecoverableHandleCache`. `RoutingIndex` currently accepts only a single ambient `Option<&SessionDb>`.

### 4.2 Python Lock Hierarchy & Execution Phases

Python coordinates multiple fine-grained locks to avoid deadlocks and minimize critical-section contention:

```
┌────────────────────────────────────────────────────────┐
│                   _inflight_lock                       │  (Per-key flight coordination)
└──────────────────────────┬─────────────────────────────┘
                           │ (Held microseconds; released before I/O)
                           ▼
┌────────────────────────────────────────────────────────┐
│                       _lock                            │  (In-memory _entries & generations)
└──────────────┬──────────────────────────┬──────────────┘
               │                          │
               │ (Nested inside _lock     │ (Released BEFORE
               │  during Slack migration) │  persistence/save)
               ▼                          ▼
┌──────────────────────────────┐  ┌──────────────────────┐
│ _legacy_slack_claim_lock     │  │      _save_lock      │  (Durable file/DB writes)
└──────────────────────────────┘  └──────────┬───────────┘
                                             │
                                             ▼
                                  ┌──────────────────────┐
                                  │ SQLite Conn Mutexes  │  (SessionDb internal transactions)
                                  └──────────────────────┘
```

#### Phase Breakdown in `get_or_create_session`:
1. **Single-Flight Lock (`_inflight_lock`, lines 2863-2902)**:
   - Serializes concurrent requests for the **same** session key.
   - Distinct session keys run concurrently.
2. **In-Memory Slack Migration (`_lock -> _legacy_slack_claim_lock`, lines 2935-2954)**:
   - Acquires `_lock`, evaluates legacy entry adoption, claims legacy key under `_legacy_slack_claim_lock`, mutations made to `_entries`.
   - Releases `_lock`.
3. **Phase 0 & 0b (Compression Tip, lines 2969-2982)**:
   - Phase 0: Acquires `_lock` to read current `existing_session_id`.
   - Phase 0b: Releases `_lock`, executes DB lineage query outside lock.
4. **Phase 1 & 1b (Staleness & Policy Snapshot, lines 2984-3025)**:
   - Phase 1: Acquires `_lock` to snapshot `_entry_for_checks`.
   - Phase 1b: Releases `_lock`, performs DB staleness check (`_is_session_ended_in_db`) and evaluates reset policy without holding locks.
5. **Phase 2 (State Mutation, lines 3040-3079)**:
   - Acquires `_lock`.
   - Applies stale drops and auto-reset pops to `_entries`.
   - Sets flags: `_needs_recover`, `was_auto_reset`, `prev_session_id`.
   - Releases `_lock`.
6. **Phase 3 (Lock-Free I/O, Recovery, and Persistence, lines 3107-3232)**:
   - Completely lock-free window.
   - If `_needs_recover`: calls `_query_recoverable_session`.
   - If recovered and alive: calls `db.reopen_session()`.
   - Briefly acquires `_lock` ([lines 3133-3138](../../gateway/session.py#L3133-L3138)) to publish recovered entry if slot remains vacant.
   - If candidate created: creates candidate, briefly acquires `_lock` ([lines 3159-3168](../../gateway/session.py#L3159-L3168)) to publish.
   - Persistence: calls `_save_entries()` or `_save_entry()`. Inside `_save_entry`, captures revision under `_lock`, releases `_lock`, then acquires `_save_lock` ([lines 2123-2140](../../gateway/session.py#L2123-L2140)) for SQLite upsert.
   - Predecessor cleanup: calls `promote_to_session_reset` or `end_session` on predecessor row outside `_lock` ([lines 3206-3232](../../gateway/session.py#L3206-L3232)).

---

## 5. Concrete Integration Requirements for Rust

To complete the runtime port of session recovery and lifecycle management, the following components must be implemented and wired into `hermes-gateway`:

### Requirement 1: Database Handle Manager (`SessionDbManager`)
A thread-safe container bridging [`session_db_recovery::RecoverableHandleCache`](../crates/hermes-gateway/src/session_db_recovery.rs#L63) and profile routing:
- Maps `session_key` to `Option<Arc<SessionDb>>`.
- Uses `profile_from_key` ([`session_routing.rs:341`](../crates/hermes-gateway/src/session_routing.rs#L341)) to extract profile names.
- Resolves `$HERMES_HOME/profiles/<profile>/state.db` and fails closed (returns `None`) when the profile directory does not exist.
- Resolves root `$HERMES_HOME/state.db` for unprofiled/default keys.

### Requirement 2: Legacy Slack Claim Tracker
A thread-safe tracker matching Python's `_claim_legacy_slack_key`:
- State: `Arc<Mutex<HashSet<String>>>`.
- Method `legacy_slack_session_key(source: &SessionSource, profile: Option<&str>) -> Option<String>` generating the pre-workspace key format.
- Method `claim_legacy_slack_key(&self, legacy_key: &str) -> bool` performing atomic insert.

### Requirement 3: Unified Recovery Query Primitives
Two orchestration methods implemented on the session coordinator:
1. `query_recoverable_session`:
   - Wraps `SessionDb::find_latest_gateway_session_for_peer` with `allow_peer_fallback = legacy_key.is_none()`.
   - Attempts legacy Slack key fallback if exact key misses and claim succeeds.
   - Enforces `recovered_scope_matches` ([`session_routing.rs:354`](../crates/hermes-gateway/src/session_routing.rs#L354)).
   - Enforces `recovered_profile_allowed` ([`session_routing.rs:314`](../crates/hermes-gateway/src/session_routing.rs#L314)).
   - Reconstructs entry via `SessionEntry::from_recovered_row` ([`session_entry.rs:71`](../crates/hermes-gateway/src/session_entry.rs#L71)).
   - Updates peer metadata in DB via `SessionDb::record_gateway_session_peer` on legacy migration.
   - **Does not check reset policy and does not mutate session open/ended status**.
2. `recover_session_from_db`:
   - Invokes `query_recoverable_session` with error propagation enabled.
   - Evaluates `session_reset::reset_reason` ([`session_reset.rs:59`](../crates/hermes-gateway/src/session_reset.rs#L59)).
   - If reset: calls `SessionDb::promote_to_session_reset` ([`session_db.rs:580`](../crates/hermes-gateway/src/session_db.rs#L580)) and returns `None`.
   - If alive: calls `SessionDb::reopen_session` ([`session_db.rs:603`](../crates/hermes-gateway/src/session_db.rs#L603)) and returns `Some(entry)`.

### Requirement 4: Stale-Route Pruning in `RoutingIndex`
Integrate startup pruning into [`RoutingIndex::ensure_loaded`](../crates/hermes-gateway/src/session_routing.rs#L108-L148):
- For each entry in `RoutingIndex`:
  - Inspect `SessionDb::get_session(entry.session_id)`.
  - If `end_reason.is_some()`: run `recover_session_from_db`.
  - If recovery lookup errors: keep current entry (do not prune on DB error).
  - If recovered with different `session_id`: update route to child session and mark dirty for persistence.
  - If recovered with same `session_id`: retain original in-memory entry (preserves tokens/overrides).
  - If unrecoverable (`None`): delete from `RoutingIndex`.

### Requirement 5: Phased Single-Flight Session Dispatch
Implement a top-level `SessionStore` struct holding:
- `inner: Mutex<RoutingIndex>` (for in-memory state).
- `writer: Arc<RoutingWriter>` (for separated disk/DB writes).
- `db_manager: Arc<SessionDbManager>` (for per-profile handle resolution).
- `slack_claims: Arc<Mutex<HashSet<String>>>` (for legacy claims).
- Single-flight per-key coordinator (mapping `session_key` to channel/broadcast slots).
- Executes Phases 0 through 3 matching Python lock discipline: read under lock, perform I/O lock-free, write mutations under lock, and link lineage on reset.

---

## 6. Unresolved Runtime Integration Flags

The following concrete runtime gaps remain in the Rust codebase and must be resolved before the native gateway can replace the Python `SessionStore`:

| Unresolved Component | Current Rust Status | Required Implementation |
| :--- | :--- | :--- |
| **Startup Stale Pruning** | **Missing**: `RoutingIndex::ensure_loaded` reads tables but never validates `end_reason` or prunes stale keys. | Implement `prune_stale_sessions` loop calling `get_session` and `recover_session_from_db`. |
| **Live Routing Self-Heal** | **Missing**: No staleness detection during route resolution. Ended sessions remain in memory. | Add `is_session_ended_in_db` check in route lookup to drop stale entries. |
| **Per-Key Profile DB Routing** | **Partial**: `session_db_recovery.rs` provides handle caching, but `RoutingIndex` only takes a single ambient DB reference. | Build `SessionDbManager` to derive per-key DB handles with fail-closed profile semantics. |
| **Legacy Slack Key Migration** | **Missing**: `recovered_scope_matches` exists, but legacy key generation and claim locking are absent. | Port `_legacy_slack_session_key` and thread-safe atomic claim set. |
| **Single-Flight Dispatch** | **Missing**: No per-key flight coalescing. Concurrent requests for the same session key race. | Implement single-flight join/wait mechanism for session creation. |
| **Lineage & Predecessor Reset Stamping** | **Partial**: SQL schema supports `parent_session_id` and `_reset_from`, but no runtime code links them during reset. | Pass predecessor IDs during auto-reset creation and invoke `promote_to_session_reset`. |
