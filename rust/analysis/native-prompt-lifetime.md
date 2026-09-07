> Follow-up: ConversationAgent and run_turn_in_home now route native production
> turns by selected profile home and resolved conversation ID in both ingress
> paths. The singleton-only findings below describe the earlier checkpoint.
> Prompt construction/restoration, eviction and runtime-drift handling remain
> proposals to implement and verify against the Python reference.

# Native Agent Lifetime and Per-Conversation Prompt Initialization Design

## Executive Summary

This document traces the lifetime of the native agent client in the Rust gateway (`rust/crates/hermes-gateway`), contrasts it with the agent caching, reuse, and prompt lifecycle in Python (`gateway/run.py` and `agent/system_prompt.py`), and provides a concrete, actionable design for per-conversation prompt initialization across push and HTTP ingress paths.

All findings are divided strictly into **Source Evidence** (what the codebase currently implements) and **Proposals** (the recommended architectural changes).

---

## Source Evidence: Native Agent Lifetime in Rust

### 1. Lifetime in `main.rs`

* Source reference: [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L200-L210), [`main.rs:L215-L396`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L215-L396), [`main.rs:L412-L460`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L412-L460), [`main.rs:L542-L557`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L542-L557), [`main.rs:L639-L680`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L639-L680).
* Construction:
  * During startup, `main()` resolves configuration and calls `build_agent_client(&config, &user_config, configured_model.as_deref())`.
  * If `config.agent_native` is `true`, this invokes `build_agent_client_for_home(config, user_config, model, &config_file::hermes_home())`.
  * `build_agent_client_for_home` resolves provider profiles, environment variables, extra headers, reasoning parameters, request overrides, and native tools (`CurrentTimeTool` when `agent_tools` is enabled).
  * It constructs a `NativeAgentClient` via `NativeAgentClient::new(model, key, base_url)` and applies configuration options.
  * Crucially, `system_prompt` is left as `None`. The builder method `with_system_prompt` is never called.
  * The resulting client is wrapped in `Arc<dyn AgentClient>` and stored in `AppState.agent`.
* Distribution:
  * For push paths, `start_push_path` is called for Telegram, Discord, and Slack. It instantiates a `Dispatcher` passing `state.agent.clone()`.
  * For HTTP ingress, `state` is attached to the Axum router via `.with_state(state.clone())`.
* Scope and Lifetime:
  * `state.agent` is a process-level singleton. It lives from gateway boot until process shutdown.

### 2. File Status of `ingress.rs`

* Source reference: Filesystem search across `/home/eins0fx/development/hermes-agent-port`.
* Evidence: There is no file named `ingress.rs` in the codebase.
* Gateway ingress is split across two separate entry points:
  1. HTTP Ingress: Implemented in [`rust/crates/hermes-gateway/src/message.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L79-L245) via `post_message` handling `POST /message`.
  2. Push Ingress: Initialized in [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L412-L460) via `start_push_path` and driven by `Dispatcher::run` in [`rust/crates/hermes-gateway/src/dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L141-L148).

### 3. Lifetime in `dispatch.rs` (Push Ingress Path)

