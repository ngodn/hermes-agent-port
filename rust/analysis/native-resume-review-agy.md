# Native `/resume` and `/sessions` Review

## Executive Summary

The uncommitted native `/resume` and `/sessions` checkpoint implements a solid, disciplined core for route repointing, lease serialization, prompt-cache separation, and atomic SQLite state transitions. The lease acquisition hierarchy eliminates deadlocks, and the SQLite transaction in [`SessionDb::switch_gateway_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1034-L1111) coordinates route, peer, generation, and session reopening changes atomically.

However, the checkpoint is currently blocked from being useful in production by an architectural feature deadlock: the listing query filters exclusively for titled sessions, but native Rust does not yet implement `/title` or auto-titling, and `/sessions full` is hard-disabled. In addition, there is a high-severity IDOR boundary gap where `user_id` ownership is not verified for DM and shared channel routes.

Below is the ranked review of blocking issues, required fixes, architectural findings, and explicit deferral boundaries.

---

## Ranked Findings

### Finding 1 (Blocker): Titled-Only Listing and Disabled `/sessions full` Lock Users Out of All Native Resume Functionality

- **Files and Symbols**:
  - [`SessionDb::list_titled_sessions`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1265-L1303)
  - [`session_commands::resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L138-L442)
  - [`session_commands::parse_resume_request`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L33-L104)
- **Impact**:
  Native Rust creates sessions with `title = NULL` because `/title` is not implemented in [`slash.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slash.rs#L83-L94), auto-titling is unported, and [`session_commands::reset_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L562-L564) explicitly discards titles with `"Session titles are not available in the native gateway yet."`.
  In [`SessionDb::list_titled_sessions`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1283), the query enforces `WHERE s.title IS NOT NULL AND TRIM(s.title) != ''`. At the same time, [`resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L162-L166) rejects `include_unnamed` with `"Full session listing is not available in the native gateway yet."`.
  As a result:
  1. Bare `/sessions` and bare `/resume` always return `"No named sessions found. Use /title My Session to name your current session, then /resume My Session to return to it later."`.
  2. The reply instructs the user to run `/title`, which is unported and unhandled.
  3. Numeric `/resume 1` looks up from `list_titled_sessions` ([`session_commands.rs:256`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L256)), so it always fails with `"Resume index 1 is out of range."`.
  4. The only way to resume is typing a raw UUID (`/resume <session_id>`), which users cannot discover.
  Tests in [`message.rs:1054`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L1054) and [`dispatch.rs:1150`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L1150) could only pass by manually injecting `UPDATE sessions SET title = 'First Work' ...` via raw SQLite connections.
- **Remediation**:
  Because `/title` and auto-titling are deferred, the default own-route listing must not hide untitled sessions. Fall back to preview-based labeling when `title` is `NULL` (or format as `COALESCE(s.title, s.preview, 'Untitled session')`), and remove the prompt instructing users to use `/title`.

---

### Finding 2 (High / Security): IDOR Boundary Gap in `resume_target_allowed` for Multi-User DM and HTTP Routes

- **Files and Symbols**:
  - [`session_commands::resume_target_allowed`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L106-L136)
  - [`SessionStore::lookup_by_session_id`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L242-L254)
  - Python reference: [`gateway/slash_commands.py:1204-1221`](file:///home/eins0fx/development/hermes-agent-port/gateway/slash_commands.py#L1204-L1221)
- **Impact**:
  In [`resume_target_allowed`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L132-L135):
  ```rust
  Ok(row["session_key"].as_str() == Some(route_key.as_str())
      && row["source"].as_str() == Some(source.platform.as_str())
      && row["chat_id"].as_str() == Some(source.chat_id.as_str())
      && row["thread_id"].as_str().unwrap_or("") == source.thread_id.as_deref().unwrap_or(""))
  ```
  This checks `session_key`, `source`, `chat_id`, and `thread_id`, but completely omits checking `user_id`.
  On HTTP (`Platform::Cli`), [`source_from_message`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session.rs#L241-L250) defaults `chat_type` to `"dm"`, `chat_id` to `message.channel_id`, and `user_id` to `message.sender_id`. In [`build_session_key`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session.rs#L355-L371), when `chat_id` is present, the key is `agent:main:local:dm:{channel_id}` and does not contain `user_id`.
  If user Alice uses HTTP endpoint with `channel_id: "public", sender_id: "alice"`, and user Bob later issues `/resume <alice_session_id>` with `channel_id: "public", sender_id: "bob"`, Bob's `route_key`, `chat_id`, and `source` match Alice's session. Because `user_id` is not verified, Bob can resume Alice's session and read her transcript history.
  Python's reference `_resume_target_allowed` explicitly guards against this (`CWE-639`) by requiring `bool(row_uid) and row_uid == caller_uid` for DMs, and also scopes multi-user sessions when `group_sessions_per_user` is enabled.
- **Remediation**:
  In [`resume_target_allowed`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L106-L136), check that `row["user_id"]` (and `entry.origin.user_id`) equals `source.user_id` when caller identity is present in DM or per-user configurations. In [`SessionDb::list_titled_sessions`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1283), scope the query by `user_id` on DM routes when `user_id` is provided.

---

### Finding 3 (Medium / Compatibility): Preview Subquery Selects the Last User Message Instead of the First

- **Files and Symbols**:
  - [`SessionDb::list_titled_sessions`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1281)
  - Python reference: [`hermes_state.py:11950-11957`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L11950-L11957), [`hermes_state.py:12033`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L12033)
- **Impact**:
  Line 1281 of [`session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1281) runs:
  ```sql
  (SELECT m.content FROM messages m
   WHERE m.session_id = s.id AND m.active = 1
     AND m.role = 'user' AND m.content IS NOT NULL
   ORDER BY m.id DESC LIMIT 1)
  ```
  `ORDER BY m.id DESC` retrieves the latest user message. In Python, `preview` is defined as the first user message (order by `m.timestamp, m.id ASC LIMIT 1`).
  Extracting the last message causes session preview labels in listing menus to change on every turn instead of preserving the session topic.
