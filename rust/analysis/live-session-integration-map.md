Codex review notes: this report is a call-site map with claims to recheck.
There is no gateway_session_peer table; peer metadata is stored on sessions.
The routing index has one fixed owning database and contains all profile keys,
not one independently persisted index per profile. Python gateway IDs use a
local timestamp plus UUID suffix, so the schema does not require UUID-only IDs.
The suggested Python stream_turn edits are outside the Rust-only port scope.
Verify bridge history behavior directly before accepting the reported defect.

# Live Session Integration Map: Transitioning to Durable Routing Session IDs

## 1. Architectural Gap: Message-Derived IDs vs. Durable Routing Session IDs
The Rust gateway currently conflates routing keys with durable session identifiers:
- `session_db.rs:146-167` computes `message_session_id(&Message)` by concatenating platform, workspace, channel, and thread (e.g. `Slack:workspace:...` or `telegram:12345`).
- This temporary string is directly used as `sessions.id` and `messages.session_id` (`session_db.rs:184-199`, `210-211`), bypassing the routing index and schema contracts.
- In Python (`gateway/session.py:2848-3240`, `gateway/run.py:21301-21612`), session management separates two distinct concepts:
  1. `session_key`: Deterministic routing key (`build_session_key`, e.g. `agent:main:telegram:dm:12345`).
  2. `session_id`: Durable timestamped UUID (`YYYYmmdd_HHMMSS_xxxxxxxx`) serving as the SQLite primary key.
- Conflation causes three severe failures:
  1. Lack of rotation/reset: All turns for a channel collapse into one un-resettable session.
  2. History partition: Python gateway and desktop sessions (keyed by UUID) are invisible to Rust.
  3. Leases and invalidations: Turn leases and run generations cannot correctly track session lifecycles.

## 2. Python Phased `get_or_create` Runtime Lifecycle
`gateway/session.py:2848-3240` defines a 6-phase single-flight resolution pipeline per `session_key`:
- Single-Flight Gate (`gateway/session.py:2863-2901`): `_inflight_sessions` deduplicates concurrent turns for the same `session_key`.
- Phase 0 (Lock Read + Async DB I/O, lines 2969-2983): Reads cached `existing_session_id` from `_entries[session_key]`; resolves compression tips outside lock via `_compression_tip_for_session_id`.
- Phase 1 & 1b (Stale Check & Policy, lines 2984-3025): Verifies if session ended in SQLite (`_is_session_ended_in_db`); evaluates reset policy (`_should_reset`: idle, daily, suspended, or resume-pending expiration).
- Phase 2 (Lock Write, lines 3026-3106): Mutates `_entries`. Heals compression tips; on reset or stale entry, evicts old entry and sets `_needs_recover = True`. Healthy routes update `updated_at`.
- Phase 3 (Lock-Free DB Recovery, lines 3107-3140): When `_needs_recover`, calls `_query_recoverable_session` and claims legacy Slack keys. Reopens recoverable sessions in SQLite via `db.reopen_session()`.
- Phase 4 (Candidate Creation, lines 3141-3198): If no session exists, creates a fresh `SessionEntry` with UUID `session_id = f"{now.strftime('%Y%m%d_%H%M%S')}_{uuid.uuid4().hex[:8]}"` and publishes under lock.
- Phase 5 (Persist Routing, lines 3199-3204): Persists entry via single-row UPSERT (`_save_entry`) or full table rewrite (`_save_entries`) into `gateway_routing`.
- Phase 6 (Lock-Free SQLite Operations, lines 3205-3240): Calls `promote_to_session_reset` on predecessor row; calls `create_session` and `_record_gateway_session_peer` for new candidate.

## 3. Exact Runtime Call Sites to Replace in Rust
1. `rust/crates/hermes-gateway/src/dispatch.rs`:
   - Line 143 (`deliver`): Replace `let session_id = crate::session_db::message_session_id(to);` with durable `session_id` from resolved `SessionEntry`. Passes into `delivery_ledger.record_obligation` (lines 153-160).
   - Lines 272-284 (`handle_turn`): Replace `let session_id = crate::session_db::message_session_id(&msg);` and `self.generation.fetch_add(1, Ordering::Relaxed)` with:
     - Generate `SessionSource` from `msg`.
     - Build `session_key = crate::session::build_session_key(&source, ...)`.
     - Resolve `SessionEntry` via `RoutingIndex` get-or-create logic.
     - Fetch monotonic token `run_generation = session_registry.begin_run_generation(&session_key)`.
     - Acquire lease: `self.lease.acquire(&entry.session_id, &session_key, run_generation, timeout)`.
   - Lines 289-293 (`handle_turn`): Replace `begin_turn(..., &msg, &source)` by passing resolved `&entry.session_id`, `&session_key`, and `source`.
   - Lines 334-335 (`handle_turn`): Replace `end_turn(..., &msg, &reply)` by passing resolved `&entry.session_id`.
2. `rust/crates/hermes-gateway/src/message.rs`:
   - Lines 122-124 (`post_message`): Construct `SessionSource` (platform: local/cli), resolve durable `session_key` (`agent:main:local:dm:local`) and `SessionEntry`. Pass `entry.session_id` into `begin_turn`.
   - Line 151 (`post_message`): Pass `entry.session_id` into `end_turn`.
3. `rust/crates/hermes-gateway/src/session_db.rs`:
   - Lines 146-167 (`message_session_id`): Deprecate runtime usage. Retain only for legacy Slack key fallback migrations.
   - Lines 172-201 (`begin_turn`): Update signature to `begin_turn(db, manages_history, session_id, session_key, source, msg, channel_id, chat_type)`. Replace line 184 (`sid = message_session_id(msg)`) with parameter `session_id`. Populate `session_key` in `db.ensure_session` (line 185).
   - Lines 205-213 (`end_turn`): Update signature to accept `session_id: &str`. Replace line 210 with the passed `session_id`.