* Source reference: [`rust/crates/hermes-gateway/src/dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L30-L52), [`dispatch.rs:L225-L381`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L225-L381), [`dispatch.rs:L383-L454`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L383-L454).
* Flow:
  * `Dispatcher` holds `agent: Arc<dyn AgentClient>` (the process-level singleton passed during `start_push_path`).
  * In `Dispatcher::handle_turn(msg)`:
    * Inbound audio is transcribed if attachments exist and STT is configured.
    * Slash commands are evaluated (`slash::evaluate`).
    * Session is resolved through `SessionStore::get_or_create_with_legacy`, yielding `entry: SessionEntry`, `routing_key: Option<String>`, and `turn_db: Option<Arc<SessionDb>>`.
    * Turn lease is acquired from `self.lease.acquire(&session_id, &msg.sender_id, generation, None)`.
    * Once admitted, `self.run_admitted_turn(msg, turn_db, manages, routing_key, _lease)` is spawned.
  * In `Dispatcher::run_admitted_turn`:
    * History is loaded and the user message is appended: `let history = crate::session_db::begin_turn(turn_db.as_deref(), manages, &msg, &source);`.
    * The agent reference is cloned directly from the dispatcher struct: `let agent = Arc::clone(&self.agent);`.
    * The turn runs asynchronously: `tokio::spawn(async move { agent.run_turn(&msg_for_agent, &history, tx).await });`.
    * The assistant reply is recorded: `crate::session_db::end_turn(turn_db.as_deref(), manages, &msg, &reply);`.
    * Session activity is updated in `session_store`.
    * Reply is delivered back to the platform adapter.
* Agent Lifetime:
  * The dispatcher holds the process-level singleton `agent`. No conversation-specific client is created or cached.

### 4. Lifetime in `message.rs` (HTTP Ingress Path)

* Source reference: [`rust/crates/hermes-gateway/src/message.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L79-L245).
* Flow:
  * `post_message(State(state), Json(payload))` receives the HTTP request.
  * Slash commands are evaluated.
  * Session is resolved via `state.session_store` (`get_or_create_with_legacy`).
  * Turn lease is acquired via `state.turn_leases.acquire(...)`.
  * An admitted turn task is spawned:
    * History is loaded: `let history = crate::session_db::begin_turn(turn_db.as_deref(), manages, &msg, "cli");`.
    * The agent is cloned directly from shared state: `let agent = state.agent.clone();`.
    * The turn executes: `tokio::spawn(async move { agent.run_turn(&msg_for_agent, &history, tx).await });`.
    * Stream events are aggregated into `reply`.
    * `crate::session_db::end_turn` records the assistant message.
    * Activity is touched in `state.session_store`.
* Agent Lifetime:
  * Exactly matches `dispatch.rs`: `state.agent` is cloned from `AppState`. No conversation-specific prompt or client is attached.

### 5. Lifetime in `native_agent.rs`

* Source reference: [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L192-L213), [`native_agent.rs:L215-L251`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L215-L251), [`native_agent.rs:L457-L517`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L457-L517).
* Struct definition:
  * `NativeAgentClient` holds `system_prompt: Option<std::sync::Arc<str>>`.
  * The doc comment notes: "Already assembled conversation prompt. Clones share the same immutable bytes, including across tool rounds; construction never reads files here."
  * `with_system_prompt(mut self, prompt: impl Into<String>) -> Self` is annotated: `#[allow(dead_code)] // Consumed by the pending conversation prompt assembler.`
  * The doc comment explicitly states: "Prompt assembly and persisted-session restoration belong to the caller; this client keeps the supplied bytes unchanged throughout its lifetime."
* Turn Execution:
  * In `run_turn(&self, msg, history, events)`:
    * It clones itself: `let mut turn_client = self.clone();`.
    * It sets `turn_client.cache_scope = Some(crate::session_db::message_session_id(msg));`.
    * If `client.system_prompt` is `Some(prompt)`, it prepends `HistoryMessage { role: "system".into(), content: prompt.to_string() }` to `history`.
    * If `client.system_prompt` is `None` (current production behavior), no system prompt is added to `history`.
    * If `client.tools` is non-empty, it calls `crate::native_tools::run_tool_loop_with_content`. Otherwise, it makes an HTTP streaming call to `{base_url}/chat/completions`.
* Agent Lifetime:
  * `NativeAgentClient` is cloned per turn to set `cache_scope`, but holds `system_prompt: None` throughout the lifetime of the process unless constructed via `with_system_prompt`.

### 6. Audit of All `AgentClient::run_turn` Call Sites in `hermes-gateway/src`

Every call site of `run_turn` across `rust/crates/hermes-gateway/src` was audited:

* **Production Call Sites (2 total)**:
  1. [`rust/crates/hermes-gateway/src/dispatch.rs:L401`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L401): Inside `Dispatcher::run_admitted_turn`, invoked on `agent = Arc::clone(&self.agent)`.
  2. [`rust/crates/hermes-gateway/src/message.rs:L198`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L198): Inside `post_message` admitted turn task, invoked on `agent = state.agent.clone()`.