- **Remediation**:
  Change `ORDER BY m.id DESC LIMIT 1` to `ORDER BY m.id ASC LIMIT 1`.

---

### Finding 4 (Medium / Correctness): Numeric Resume Reply Emits Literal Digit in Place of Title

- **Files and Symbols**:
  - [`session_commands::resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L250-L265)
  - [`session_commands::resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L330-L333)
  - [`session_commands::resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L431-L438)
  - Python reference: [`gateway/slash_commands.py:5305`](file:///home/eins0fx/development/hermes-agent-port/gateway/slash_commands.py#L5305)
- **Impact**:
  When a user runs numeric `/resume 1`, `target_name` remains `"1"`. If the session has no stored title or if the session is already active:
  1. [`resume_session:331`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L331) formats `"📌 Already on session **1**."`.
  2. [`resume_session:431`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L431) falls back to `stored_title.unwrap_or(target_name)`, yielding `"↻ Resumed session **1** (N messages). Conversation restored."`.
  Python explicitly re-binds `name = target.get("title") or name`.
- **Remediation**:
  When resolving a numeric index from the listing row, update `target_name` to the row's title or preview string.

---

### Finding 5 (Low / Architectural): Unnecessary Dummy Session Lifecycle via `reset_session` on Cold Routes

- **Files and Symbols**:
  - [`session_commands::resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L282-L301)
- **Impact**:
  When `/resume` is called on a fresh route where `current_entry_for_source` is `None`, `resume_session` calls full [`reset_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L488-L565). This mints a UUID, persists a dummy row, and invokes `agent.retire_conversation`. A millisecond later, the resume loop repoints the route away, updates the dummy row to `end_reason = 'session_switch'`, and bumps `conversation_generations`. This leaves a zero-message orphan in SQLite.
- **Remediation**:
  Allow [`SessionStore::switch_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L177-L240) to bind an initial route candidate directly without materializing and retiring a throwaway session.

---