4. `rust/crates/hermes-gateway/src/native_agent.rs`:
   - Lines 452-454 (`run_turn`): Replace `turn_client.cache_scope = Some(crate::session_db::message_session_id(msg));` with the resolved durable routing `session_key` or `session_id` for prompt cache boundaries (`prompt_cache::apply`, lines 408-415).
5. `rust/crates/hermes-gateway/src/main.rs`:
   - Lines 317-330 (`start_push_path`): Provide `Dispatcher` with `Arc<SessionRegistry>` and `Arc<Mutex<RoutingIndex>>` (or a unified `SessionStore`).
   - Lines 443-452 (`main`): Expand single root `state.db` handle to dynamic profile-aware DB resolution matching `gateway/session.py:1516-1550`.

## 4. Profile Scope and History Propagation
- Profile Matching: Inbound messages are mapped to a profile via `ProfileRoute` rules in `profile_routing.rs:64-150` (matching guild, channel, thread, and WhatsApp identifiers).
- Key Namespacing: `session.rs:271-277, 318-323` embeds the profile into `session_key` (`agent:<profile>:<platform>:...`). Default/empty maps to `agent:main:...`.
- DB Isolation:
  - Root profile writes to `~/.hermes/state.db` (`session_db.rs:223-228`).
  - Named profiles write to `~/.hermes/profiles/<profile>/state.db` (`session_db.rs:229-235`, `gateway/session.py:1516-1550`).
- Routing Scope: `RoutingIndex.scope` (`session_routing.rs:214-217`) reflects the profile's canonical sessions directory. Each profile stores entries under its own `gateway_routing(scope, session_key)` SQLite partition.
- History Retrieval: `begin_turn` loads history via `db.load_history(session_id, HISTORY_LIMIT)` (`session_db.rs:192`) on the specific profile's database handle. `NativeAgentClient` receives this history slice (`native_agent.rs:54-65`) and `CliAgentClient` renders it (`cli_agent.rs:44-58`).

## 5. History Compatibility Across Boundaries
- SQLite Table Contracts:
  - `sessions` (`session_db.rs:956-968`): `id` is primary key (UUID), `session_key` is routing key.
  - `messages` (`session_db.rs:971-989`): `session_id` references `sessions(id)`.
  - `gateway_routing` (`session_db.rs:945-953`): `(scope, session_key)` -> `entry_json`.
  - `gateway_session_peer` (`session_db.rs:512-535`): Maps `session_id` to routing metadata.
- Current Rust Incompatibility: Writing `message_session_id` directly to `sessions.id` and `messages.session_id` prevents Python gateway, desktop apps, and TUI from finding conversation histories. Python queries expect UUIDs linked via `gateway_routing`.
- Unified Continuity: Using durable `session_id` generated during `get_or_create` allows bidirectional history compatibility. Structured message content encoded by `encode_message_content` (`session_db.rs:198`) already aligns with Python `hermes_state.py`.

## 6. Python-Bridge Backend Defect and Remediation
- The Amnesia Defect:
  - `agent.rs:179-181`: `SubprocessAgentClient::manages_history()` returns `true`.
  - `dispatch.rs:289-293` and `message.rs:122` skip `begin_turn` and `end_turn`, passing empty history.
  - `agent.rs:194-206`: `SubprocessAgentClient` spawns `python -m hermes_cli.stream_turn -p -` with prompt only.
  - `hermes_cli/stream_turn.py:88, 106-118`: Spawns `AIAgent(..., quiet_mode=True)` with `session_id=None`.
  - `agent/agent_init.py:1685-1692`: When `session_id` is None, `AIAgent` auto-generates a fresh UUID every single turn.
  - Impact: The Python subprocess bridge is completely amnesic across turns; history is discarded after every reply.
- Remediation Path:
  1. Update `SubprocessAgentClient::run_turn` (`agent.rs:183-210`) to forward `--session-id <id>`, `--session-key <key>`, and `--profile <profile>` as CLI arguments.
  2. Update `hermes_cli/stream_turn.py:60-71, 106-118` to parse `--session-id`, `--session-key`, and `--profile`.
  3. Pass `session_id`, `gateway_session_key`, and `profile` into `AIAgent`, binding them via `set_session_vars` (`gateway/session_context.py:224-260`).
  4. This restores multi-turn history, session lineage, and profile memory to the subprocess bridge.

## 7. Reusable Existing Types in Rust Gateway
- `SessionSource` (`session.rs:81-107`): Platform origin descriptor.
- `build_session_key` (`session.rs:318-372`): Deterministic routing key generator.
- `SessionEntry` (`session_entry.rs:58-65`): In-memory and persisted routing state.
- `RoutingIndex` (`session_routing.rs:197-207`): Persistent routing table and fast-path writer.
- `RecoveryRequest` & `SessionRecovery` (`session_routing.rs:15-33`): Database recovery and Slack key migration.
- `SessionRegistry` (`session_registry.rs:33-36`): Per-session active state and monotonic run generation tokens.
- `SessionState` & `AgentSlot` (`session_state.rs`): In-flight agent slot and image buffers.
- `SessionTurnLeaseRegistry` (`turn_lease.rs:26-36`): Concurrency serializer per session ID.
- `SessionDb` (`session_db.rs:16-1150`): SQLite transcript and metadata accessors (`reopen_session`, `promote_to_session_reset`, `append_message`, `load_history`).
- `ProfileRoute` (`profile_routing.rs:65-74`): Route classifier mapping platform traffic to profiles.