* **Test Call Sites**:
  1. `main.rs`:
     * [`main.rs:L874`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L874): Testing native client wiring with and without tools.
     * [`main.rs:L923, L947, L966, L980, L995, L1016, L1048, L1065, L1088, L1110, L1111, L1147`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L923-L1147): Provider profile registration, fallback models, request overrides, and multi-profile config tests.
  2. `native_agent.rs`:
     * [`native_agent.rs:L906, L1278, L1297, L1302, L1313, L1314, L1339, L1347, L1362, L1383, L1401, L1409, L1564`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L906-L1564): Unit tests for tool calling, streaming SSE, prompt injection, and cache scoping.
  3. `message.rs`:
     * [`message.rs:L552, L664`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L552): Test mocks verifying lease contention and history ownership.
  4. `dispatch.rs`:
     * [`dispatch.rs:L466, L543, L619, L676, L782`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L466): Test mocks verifying cancelled waiters, delivery obligations, and slash gating.
  5. `cli_agent.rs`:
     * [`cli_agent.rs:L183, L216`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/cli_agent.rs#L183): Testing CLI agent execution and failure propagation.
  6. `agent.rs`:
     * [`agent.rs:L408`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent.rs#L408): Testing `SubprocessAgentClient` streaming.

---

## Source Evidence: Python Reference Patterns

### 1. Python Agent Caching and Reuse (`gateway/run.py`)

* Source reference: [`gateway/run.py:L6124-L6387`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L6124-L6387), [`gateway/run.py:L7787-L7795`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7787-L7795), [`gateway/run.py:L30578-L30748`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30578-L30748).
* Cache Structure:
  * `self._agent_cache: OrderedDict[str, tuple]` is keyed by `ctx.session_key`.
  * The cached entry tuple is `(agent, _sig, _current_msg_count, ctx.session_id)`.
  * Protected by `self._agent_cache_lock`.
* Turn Lookup and Validation:
  * Before running a turn, `_agent_config_signature` computes `_sig` incorporating model, runtime parameters, toolsets, context skip flags, and user IDs.
  * Stale Dead Session Check: Inspects `_peek_entry[3]` (`session_id`). If the cached agent's `session_id` has already ended in `state.db` (a self-heal artifact), the cached agent is evicted immediately to prevent routing loop resurrecting dead sessions.
  * Cross-Process Write Invalidation: Compares `_sess_row["message_count"]` in `SessionDB` against `_cached_mc`. If another process appended to the transcript, the cached agent is evicted so a fresh agent re-reads the full history from disk.
  * Cache Hit: If `cached[1] == _sig`, moves the session key to the end (MRU), re-initializes transient turn fields via `_init_cached_agent_for_turn(agent, ctx._interrupt_depth)`, and marks `reused_cached_agent = True`.
  * Cache Miss: Constructs fresh `agent = ctx.AIAgent(...)`, stores `_cache[ctx.session_key] = (agent, _sig, _current_msg_count, ctx.session_id)`, and calls `self._enforce_agent_cache_cap()`.
* Eviction Policies:
  * LRU Capacity (`_enforce_agent_cache_cap`): Walks LRU order. Agents currently in `_running_agents` are skipped to protect in-flight turns. Excess entries are popped and soft-released on background daemon threads.
  * Idle TTL Sweep (`_sweep_idle_cached_agents`): Evicts agents whose `last_activity_ts` exceeds idle TTL, deferring eviction if the session store is waiting to run `on_session_end`.
  * Memory Pressure Sweep (`_sweep_agent_cache_under_pressure`): Sheds entries when process anonymous RSS exceeds budget.

### 2. Python Prompt Initialization (`agent/system_prompt.py` and `agent/conversation_loop.py`)

* Source reference: [`agent/system_prompt.py:L1-L24`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L1-L24), [`agent/system_prompt.py:L1036-L1080`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L1036-L1080), [`agent/conversation_loop.py:L993-L1228`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L993-L1228).
* Three-Tier Prompt Structure:
  * `stable`: Cross-session prefix. Identity (`SOUL.md` or default), universal guidance, tool guidance, operational guidance, environment hints, coding prefix, platform hint.
  * `context`: Session-dynamic tier. Workspace snapshot, context files (`AGENTS.md`, `.cursorrules`), `system_message`.
  * `volatile`: Rebuild tail. Skills index, memory snapshot (`MEMORY.md`, `USER.md`), external memory, plugin sections, metadata footer.
* Once-Per-Session Lifecycle (`_restore_or_build_system_prompt`):
  * Check Session DB: If `conversation_history` is non-empty and `agent._session_db` exists, calls `agent._session_db.get_session(agent.session_id)`.
  * Runtime Compatibility Check: If `raw_prompt` exists, calls `_stored_prompt_matches_runtime(agent, stored_prompt)` to verify that model, provider, platform, and terminal cwd match the running process.
  * Verbatim Reuse: If compatible (and Bot Chat capability epoch is not stale), restores `agent._cached_system_prompt = stored_prompt`. Restores tool order prefix and plugin sections. Zero prompt rebuild is performed, keeping the upstream LLM prefix cache completely warm.
  * Fresh Build: If new session, missing row, null prompt, or stale runtime identity:
    * Rebuilds via `agent._build_system_prompt(system_message)`.
    * Fires lifecycle hook `on_session_start`.
    * Persists prompt to database via `agent._session_db.update_system_prompt(agent.session_id, prompt)`.
* Invalidation:
  * `invalidate_system_prompt(agent)` is called only upon context compression events. It clears `_cached_system_prompt` and forces a rebuild on the next turn.

---

## Comparison: Rust Current State vs Python Architecture

| Architectural Dimension | Python Gateway (`gateway/run.py` + `agent/`) | Rust Gateway (`hermes-gateway`) |
| :--- | :--- | :--- |
| **Agent Backend Lifetime** | Per-session `AIAgent` instances cached in an LRU `_agent_cache` map. | Process-wide singleton `Arc<dyn AgentClient>` created at startup in `main.rs`. |
| **Agent Statefulness** | Stateful: `AIAgent` holds in-memory transcript, tool subprocesses, and cached prompt. | Stateless: `NativeAgentClient` holds only HTTP client and configuration. History is stored in SQLite. |
| **Prompt Storage** | Stored in `sessions.system_prompt` in `state.db`. | Stored in `system_prompts` (SHA-256 deduplicated) and `sessions.system_prompt_hash` in `session_db.rs`. |
| **Prompt Restoration** | Restores verbatim from DB via `_restore_or_build_system_prompt` if runtime matches. | DB read (`get_session`) and write (`update_system_prompt`) exist, but are never called from gateway turn paths. |
| **System Prompt Injection** | Embedded in `AIAgent.messages[0]`. | `NativeAgentClient` has `with_system_prompt`, but it is dead code. In production, `system_prompt` is `None`. |
| **Cache Invalidation** | Runtime drift check (`_stored_prompt_matches_runtime`) and cross-process message count check. | `system_prompt::stored_prompt_matches_runtime` is implemented with unit tests, but unwired at runtime. |

---

## Proposal: Concrete Place for Per-Conversation Prompt Initialization

### 1. Concrete Location: Admitted Turn Execution Boundary

Prompt initialization must occur **after session resolution and after lease acquisition, but before `AgentClient::run_turn`**.

#### Why this location is necessary:
1. **Concurrency and Serialization**: The turn lease (`SessionTurnLeaseRegistry::acquire`) is already held. No two turns can concurrently evaluate, build, or write prompts for the same session.
2. **Resolved Context**: The internal durable session ID (`entry.session_id`) and routing key (`entry.session_key`) are fully resolved by `SessionStore`.
3. **Database Availability**: The session database (`turn_db: Option<Arc<SessionDb>>`) is resolved for the target profile and session.
4. **Backend Awareness**: The caller checks `manages = agent.manages_history()`. If `true` (the Python bridge `SubprocessAgentClient`), prompt assembly is skipped completely because the child process manages its own prompt. If `false` (native agent), Rust prompt initialization executes.

Both `Dispatcher::run_admitted_turn` (push path in `dispatch.rs`) and the admitted turn task in `message.rs` (HTTP path) share this exact boundary.

```
+-----------------------------------------------------------------------------+
| Turn Ingress (Push: dispatch.rs handle_turn / HTTP: message.rs post_message)|
+-----------------------------------------------------------------------------+
                                       |
                                       v
        Session Resolution (SessionStore::get_or_create_with_legacy)
                                       |
                                       v
               Turn Lease Acquisition (SessionTurnLeaseRegistry)
                                       |
                                       v
               +-----------------------------------------------+
               | PROPOSED: resolve_conversation_agent(...)     |
               |                                               |
               | 1. Check if agent.manages_history() == true.  |
               |    If so, return base agent unchanged.        |
               | 2. Query turn_db.get_session(session_id).     |
               | 3. If stored prompt exists and matches:       |
               |    -> RESUME: Reuse stored prompt verbatim.   |
               | 4. If missing, null, or runtime mismatch:     |
               |    -> RESET/NEW: Assemble prompt fresh via   |
               |       ResolvedPromptSections and persist to   |
               |       turn_db.update_system_prompt(...).      |
               | 5. Return native_agent.with_system_prompt(...) |
               +-----------------------------------------------+
                                       |
                                       v
              session_db::begin_turn(turn_db, manages, msg, source)
                                       |
                                       v
                  agent.run_turn(&msg, &history, tx).await
                                       |
                                       v
              session_db::end_turn(turn_db, manages, msg, reply)
                                       |
                                       v
                          Release Turn Lease & Deliver
```

### 2. Concrete Helper Interface: `resolve_conversation_agent`

Define a shared initialization helper (placed in a common module such as `session_prompt.rs` or directly in `system_prompt.rs`):

```rust
pub async fn resolve_conversation_agent(
    base_agent: &Arc<dyn AgentClient>,
    turn_db: Option<&SessionDb>,
    session_id: &str,
    platform: &str,
    profile_home: &std::path::Path,
    user_config: &serde_json::Value,
    configured_model: Option<&str>,
) -> Arc<dyn AgentClient>
```

#### Step-by-Step Logic within `resolve_conversation_agent`:

1. **Bypass for History-Managing Backends**:
   * If `base_agent.manages_history()` is true (e.g. `SubprocessAgentClient`), return `base_agent.clone()`.
   * If `base_agent` is not a native agent client (e.g. `CliAgentClient`), return `base_agent.clone()`.

2. **Downcasting or Specializing the Native Client**:
   * To allow customizing `NativeAgentClient` without reconstructing HTTP connection pools, add a trait method to `AgentClient`:
     ```rust
     fn with_conversation_prompt(&self, prompt: Arc<str>) -> Arc<dyn AgentClient> {
         Arc::new(self.clone()) // default fallback
     }
     ```
   * On `NativeAgentClient`, implement `with_conversation_prompt`:
     ```rust
     fn with_conversation_prompt(&self, prompt: Arc<str>) -> Arc<dyn AgentClient> {
         let mut client = self.clone();
         client.system_prompt = Some(prompt);
         Arc::new(client)
     }
     ```
   * This reuses `reqwest::Client`'s connection pool, header maps, and provider configurations while binding the immutable prompt.

3. **Resume Path (Verbatim Reuse from Database)**:
   * When `turn_db` is present:
     * Call `turn_db.get_session(session_id)`.
     * Inspect `row["_system_prompt_resolved"]`.
     * If present as a non-empty string `stored_prompt`:
       * Construct `runtime = PromptRuntime { model, provider, platform, cwd }`.
       * Evaluate `stored_prompt_matches_runtime(&stored_prompt, &runtime)`.
       * If compatible: **Reuse `stored_prompt` verbatim**.
       * Return `base_agent.with_conversation_prompt(Arc::from(stored_prompt))`.
       * Result: Zero disk I/O and identical bytes across turns, ensuring upstream LLM prefix cache hits.

4. **Reset / New Conversation Path (Assembly and Persistence)**:
   * If no stored prompt exists (new session or auto-reset) or `stored_prompt_matches_runtime` returned `false`:
     * Instantiate `sections = ResolvedPromptSections::default()`.
     * Assemble sections using existing ported helpers in `system_prompt.rs`:
       1. `sections.load_skills(...)`
       2. `sections.initialize_stable(...)` (reads `profile_home/SOUL.md`, stable guidance)
       3. `sections.load_runtime_guidance(...)` (provider identity, environment hints, coding context)
       4. `sections.load_context(...)` (reads `AGENTS.md`, `.cursorrules`, with threat scanner)
       5. `sections.set_memory_snapshot(...)` (reads `profile_home/memories/`)
       6. `sections.set_footer(...)` (session timestamps, metadata footer)
       7. `sections.append_profile_platform(...)` (profile hints and platform hints with overrides)
     * Call `let assembled = sections.assemble().joined();`.
     * Persist to SQLite: `turn_db.update_system_prompt(session_id, Some(&assembled))`.
     * Return `base_agent.with_conversation_prompt(Arc::from(assembled))`.

---

## Reset and Resume Behavior

### 1. Resume Behavior
* Trigger: User continues chatting in an existing channel or thread, or resumes via `/resume`.
* In `SessionStore::transition`:
  * `existing_reset_reason` evaluates to `None`.
  * The existing `session_id` is retained.
* In `resolve_conversation_agent`:
  * `turn_db.get_session(session_id)` loads the previously saved prompt from the `system_prompts` table via `_system_prompt_resolved`.
  * `stored_prompt_matches_runtime` verifies that model, provider, platform, and cwd are identical.
  * The stored prompt is adopted without reading `SOUL.md`, skills, or context files.
  * Prefix cache hits are maintained across turns.

### 2. Auto-Reset Behavior
* Trigger: Session exceeds configured freshness (`gateway_auto_continue_freshness`), daily reset boundary, or idle timeout (`existing_reset_reason` returns a reason).
* In `SessionStore::transition`:
  * `SessionEntry::new_candidate` generates a new, distinct `session_id` for the session key.
  * Context records `was_auto_reset: true`.
* In `resolve_conversation_agent`:
  * `turn_db.get_session(&new_session_id)` returns no existing prompt.
  * The helper recognizes a new session lifecycle, re-reads `SOUL.md`, skills, and memory, and builds a fresh prompt.
  * The newly assembled prompt is stored in `system_prompts` linked to `new_session_id`.

### 3. Explicit Reset Command (`/new` or `/reset`)
* Trigger: User sends `/new` or `/reset`.
* `SessionStore` drops or archives the current entry and allocates a fresh `session_id`.
* The prompt initialization helper builds a fresh prompt for the new session ID and persists it.

### 4. Runtime Identity Drift
* Trigger: Operator updates `config.yaml` to point to a different model or provider, or the working directory changes.
* In `resolve_conversation_agent`:
  * `turn_db.get_session(session_id)` returns the old prompt.
  * `stored_prompt_matches_runtime(&stored_prompt, &runtime)` returns `false` due to model or provider mismatch.
  * The helper discards the stale prompt, rebuilds with the new model guidance, and calls `turn_db.update_system_prompt` to overwrite the session prompt hash.

---

## Integration in Push and HTTP Paths

### 1. Push Path Integration (`dispatch.rs`)

Replace the static clone in [`dispatch.rs:L398`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L398):

```rust
// dispatch.rs - run_admitted_turn
let session_id = crate::session_db::message_session_id(&msg);
let profile_home = crate::config_file::hermes_home();
let platform_name = format!("{:?}", msg.platform).to_lowercase();

let agent = resolve_conversation_agent(
    &self.agent,
    turn_db.as_deref(),
    &session_id,
    &platform_name,
    &profile_home,
    &self.user_config,
    None,
).await;

let source = platform_name;
let history = crate::session_db::begin_turn(turn_db.as_deref(), manages, &msg, &source);

let msg_for_agent = msg.clone();
let agent_task = tokio::spawn(async move { agent.run_turn(&msg_for_agent, &history, tx).await });
```

### 2. HTTP Path Integration (`message.rs`)

Replace the static clone in [`message.rs:L196`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L196):

```rust
// message.rs - post_message admitted turn
let session_id = crate::session_db::message_session_id(&msg);
let profile_home = crate::config_file::hermes_home();

let agent = resolve_conversation_agent(
    &state.agent,
    turn_db.as_deref(),
    &session_id,
    "cli",
    &profile_home,
    &state.user_config,
    state.configured_model.as_deref(),
).await;

let history = crate::session_db::begin_turn(turn_db.as_deref(), manages, &msg, "cli");

let msg_for_agent = msg.clone();
let turn = tokio::spawn(async move { agent.run_turn(&msg_for_agent, &history, tx).await });
```

---

## Testing Strategy

To verify this implementation without regressions:

1. **Unit Tests for `resolve_conversation_agent`**:
   * *First Turn Build*: Pass an in-memory `SessionDb` and verify that a new session causes `get_session` to return `None`, triggers assembly, and persists a prompt into `system_prompts`.
   * *Second Turn Resume*: Call a second time with the same `session_id`. Verify that `update_system_prompt` is not called and the exact prompt bytes are returned without filesystem reads.
   * *Runtime Drift Invalidation*: Mutate the model name in `PromptRuntime`. Verify that `stored_prompt_matches_runtime` detects the change, triggers a rebuild, and updates the database row.
   * *Subprocess Bypass*: Pass a mock `AgentClient` with `manages_history() == true`. Verify that the helper returns the agent unmodified and executes zero database operations.

2. **Integration Tests for Ingress Paths**:
   * *Push Ingress (`dispatch.rs`)*: Send two successive messages on a test channel. Verify that turn 1 creates the prompt and turn 2 executes `run_turn` with the identical system message at `history[0]`.
   * *HTTP Ingress (`message.rs`)*: Send two successive requests to `/message` with the same `channel_id`. Confirm the returned response incorporates the initialized system prompt.
   * *Reset Transition*: Trigger an auto-reset by manipulating timestamps in `SessionStore`. Verify that the subsequent turn receives a new `session_id` and an updated system prompt.