### Finding 6 (Low / Cleanliness): Dead Code in Listing Header Branch

- **Files and Symbols**:
  - [`session_commands::resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L162-L166)
  - [`session_commands::resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L207-L211)
- **Impact**:
  Line 207 checks `if request.include_unnamed { "📋 **Sessions**\n" } else { "📋 **Named Sessions**\n" }`. However, line 163 already rejected `request.include_unnamed` with an early return, making the `include_unnamed` branch unreachable.
- **Remediation**:
  Consolidate listing branches when `include_unnamed` support is enabled or removed.

---

## Detailed Focus Area Analysis

### 1. Security and IDOR Boundaries
- **Status**: Vulnerable on shared/DM channels (Finding 2).
- **Admin Widening**: The check [`is_explicit_admin`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slash.rs#L118-L121) correctly requires `policy.enabled && policy.is_admin(...)`. Disabled slash policy defaults to rejecting cross-origin data widening, matching Python's security posture.
- **Path Sanitization**: [`SessionEntry::resumed_candidate`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_entry.rs#L84-L105) runs `SessionEntry::from_dict`, which asserts `!is_path_unsafe(&session_id)` and `!is_session_key_unsafe(&session_key)`.

### 2. Prompt-Cache Identity and Client Retirement Semantics
- **Status**: Sound.
- **Cache Identity**: Unlike Python (which keys agent instances by `session_key`), Rust's [`ConversationAgent`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L108-L113) keys cached clients by `(PathBuf, String)` where the string is `message_session_id`.
- **Retirement**: Resuming another session rebinds the route's `session_id`. The subsequent turn automatically checks out the target session's client. The outgoing session's client remains in cache under its own ID and idles out via soft `RetirementKind::Release`. No destructive `SessionEnd` is fired, correctly matching Python's `session_switch` boundary. No cache eviction or prompt re-warming is required.

### 3. Route and Transcript Lease Ordering
- **Status**: Sound and deadlock-free.
- **Hierarchy**:
  1. [`route_leases.acquire`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L305-L315) is taken first.
  2. The current route entry is observed.
  3. Transcript IDs are collected and sorted:
     ```rust
     let mut ids = vec![observed.session_id.clone(), target_id.clone()];
     ids.sort();
     ids.dedup();
     ```
  4. Both transcript leases are acquired in ascending lexicographical order.
- **Deadlock Prevention**: Sorting transcript IDs guarantees that two concurrent switches involving the same two sessions acquire leases in identical order, avoiding ABBA deadlocks.
- **In-Flight Turn Serialization**: Holds transcript lease during switch; concurrent in-flight turns finish persisting to the outgoing session before the switch proceeds. This is verified by [`message.rs:1425-1490`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L1425-L1490).

### 4. SQLite Transaction Atomicity
- **Status**: Sound.
- **Transaction Discipline**: [`SessionDb::switch_gateway_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1034-L1111) executes in an `Immediate` transaction:
  - Validates `target_id` presence.
  - Updates outgoing session to `end_reason = 'session_switch'` and bumps `conversation_generations`.
  - Reopens target session (`ended_at = NULL, end_reason = NULL`) and stamps `_reset_from` on legacy children.
  - Rebinds peer information up the compression chain via `COMPRESSION_PEER_CTE`.
  - Updates `gateway_routing` with upsert.
- **In-Memory Concurrency**: [`SessionStore::switch_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L188-L238) holds `self.index.lock()` during the database call, ensuring in-memory route maps are updated only when the SQLite transaction succeeds.
- **Rollback Verification**: Tested via `CREATE TRIGGER reject_resume_route ... RAISE(ABORT)` in [`session_db.rs:2385`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L2385).

### 5. Title Resolution and Continuation Logic
- **Status**: Mostly sound, but blocked by lack of title persistence.
- **Lookup Sequence**: [`SessionDb::resolve_session_target`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1234-L1263) tests:
  1. Exact session ID.
  2. Numbered title continuations (`LIKE target #% ESCAPE '\'` ordered by `started_at DESC`).
  3. Exact title match.
  This faithfully ports [`hermes_state.py:11624-11651`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L11624-L11651).
- **Compression Tip**: Target session resolves to lineage tip via [`SessionDb::get_compression_tip`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L696) before lease acquisition.

### 6. Parser Compatibility
- **Status**: Sound.
- **Shell Parsing**: Uses `shell_words::split` in [`parse_resume_request`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L33-L104), handling unbalanced quotes with clear feedback.
- **Sub-grammar**: Sub-keywords (`list`, `ls`, `browse`, `all`, `--all`, `full`, `--full`, `search`, `find`) and quotation wrappers (`<...>`, `[...]`, `"..."`, `'...'`) are parsed cleanly.
- **Command Classification**: In [`slash.rs:88`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slash.rs#L88), both `"resume"` and `"sessions"` classify into `NativeSlashCommand::Resume` with `from_sessions` flag preserved.

### 7. HTTP and Push Ingress Wiring
- **Status**: Sound.
- **Wiring Symmetry**: Both [`dispatch.rs:326-361`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L326-L361) and [`message.rs:157-194`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L157-L194) wire [`resume_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L138) using identical `AdmissionDeps` and parameter structures.
- **Confirmation Handling**: Pending confirmation state in [`SlashConfirmations`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slash_confirm.rs) is cleared upon a successful session switch.

### 8. Deterministic Test Coverage
- **Status**: High coverage on concurrency and storage; missing IDOR regression test.
- **Existing Coverage**:
  - `session_db::tests::resume_catalog_resolves_ids_titles_lineage_and_exact_lane`: verifies LIKE escaping, title continuation, preview, and message count.
  - `session_db::tests::resume_transaction_rolls_back_every_durable_change`: verifies complete transaction rollback under trigger abort.
  - `session_store::tests::explicit_resume_commits_route_boundary_and_reopen_together`: verifies CAS fencing, route re-pointing, and reopening.
  - `dispatch::tests::push_reset_and_resume_rotate_without_forwarding_control_messages`: verifies push dispatch never invokes model turns for control operations.
  - `message::tests::http_message_endpoint_handles_slash_reset_confirmation_and_rotation`: verifies in-flight turn lease serialization, route isolation, and `already_on` detection.
- **Missing Test**:
  - A test where two different callers (`sender_id: "alice"` vs `sender_id: "bob"`) communicate over the same HTTP `channel_id`. This test would have caught Finding 2.

---

## Required Fixes vs Explicitly Deferred Work

### Required Fixes (Must Be Resolved Before Merge)
1. **Fix Listing Blocker**: Remove the strict `s.title IS NOT NULL` requirement when listing own-route sessions, or allow untitled preview listing by default so users can discover and resume sessions. Do not recommend running `/title`.
2. **Fix IDOR Gap**: Add `user_id` equality check to [`resume_target_allowed`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L106-L136) and [`list_titled_sessions`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1283) for DM and per-user routes.
3. **Fix Preview Direction**: Switch [`session_db.rs:1281`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1281) from `m.id DESC` to `m.id ASC`.
4. **Fix Numeric Resume Feedback**: Re-bind `target_name` to the resolved session title or preview to prevent printing `"📌 Already on session **1**."`.

### Explicitly Deferred Work (Safe to Defer)
1. **`/title` Slash Command & `/new <title>` Persistence**: Storing user-specified titles remains deferred.
2. **Auto-Titling Pipeline**: Background LLM and heuristic first-turn title generation remains deferred.
3. **`/sessions search <query>`**: FTS over session titles and previews remains deferred (currently returns clean unavailable message).
4. **Full `resolve_resume_session_id` Parent Walk**: The forward descendant tree walk with reset exclusion is deferred; [`get_compression_tip`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L696) covers compression continuations for this milestone.
5. **Matrix Cross-Room Semantics**: Matrix-specific room origin guards (`_same_matrix_room`) remain deferred.
6. **Adapter UI Menus and Buttons**: Text-only numbered list matches gateway requirements; interactive button callbacks remain deferred.
