# Profile Database Resolution and Session DB Ownership Map

This document provides a comprehensive structural and behavioral analysis of per-key database resolution, profile and root path handling, missing-profile fail-closed semantics, multiplex-off mode, handle caching and retry logic, and process-wide database ownership for the Python-to-Rust migration of Hermes Gateway (`crates/hermes-gateway`).

---

## Table of Contents

1. [Architectural Invariants and Core Principles](#1-architectural-invariants-and-core-principles)
2. [Python Reference Architecture: `gateway/session.py`](#2-python-reference-architecture-gatewaysessionpy)
   - [2.1 Lifecycle State and Storage Properties](#21-lifecycle-state-and-storage-properties)
   - [2.2 Method-by-Method Analysis and Source Line References](#22-method-by-method-analysis-and-source-line-references)
   - [2.3 Complete Call-Site Inventory](#23-complete-call-site-inventory)
3. [Existing Rust Assets and Reusable Modules](#3-existing-rust-assets-and-reusable-modules)
   - [3.1 `session_db_recovery.rs` (`RecoverableHandleCache` and `SessionDatabases`)](#31-session_db_recoveryrs-recoverablehandlecache-and-sessiondatabases)
   - [3.2 `session_db.rs` (`SessionDb`)](#32-session_dbrs-sessiondb)
   - [3.3 `session_routing.rs` (`RoutingIndex`, `profile_from_key`, `recovered_profile_allowed`)](#33-session_routingrs-routingindex-profile_from_key-recovered_profile_allowed)
   - [3.4 `profile_name.rs` and `profile_routing.rs`](#34-profile_namers-and-profile_routingrs)
   - [3.5 `config_file.rs` and `config_gateway.rs`](#35-config_filers-and-config_gatewayrs)
   - [3.6 `main.rs` Database Ownership Audit](#36-mainrs-database-ownership-audit)
4. [Exact Profile and Root Path Behavior](#4-exact-profile-and-root-path-behavior)
   - [4.1 Path Resolution Rules and Precedence](#41-path-resolution-rules-and-precedence)
   - [4.2 On-Disk Profile Layout and Tombstones](#42-on-disk-profile-layout-and-tombstones)
   - [4.3 Routing Store vs Scoped Profile Store Separation](#43-routing-store-vs-scoped-profile-store-separation)
5. [Missing-Profile Behavior: Fail-Closed Contract](#5-missing-profile-behavior-fail-closed-contract)
   - [5.1 The Fail-Closed Contract](#51-the-fail-closed-contract)
   - [5.2 Cache Miss Non-Memoization and Dynamic Enrollment](#52-cache-miss-non-memoization-and-dynamic-enrollment)
6. [Multiplex-Off Behavior](#6-multiplex-off-behavior)
   - [6.1 Single-Profile Execution Semantics](#61-single-profile-execution-semantics)
   - [6.2 Comparison: Multiplex On vs Multiplex Off](#62-comparison-multiplex-on-vs-multiplex-off)
7. [Cache Retry Logic and Exponential Backoff](#7-cache-retry-logic-and-exponential-backoff)
   - [7.1 Backoff Formula and Intervals](#71-backoff-formula-and-intervals)
   - [7.2 Single-Flight Concurrency Control](#72-single-flight-concurrency-control)
   - [7.3 Generation Invalidation and Stale Handle Teardown](#73-generation-invalidation-and-stale-handle-teardown)
   - [7.4 Non-Cacheable Errors and Global Health Aggregation](#74-non-cacheable-errors-and-global-health-aggregation)
8. [Concurrency Constraints and Lock Hierarchy](#8-concurrency-constraints-and-lock-hierarchy)
   - [8.1 Memory Mutexes vs Disk IO Separation](#81-memory-mutexes-vs-disk-io-separation)
   - [8.2 Multi-Database Parallelism in WAL Mode](#82-multi-database-parallelism-in-wal-mode)
   - [8.3 Lock Ordering Hierarchy](#83-lock-ordering-hierarchy)
   - [8.4 Safe Shutdown and Handle Drainage Outside Locks](#84-safe-shutdown-and-handle-drainage-outside-locks)
9. [Concrete Implementation Outline for Rust Gateway](#9-concrete-implementation-outline-for-rust-gateway)
   - [9.1 Phase 1: Complete `SessionDatabases` Capabilities](#91-phase-1-complete-sessiondatabases-capabilities)
   - [9.2 Phase 2: Connect `RoutingIndex` to `SessionDatabases`](#92-phase-2-connect-routingindex-to-sessiondatabases)
   - [9.3 Phase 3: Migrate `main.rs` and `AppState` Database Ownership](#93-phase-3-migrate-mainrs-and-appstate-database-ownership)
   - [9.4 Phase 4: Wire `Dispatcher` Turn Execution](#94-phase-4-wire-dispatcher-turn-execution)
   - [9.5 Phase 5: Hook Clean Teardown on Process Termination](#95-phase-5-hook-clean-teardown-on-process-termination)

---

## 1. Architectural Invariants and Core Principles

In Hermes Gateway, a single operating process can serve messages across multiple platforms (Discord, Slack, Telegram, WhatsApp) and multiple isolated agent personas ("profiles"). Each named profile possesses its own persona, tools, configuration, and separate SQLite session history (`state.db`).

The database subsystem enforces three critical invariants:

1. **Deterministic Process-Wide Routing Index**: The routing index table (`gateway_routing`) tracks which active session ID corresponds to which logical routing key across all profiles. Because the routing map is a unified dictionary across profiles, it lives in exactly one physical file: the process routing home (`<routing_home>/state.db`), captured once at gateway startup.
2. **Profile-Isolated Conversation Transcripts**: Actual conversation messages and session metadata belong to the profile owning that turn. Under multiplexing, keys formatted as `agent:<profile>:...` store their history in `<root>/profiles/<profile>/state.db`. Default or legacy keys (`agent:main:...` or keys without a namespace) store their history in the root `<root>/state.db`.
3. **Fail-Closed on Unresolvable Profiles**: If a session key explicitly names a profile (for example, `agent:finance:...`), but that profile directory does not exist on disk or has been tombstoned, the database resolution MUST return `None`. It MUST NOT fall back to the root database. Falling back would write rows for that profile into the root store; once the profile directory is later provisioned at runtime, subsequent turns would write to the profile store, splitting the conversation history across two physical files.

---

## 2. Python Reference Architecture: `gateway/session.py`

### 2.1 Lifecycle State and Storage Properties

The reference implementation in [`gateway/session.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py) initializes handle caching and routing storage inside `SessionStore.__init__`:

- `self._db_pinned` ([`gateway/session.py:1327`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1327)): Initialized to `_DB_UNPINNED = object()`. Allows tests to install a mock handle or force SQLite off (`store._db = None`).
- `self._db_handles: Dict[Path, Any]` ([`gateway/session.py:1328`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1328)): Mapping of canonical file path to cached `SessionDB` instance.
- `self._db_handles_lock = threading.Lock()` ([`gateway/session.py:1329`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1329)): Synchronizes handle table lookups and inserts.
- `self._profile_home_cache: Dict[str, Optional[Path]]` ([`gateway/session.py:1333`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1333)): Cache of profile name to resolved `HERMES_HOME` directory. Only successful lookups are stored; misses are never cached.
- `self._session_owner_hints: Dict[str, str]` ([`gateway/session.py:1340`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1340)): Maps `child_session_id -> owning_session_key` during session compression before the new routing entry is published to `self._entries`.
- `self._db_handle_cache = RecoverableHandleCache(...)` ([`gateway/session.py:1343-1346`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1343-L1346)): Manages exponential backoff and single-flight reconnection attempts per database path.
- `self._routing_home: Optional[Path]` ([`gateway/session.py:1355`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1355)): Captured once at startup from `get_hermes_home()`. Pins the location of the process-wide routing index.

### 2.2 Method-by-Method Analysis and Source Line References

#### `_open_session_db_for_active_scope(db_path=None)` ([`gateway/session.py:1360-1411`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1360-L1411))
- Resolves target path: explicit `db_path` if provided, otherwise `_default_db_path()` (which respects context-local `HERMES_HOME` set by `_profile_runtime_scope`).
- Defines `_open()` closure calling `get_shared_session_db(path)`.
- Live-system guard check: if `RuntimeError` contains `"live-system guard"` (test isolation failure), it is classified as non-cacheable and immediately re-raised.
- Other exceptions log a warning and delegate to `RecoverableHandleCache`.
- Invokes `self._db_handle_cache.get(path, _open, non_cacheable=...)`.

#### `_db` Property Getter and Setter ([`gateway/session.py:1413-1431`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1413-L1431))
- Getter: If `self._db_pinned` is set, returns the pinned handle. Otherwise returns `self._open_session_db_for_active_scope()`.
- Setter: Sets `self._db_pinned = value`.

#### `_routing_db` Property ([`gateway/session.py:1433-1464`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1433-L1464))
- Returns pinned handle if set.
- If `self._routing_home` is None, falls back to `self._db`.
- Otherwise calls `self._open_session_db_for_active_scope(db_path=self._routing_home / "state.db")`.
- Guarantees the routing table is always read and written from the gateway launch home, preventing secondary profile turns from scattering routing rows across profile databases.

#### `_named_profile_for_key(session_key)` ([`gateway/session.py:1465-1480`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1465-L1480))
- Checks `self.config.multiplex_profiles`. If disabled, returns `None`.
- Extracts namespace using `self._profile_from_session_key(session_key)`.
- If profile is missing, empty, or `"default"`, returns `None`.
- Returns profile name string for valid secondary profiles.

#### `_profile_home_for_key(session_key)` ([`gateway/session.py:1481-1515`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1481-L1515))
- Calls `_named_profile_for_key(session_key)`. Returns `None` if `None`.
- Checks memory cache `self._profile_home_cache`. Returns cached `Path` on hit.
- Invokes `profile_exists(profile)` and `get_profile_dir(profile)` from `hermes_cli.profiles`.
- If valid directory exists and is not tombstoned: memoizes in `self._profile_home_cache` and returns `Path`.
- If lookup fails or profile does not exist: returns `None`. Does not cache the miss, ensuring dynamically provisioned profiles are detected on subsequent turns.

#### `_db_for_key(session_key)` ([`gateway/session.py:1516-1563`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1516-L1563))
- Returns `_db_pinned` if pinned.
- Calls `_named_profile_for_key(session_key)`.
  - If `None`: session belongs to ambient/root store. Returns `self._db`.
- Calls `_profile_home_for_key(session_key)`.
  - If `None`: Fails closed. Logs warning:
    `"gateway.session: profile %r has no resolvable home (key %r); refusing to fall back to the ambient store"`.
    Returns `None`.
  - If valid path `home`: calls `self._open_session_db_for_active_scope(db_path=home / "state.db")`.
  - On open exception: returns `None` (graceful degradation to JSONL / memory).

#### `_owner_key_for_session_id(session_id)` ([`gateway/session.py:1564-1584`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1564-L1584))
- Lock-free lookup over `self._entries.values()`.
- If matching entry found, returns `entry.session_key`.
- Otherwise checks `self._session_owner_hints.get(session_id)` for in-flight compression child sessions.

#### `_db_for_session_id(session_id)` ([`gateway/session.py:1585-1596`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1585-L1596))
- If `session_id` is empty or None, returns `self._db`.
- Resolves key via `_owner_key_for_session_id(session_id)`.
- Returns `self._db_for_key(owner_key)`. If unknown session ID, falls back to `self._db`.

#### `close_all_db_handles()` ([`gateway/session.py:1598-1629`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1598-L1629))
- Closes every cached `SessionDB` handle across all profiles using `self._db_handle_cache.close_all(_close)`.
- Handles are removed under cache lock, then closed outside the lock.
- Avoids blocking concurrent threads while waiting for SQLite connection flushes.

#### `_profile_from_session_key(session_key)` ([`gateway/session.py:2188-2197`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2188-L2197))
- Splits key on `":"`.
- Requires prefix `"agent"`.
- If second token is `""` or `"main"`, returns `"default"`. Otherwise returns the profile name.

#### `_recovered_row_allowed_for_active_profile(...)` ([`gateway/session.py:2206-2234`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2206-L2234))
- Verifies recovered database row belongs to the active profile context.
- When `multiplex_profiles` is false: `recovered_profile == active_profile`.
- When `multiplex_profiles` is true: validates that recovered row namespace matches requested turn namespace.

### 2.3 Complete Call-Site Inventory

`_db_for_key` is invoked across all state-mutating operations in `gateway/session.py`:

| Operation | Line Reference | Purpose |
| :--- | :--- | :--- |
| Stale route scan and recovery | [`gateway/session.py:1786`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1786) | Inspects owning store to check if `end_reason` is set before pruning. |
| Peer lookup | [`gateway/session.py:2370, 2372`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2370) | Queries `find_latest_gateway_session_for_peer` in profile store. |
| Reset promotion on recovery | [`gateway/session.py:2453, 2457`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2453) | Promotes row to `session_reset` in the owning store. |
| Reopen recovered session | [`gateway/session.py:2466`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2466) | Clears `end_reason` in profile store. |
| Peer recording | [`gateway/session.py:2543, 2545`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2543) | Stores peer record into profile database. |
| Expiry watcher finalization | [`gateway/session.py:2606`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2606) | Marks `set_expiry_finalized` in profile database from background task. |
| Recovery reopen | [`gateway/session.py:3126`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L3126) | Reopens session during inbound message resolution. |
| Reset predecessor row | [`gateway/session.py:3206, 3217, 3221`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L3206) | Promotes previous session row to reset in profile store. |
| Create new session row | [`gateway/session.py:3234, 3236`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L3234) | Inserts session row in profile database. |
| Explicit session reset | [`gateway/session.py:3706, 3712, 3716, 3726, 3728`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L3706) | Ends old session and creates new session row in profile store. |
| Switch / resume session | [`gateway/session.py:3825, 3831, 3835, 3839, 3841`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L3825) | Promotes predecessor to `session_switch` and reopens target session. |

`_db_for_session_id` is invoked when operations are addressed by session ID rather than routing key:

| Operation | Line Reference | Purpose |
| :--- | :--- | :--- |
| Message append | [`gateway/session.py:2728`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2728) | Writes turn message into profile transcript. |
| Active session ID resolution | [`gateway/session.py:2792, 2795`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L2792) | Resolves compression lineage tip from profile store. |
| Save transcript snapshot | [`gateway/session.py:3915`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L3915) | Syncs transcript to profile store. |
| Drain transcript queue | [`gateway/session.py:4055`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L4055) | Flushes delayed transcript queue into profile store. |
| Persist transcript immediate | [`gateway/session.py:4207`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L4207) | Forces immediate write of dirty transcript entries. |
| Platform message ID check | [`gateway/session.py:4330, 4333`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L4330) | Checks `has_platform_message_id` to prevent duplicate turns. |
| Message replacement | [`gateway/session.py:4371, 4375`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L4371) | Replaces messages in profile store. |
| Get conversation history | [`gateway/session.py:4401, 4413, 4423`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L4401) | Loads full history and compression tip for turn context. |
| Rewind session to message | [`gateway/session.py:4460, 4472, 4473, 4502`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L4460) | Rewinds transcript in profile database to specified message ID. |

---

## 3. Existing Rust Assets and Reusable Modules

The Rust workspace already contains working implementations of most individual building blocks.

### 3.1 `session_db_recovery.rs` (`RecoverableHandleCache` and `SessionDatabases`)

File: [`crates/hermes-gateway/src/session_db_recovery.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs)

1. `RecoverableHandleCache<H: Clone>` ([`session_db_recovery.rs:63-263`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L63-L263)):
   - Direct port of `gateway/session_db_recovery.py`.
   - Single-flight retry gating via `Unavailable.in_flight`.
   - Exponential backoff tracking with 1.0s initial delay and 60.0s maximum delay.
   - Generation invalidation on `close_all()`.
   - Non-cacheable error predicate support.
   - Global health aggregation (`global_session_store_health`, `set_health_sink`).

2. `SessionDatabases` ([`session_db_recovery.rs:34-111`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L34-L111)):
   - Manages `root: PathBuf`, `routing_home: PathBuf`, `multiplex: bool`.
   - Caches profile directory paths in `homes: Mutex<HashMap<String, PathBuf>>`.
   - Resolves databases through internal `handles: RecoverableHandleCache<Arc<SessionDb>>`.
   - Exposes `for_key(&self, key: &str, ambient_home: &Path) -> Option<Arc<SessionDb>>`.
   - Exposes `routing(&self) -> Option<Arc<SessionDb>>`.
   - Accurately checks profile directory existence and tombstone marker (`profiles/.deleted/<name>`).
   - Memoizes only successful profile home hits, leaving misses unmemoized.

### 3.2 `session_db.rs` (`SessionDb`)

File: [`crates/hermes-gateway/src/session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs)

- Thread-safe handle: `conn: Mutex<Connection>` with WAL journal mode (`lines 854-860`).
- Construction: `SessionDb::open(path: PathBuf)` (`lines 850-861`) and `SessionDb::open_default()` (`lines 846-848`).
- Ownership derivation: `store_profile_owner(path: &Path, root: &Path) -> Option<String>` (`lines 223-242`).
- Implements tables `gateway_routing`, `sessions`, `messages`.
- Implements conversation history queries, peer records, reset promotions, compression lineage, and transcript updates.

### 3.3 `session_routing.rs` (`RoutingIndex`, `profile_from_key`, `recovered_profile_allowed`)

File: [`crates/hermes-gateway/src/session_routing.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs)

- `profile_from_key(key: &str) -> Option<&str>` ([`session_routing.rs:571-580`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L571-L580)): Extracts profile namespace from `agent:<profile>:...`. Maps `""` and `"main"` to `"default"`.
- `recovered_profile_allowed(...)` ([`session_routing.rs:544-569`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L544-L569)): Ensures recovered database rows are isolated to the authorized profile.
- `RoutingIndex` ([`session_routing.rs:28-100`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L28-L100)):
  - Holds published routes in `entries: BTreeMap<String, SessionEntry>`.
  - Methods `recover_from_db`, `query_recoverable`, and `prune_stale` already accept a per-key database closure:
    `database_for_key: impl Fn(&str) -> Option<&'db SessionDb>`.

### 3.4 `profile_name.rs` and `profile_routing.rs`

- [`crates/hermes-gateway/src/profile_name.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/profile_name.rs):
  - `normalize_profile_name` (`lines 46-55`): Trims whitespace, casefolds `"default"`, lowercases name.
  - `validate_profile_name` (`lines 75-86`): Regex guard `^[a-z0-9][a-z0-9_-]{0,63}$` and check against reserved names (`"hermes"`, `"default"`, `"test"`, `"tmp"`, `"root"`, `"sudo"`).
- [`crates/hermes-gateway/src/profile_routing.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/profile_routing.rs):
  - Inbound route matching prioritizing thread (14) > chat (6) > guild (2).
  - WhatsApp phone/JID/LID identity normalization.

### 3.5 `config_file.rs` and `config_gateway.rs`

- [`crates/hermes-gateway/src/config_file.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_file.rs):
  - `hermes_home()` (`lines 68-91`): Current ambient home via `HERMES_HOME` env var, falling back to platform default.
  - `native_hermes_home()` (`lines 97-114`): Platform default (`~/.hermes` or `%LOCALAPPDATA%\hermes`), ignoring `HERMES_HOME`.
  - `hermes_root()` (`lines 120-146`): Canonical root directory for profile subdirectories.
- [`crates/hermes-gateway/src/config_gateway.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs):
  - `GatewayConfig.multiplex_profiles: bool` (`lines 235, 524, 615`): Parsed from top-level/nested config and environment overrides.

### 3.6 `main.rs` Database Ownership Audit

File: [`crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)

The database ownership in `main.rs` currently represents an early single-profile seam:

```rust
// main.rs:443-449
let session_db = match session_db::SessionDb::open_default() {
    Ok(db) => Some(Arc::new(db)),
    Err(err) => {
        tracing::warn!(%err, "session store unavailable; turns will be stateless");
        None
    }
};

let state = AppState::new(agent, user_config, configured_model, session_db);
```

Key gaps in `main.rs`:
1. **Single Static Connection**: It opens only `config_file::hermes_home().join("state.db")`.
2. **Missing `SessionDatabases` Integration**: The existing `SessionDatabases` struct from `session_db_recovery.rs` is never instantiated.
3. **No Multi-Profile Routing**: Push paths (`start_push_path`) pass `state.session_db.clone()` directly to `Dispatcher::new`. The dispatcher runs all messages against that single database handle, breaking profile isolation under multiplexing.
4. **No Clean Handle Drain at Shutdown**: While `main.rs` listens for shutdown signals via `CancellationToken` (`main.rs:471-478`), it never calls `close_all()` on the database cache, risking lingering WAL write locks across process restarts.

---

## 4. Exact Profile and Root Path Behavior

### 4.1 Path Resolution Rules and Precedence

Path resolution in Rust must strictly adhere to the precedence defined in `hermes_constants.py` and `config_file.rs`:

1. **Native Default Home (`native_hermes_home`)**:
   - Windows: `%LOCALAPPDATA%\hermes`
   - POSIX: `~/.hermes`
2. **Ambient Process Home (`hermes_home`)**:
   - Reads `HERMES_HOME` environment variable.
   - If unset or empty, falls back to `native_hermes_home`.
3. **Profiles Root Directory (`hermes_root`)**:
   - If `HERMES_HOME` is unset: returns `native_hermes_home`.
   - If `HERMES_HOME` is inside `native_hermes_home`: returns `native_hermes_home`.
   - In Docker/custom setups where `HERMES_HOME` is outside `~/.hermes` (e.g. `/opt/data`):
     - If `HERMES_HOME` has parent directory named `profiles` (`<root>/profiles/<name>`): returns the grandparent `<root>`.
     - Otherwise returns `HERMES_HOME` directly.

### 4.2 On-Disk Profile Layout and Tombstones

For any given canonical profile name `canon = normalize_profile_name(profile)`:

- **Default Profile (`canon == "default"`)**:
  - Home directory: `<hermes_root>`
  - Database file: `<hermes_root>/state.db`
- **Named Profile (`canon != "default"`)**:
  - Home directory: `<hermes_root>/profiles/<canon>`
  - Database file: `<hermes_root>/profiles/<canon>/state.db`
  - Tombstone marker file: `<hermes_root>/profiles/.deleted/<canon>`

A named profile exists if and only if:
```rust
let home = root.join("profiles").join(&canonical);
let marker = home.parent()?.join(".deleted").join(home.file_name()?);
home.is_dir() && !marker.exists()
```
This matches `session_db_recovery.rs:98-102` and Python `hermes_cli/profiles.py:393-400`.

### 4.3 Routing Store vs Scoped Profile Store Separation

In Python, `SessionStore` captures `_routing_home` at initialization:
```python
self._routing_home: Optional[Path] = Path(get_hermes_home())
```
In Rust, `SessionDatabases` mirrors this:
```rust
pub struct SessionDatabases {
    root: PathBuf,
    routing_home: PathBuf,
    multiplex: bool,
    ...
}
```
- `stores.routing()` opens `<routing_home>/state.db`. This handle owns table `gateway_routing`.
- `stores.for_key(key, ambient_home)` resolves `<root>/profiles/<profile>/state.db` (or `<ambient_home>/state.db` for default/unscoped keys). These handles own tables `sessions` and `messages`.

This guarantees that a whole-index routing table update never touches profile-specific databases.

---

## 5. Missing-Profile Behavior: Fail-Closed Contract

### 5.1 The Fail-Closed Contract

When `multiplex_profiles` is enabled and a session key contains a named profile namespace (for example, `agent:analytics:discord:dm:1001`):

1. `profile_from_key(key)` yields `Some("analytics")`.
2. The resolver checks if profile `"analytics"` exists and is active.
3. If directory `<root>/profiles/analytics` does not exist or `<root>/profiles/.deleted/analytics` exists:
   - The resolver returns `None`.
   - It **refuses** to fall back to the ambient/root store.

```
+------------------------------------+
| Inbound Session Key:               |
| agent:finance:telegram:dm:456      |
+-----------------+------------------+
                  |
                  v
       Named profile = "finance"
                  |
      +-----------+-----------+
      |                       |
      v                       v
Profile Exists?         Profile Missing / Tombstoned?
      |                       |
      v                       v
Open & return           FAIL CLOSED: return None
profiles/finance/       (NEVER fall back to root store)
state.db
```

Why fail-closed is mandatory:
- If the gateway fell back to the root database, it would insert a row for `agent:finance:...` into root `state.db`.
- When the profile is provisioned later, subsequent turns for that conversation would write to `profiles/finance/state.db`.
- The conversation history would become split across two distinct files, corrupting session resumption and breaking lineage tracking.

### 5.2 Cache Miss Non-Memoization and Dynamic Enrollment

`SessionDatabases.homes` caches profile directory paths:
```rust
if let Some(home) = self.homes.lock().unwrap().get(profile) {
    return Some(home.clone());
}
```
If the profile directory check fails, the result is `None`, and **no entry is inserted into `self.homes`**.

This non-memoization is critical:
- Profile enrollment bridges or CLI commands (`hermes profile create`) can create `<root>/profiles/<name>/` while the gateway is running.
- If a miss were memoized, the gateway would remain permanently blind to newly created profiles until restarted.
- Checking `home.is_dir()` and `!marker.exists()` on a miss costs only one stat call per message for non-existent profiles, while hits are fast memory lookups.

---

## 6. Multiplex-Off Behavior

### 6.1 Single-Profile Execution Semantics

When `multiplex_profiles` is false (the default configuration):

1. `SessionDatabases.home_for_key` suppresses profile derivation:
   ```rust
   let profile = self.multiplex
       .then(|| crate::session_routing::profile_from_key(key))
       .flatten()
       .filter(|p| *p != "default");
   let Some(profile) = profile else {
       return Some(ambient_home.to_path_buf());
   };
   ```
2. Any session key (even if it contains `agent:<name>:...`) resolves directly to `ambient_home` (`<hermes_home>/state.db`).
3. Only one `SessionDb` instance is opened for session transcripts.
4. In `session_routing.rs:567`, recovery profile validation enforces:
   ```rust
   recovered_profile == active_profile
   ```
   A single-profile gateway refuses to adopt or revive rows belonging to other profiles.

### 6.2 Comparison: Multiplex On vs Multiplex Off

| Feature | Multiplex On (`multiplex_profiles = true`) | Multiplex Off (`multiplex_profiles = false`) |
| :--- | :--- | :--- |
| Database per Key | Dispatches to `<root>/profiles/<name>/state.db` | All keys route to `<hermes_home>/state.db` |
| Missing Profile | Fails closed (`None`); skips database persistence | Not applicable; uses ambient store |
| Routing Index Location | Fixed at `<routing_home>/state.db` | Fixed at `<routing_home>/state.db` (same as ambient) |
| Active Profile Scope | Meaningless for storage; key namespace governs | Authoritative; only active profile rows adopted |
| Database Connections | N cached handles (one per active profile) | 1 handle for the entire process |

---

## 7. Cache Retry Logic and Exponential Backoff

### 7.1 Backoff Formula and Intervals

Database file open attempts can fail transiently (locked files, permission races, disk full, NFS stalls). Failed opens must heal automatically without blocking message delivery or permanently wedging the profile.

`RecoverableHandleCache` enforces bounded exponential backoff:
- `initial_retry_delay = 1.0` second.
- `max_retry_delay = 60.0` seconds.
- Backoff calculation:
  ```rust
  let exp = (failures.saturating_sub(1)).min(30);
  let delay = (self.initial_retry_delay * 2f64.powi(exp as i32)).min(self.max_retry_delay);
  ```

| Failures | Delay |
| :--- | :--- |
| 1 | 1.0s |
| 2 | 2.0s |
| 3 | 4.0s |
| 4 | 8.0s |
| 5 | 16.0s |
| 6 | 32.0s |
| 7+ | 60.0s (capped) |

### 7.2 Single-Flight Concurrency Control

When multiple threads concurrently process messages for a profile whose database is unavailable:

1. Inside `self.inner.lock()`:
   - If `path` is in `handles`: returns cached handle.
   - If `entry.in_flight || now < entry.next_retry_at`: immediately returns `Ok(None)` without attempting an open.
   - If retry interval has elapsed: sets `entry.in_flight = true`, releases lock.
2. Only ONE thread attempts `opener()` outside the lock.
3. Concurrent callers do not block or spawn redundant open attempts; they degrade to JSONL or ephemeral fallback immediately.

### 7.3 Generation Invalidation and Stale Handle Teardown

`RecoverableHandleCache` tracks a monotonic `generation: u64`:
- When `close_all()` runs, `generation` is incremented.
- When an in-flight `opener()` finishes outside the lock, it reacquires the lock and compares its captured generation with `inner.generation`.
- If `generation != inner.generation`, a `close_all()` occurred while opening was in progress.
- The newly opened handle is recognized as stale, dropped immediately (closing the SQLite connection), and not inserted into `handles`.

### 7.4 Non-Cacheable Errors and Global Health Aggregation

- **Non-Cacheable Errors**: If `non_cacheable(&exc)` returns true, the path is immediately removed from `unavailable` and the error is propagated. It is not put into exponential backoff.
- **Global Health Aggregation**:
  - Status states per path: `"ok"`, `"retrying"`, `"unavailable"`.
  - Aggregated across all active caches in `GlobalHealth`:
    - Any cache `"retrying"` -> aggregate is `"retrying"`.
    - Else any cache `"unavailable"` -> aggregate is `"unavailable"`.
    - Else aggregate is `"ok"`.
  - Published to runtime status block via `HEALTH_SINK`.

---

## 8. Concurrency Constraints and Lock Hierarchy

### 8.1 Memory Mutexes vs Disk IO Separation

A critical design rule in both Python and Rust: **Never hold an in-memory lock across disk IO, SQLite queries, or connection establishment.**

In `SessionDatabases`:
- `self.homes.lock()`: Held only to inspect or update the `HashMap<String, PathBuf>`. Released before statting or opening files.
- `RecoverableHandleCache.inner.lock()`: Held only to read/write state structs (`handles`, `unavailable`). The actual `SessionDb::open(path)` executes completely outside the lock.
- `SessionDb.conn: Mutex<Connection>`: Synchronizes SQLite transactions per database connection.

### 8.2 Multi-Database Parallelism in WAL Mode

- SQLite databases are opened with `PRAGMA journal_mode=WAL;`.
- Readers do not block writers; writers do not block readers.
- Because each profile owns an independent `SessionDb` instance with its own `Mutex<Connection>`, turns running concurrently on different profiles execute database writes completely in parallel with zero lock contention.

### 8.3 Lock Ordering Hierarchy

To prevent deadlocks, locks must always be acquired in the following strict order:

```
1. SessionTurnLeaseRegistry (per-session turn execution serialization)
   |
2. RoutingIndex internal lock (in-memory routing entries update)
   |
3. SessionDatabases.homes mutex (profile path resolution)
   |
4. RecoverableHandleCache.inner mutex (handle cache bookkeeping)
   |
5. RoutingWriter.state mutex (file persistence of gateway_routing table)
   |
6. SessionDb.conn mutex (SQLite execution inside target database)
```

No code may acquire an earlier lock while holding a later lock.

### 8.4 Safe Shutdown and Handle Drainage Outside Locks

During gateway shutdown:
```rust
pub fn close_all(&self) {
    let (handles, paths) = {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        let handles: Vec<H> = inner.handles.values().cloned().collect();
        let mut paths: Vec<PathBuf> = inner.handles.keys().cloned().collect();
        paths.extend(inner.unavailable.keys().cloned());
        inner.handles.clear();
        inner.unavailable.clear();
        (handles, paths)
    };
    // Drop outside the lock: closing connection flushes WAL and executes IO
    drop(handles);
}
```
Draining handles under the lock and dropping them outside prevents thread lockups during shutdown.

---

## 9. Concrete Implementation Outline for Rust Gateway

### 9.1 Phase 1: Complete `SessionDatabases` Capabilities

Extend `SessionDatabases` in [`crates/hermes-gateway/src/session_db_recovery.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs):

1. **Add Session ID Resolution (`for_session_id`)**:
   Add method resolving database by session ID, querying published routes and an optional hints map:
   ```rust
   pub fn for_session_id(
       &self,
       session_id: &str,
       owner_key: Option<&str>,
       ambient_home: &Path,
   ) -> Option<std::sync::Arc<crate::session_db::SessionDb>> {
       if let Some(key) = owner_key {
           return self.for_key(key, ambient_home);
       }
       // Fall back to ambient store if session ID has no known key owner
       self.open(ambient_home)
   }
   ```
2. **Expose `close_all`**:
   ```rust
   pub fn close_all(&self) {
       self.handles.close_all();
   }
   ```
3. **Expose Cache Status Inspection**:
   Expose `status_for(&self, path: &Path) -> &'static str` for status and diagnostic endpoints.

### 9.2 Phase 2: Connect `RoutingIndex` to `SessionDatabases`

In [`crates/hermes-gateway/src/session_routing.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs):

1. Update `RoutingIndex::recover_from_db`, `query_recoverable`, and `prune_stale` callers to pass a closure resolving from `SessionDatabases`:
   ```rust
   let db_lookup = |key: &str| stores.for_key(key, &ambient_home);
   ```
2. Provide a helper method on `RoutingIndex` to extract the owning key for a session ID:
   ```rust
   pub fn owner_key_for_session_id(&self, session_id: &str) -> Option<&str> {
       self.entries.values()
           .find(|e| e.session_id == session_id)
           .map(|e| e.session_key.as_str())
   }
   ```

### 9.3 Phase 3: Migrate `main.rs` and `AppState` Database Ownership

In [`crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs):

1. Replace static single-handle initialization:
   ```rust
   // Replace lines 443-449:
   let databases = std::sync::Arc::new(session_db_recovery::SessionDatabases::new(
       config_file::hermes_root(),
       config_file::hermes_home(),
       config.multiplex_profiles,
   ));
   ```
2. Update `AppState` in [`crates/hermes-gateway/src/health.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/health.rs):
   Replace `pub session_db: Option<Arc<SessionDb>>` with `pub databases: Arc<SessionDatabases>`.
3. Update `shutdown_flush::recover_pending_to_db` startup call to use `databases.routing()` or ambient handle.

### 9.4 Phase 4: Wire `Dispatcher` Turn Execution

In [`crates/hermes-gateway/src/dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs):

1. Change `Dispatcher.session_db: Option<Arc<SessionDb>>` to `Dispatcher.databases: Arc<SessionDatabases>`.
2. When starting a turn in `handle_message`:
   ```rust
   let session_key = msg.session_key.as_deref().unwrap_or("");
   let db = self.databases.for_key(session_key, &self.ambient_home);
   let history = crate::session_db::begin_turn(db.as_deref(), manages, &msg, &source);
   ```
3. This guarantees that messages routed to profile `work` write their turn history into `profiles/work/state.db`, while default messages write to the root database.

### 9.5 Phase 5: Hook Clean Teardown on Process Termination

In [`crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs):

In the graceful shutdown sequence after draining platform push paths:
```rust
// In main.rs around line 475:
tracing::info!("shutdown signal received, draining");
shutdown.cancel();

// Drain and close all cached SQLite handles across all profiles
state.databases.close_all();
tracing::info!("all profile session database handles closed");
```

This prevents lingering WAL write locks, ensuring replacement processes or CLI commands immediately access all profile databases without locking errors.
