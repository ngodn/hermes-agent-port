# External-Memory Turn Lifecycle Architecture and Correctness Review

## 1. Executive Summary

This review analyzes the uncommitted external-memory turn-lifecycle implementation in `/home/eins0fx/development/hermes-agent-port`. The changes span:
- [`hermes_cli/rust_extension_host.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py)
- [`rust/crates/hermes-gateway/src/extension_host.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs)
- [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs)
- [`rust/crates/hermes-gateway/src/session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs)
- [`rust/crates/hermes-gateway/src/conversation_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs)
- [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)

The implementation is evaluated against the Python reference paths:
- [`agent/turn_context.py`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py)
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`agent/memory_manager.py`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py)
- [`agent/memory_provider.py`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_provider.py)
- [`agent/codex_responses_adapter.py`](file:///home/eins0fx/development/hermes-agent-port/agent/codex_responses_adapter.py)

The checkpoint successfully establishes the core turn-level IPC mechanics:
1. It queries prefetch memory context before the model call via `turn_start`.
2. It composes `<memory-context>` tags using Python's canonical [`compose_user_api_content`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py#L131-L164), preventing formatting drift.
3. It emits `StreamEvent::GatewayNotice` with `recall_indicator` so the operator sees recalled facts.
4. It checkpoints `api_content` into SQLite before LLM execution, preserving crash resilience.
5. It triggers post-turn background persistence via `turn_complete` and exposes `flush_pending` for synchronization.

However, several correctness defects and architectural discrepancies exist. Most significantly:
- Tool execution transcripts are completely stripped from the `turn_complete` sync payload, hiding tool actions from memory providers.
- Timeouts during `turn_start` kill the persistent Python subprocess, permanently disabling all extension tools and memory for the remainder of the session.
- Whitespace-only assistant and user messages bypass empty filters and pollute memory backends.
- Loading `COALESCE(api_content, content)` in [`SessionDb::load_history`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1556-L1580) collapses the clean content / wire sidecar abstraction at the database interface.

---

## 2. Checkpoint Boundary vs Explicit Future Work

To prevent false alarms, this review explicitly separates checkpoint scope from deferred milestones.

### 2.1 Explicit Deferred Work (Not Defects in this Checkpoint)
1. **Bounded Cache Eviction and Subprocess Lifecycle**: Managing active conversation cache caps, LRU / TTL idle eviction, and automatic process group teardown on session reset or expiry is explicitly assigned to the next checkpoint (tracked in `conversation-eviction-map-claude.md` and `conversation-eviction-claude.txt`). The current implementation provides `session_end` and `shutdown` RPC primitives, but wiring automatic teardown hooks belongs to the eviction checkpoint.
2. **Mid-Turn Subprocess Respawn**: When an extension host subprocess crashes or is terminated mid-turn, it remains fail-closed for the lifetime of that cached client. Automatic in-flight child process respawn with full state reconstruction is an explicit future deferral (Section 9.1 of `external-memory-lifecycle-map-agy.md`).
3. **Native Built-In Memory Tool Mirroring**: Porting the built-in `memory` tool (`MEMORY.md`) and wiring [`MemoryManager::notify_memory_tool_write`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L1268-L1324) to external providers is deferred until the native tool is ported.
4. **Context Compression Checkpoints (`on_pre_compress`)**: Pre-compression fact extraction and checkpointing hooks are deferred to the native compression milestone.
5. **Platform Identity Field Expansion**: Passing extended platform metadata (`user_id_alt`, `user_name`, `chat_name`) requires modifying `hermes_core::Message` across all gateway adapters, which remains deferred.

### 2.2 In-Scope Review Dimensions
The following sections evaluate the code that was implemented: correctness defects, byte stability, persistence ordering and migration, interrupted/empty turns, multimodal handling, timeouts, lock boundaries, and test coverage.

---

## 3. Severity-Ranked Defect Catalog

### Critical Severity

#### DEFECT-1: Truncation of `messages` in `turn_complete` Drops Tool Loop History
- **Location**: [`rust/crates/hermes-gateway/src/native_agent.rs:598-605`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L598-L605)
- **Reference**: [`agent/turn_finalizer.py:803-808`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L803-L808), [`run_agent.py:4941, 4983`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L4941-L4983), [`agent/memory_provider.py:226-228`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_provider.py#L226-L228)
- **Description**:
  When a turn finishes in [`NativeAgentClient::run_native_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L524-L614), it calls `host.turn_complete(&clean_content, &response, &messages)` with:
  ```rust
  let messages = [
      json!({"role":"user", "content": clean_content}),
      json!({"role":"assistant", "content": response}),
  ];
  ```
  In Python, the `messages` argument to [`_sync_external_memory_for_turn`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L4935-L5000) is the entire conversation transcript as of the completed turn, specifically including all assistant tool call blocks (`role: "assistant", tool_calls: [...]`) and tool responses (`role: "tool", content: [...]`) produced during the tool loop.
  The [`MemoryProvider.sync_turn`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_provider.py#L213-L230) specification states:
  > "`messages` is the OpenAI-style conversation message list as of the completed turn, including any assistant tool calls and tool results."
  Providers such as OpenViking ([`plugins/memory/openviking/__init__.py:4607-4611`](file:///home/eins0fx/development/hermes-agent-port/plugins/memory/openviking/__init__.py#L4607-L4611)) rely on `_extract_current_turn_messages(messages, ...)` to record which tools were executed and what data they returned.
- **Impact**: Any tool execution performed by the agent (shell executions, web searches, file edits) is stripped from the memory sync payload. External memory providers only see user input and the final assistant message, resulting in loss of intermediate tool findings and false representations of conversational actions.
- **Remediation**: [`run_tool_loop_with_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L513-L580) should return or expose the turn's execution messages (including tool calls and tool responses), or [`NativeAgentClient::run_native_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L524-L614) must forward the complete message sequence to `turn_complete`.

---

#### DEFECT-2: Subprocess Termination on Prefetch Timeout Violates Non-Fatal Fault Isolation
- **Location**: [`rust/crates/hermes-gateway/src/extension_host.rs:500-506`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L500-L506), [`rust/crates/hermes-gateway/src/extension_host.rs:394-398`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L394-L398)
- **Reference**: [`agent/memory_manager.py:81, 654-662`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L81-L662), [`agent/turn_context.py:1586-1587`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py#L1586-L1587)
- **Description**:
  In [`extension_host.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L29), `TURN_START_TIMEOUT` is set to 10 seconds. In [`exchange`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L414-L507), if an RPC operation exceeds `timeout`:
  ```rust
  Err(_) => {
      kill_process_tree(&mut process.child).await;
      Err(ExchangeError::Transport("extension host request timed out".into()))
  }
  ```
  The worker loop catches `transport_failed`, sets `healthy = false`, and exits the worker loop.
  In Python, external memory operations are strictly best-effort. Inside [`MemoryManager`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L433), prefetch runs in a daemon thread bounded by `_EXTERNAL_PREFETCH_TIMEOUT_S = 8.0` seconds. However, if a provider's `on_turn_start` hook blocks, if multiple external providers run sequentially (8.0s + 8.0s = 16.0s), or if `describe_recall` takes time, total Python execution can exceed 10.0s.
- **Impact**: When Rust times out at 10.0s, it sends `SIGKILL` to the entire Python process group and shuts down the worker channel. For the current turn, [`NativeAgentClient::run_native_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L577-L579) logs a warning and fails open. But for all subsequent turns in this session, the extension host is dead (`"extension host response channel closed"`). This permanently disables memory and breaks all registered extension tools for the rest of the conversation.
- **Remediation**:
  1. In [`rust_extension_host.py:turn_start`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L318-L361), bound the overall execution in Python with an internal timeout (e.g. 7.5s) to guarantee a timely response before Rust's transport timer triggers.
  2. Increase Rust's `TURN_START_TIMEOUT` to 15s to allow sufficient headroom for multi-provider prefetch draining.

---

### High Severity

#### DEFECT-3: Whitespace-Only Responses Bypass Empty-Response Filters
- **Location**: [`rust/crates/hermes-gateway/src/native_agent.rs:596`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L596), [`hermes_cli/rust_extension_host.py:377-381`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L377-L381)
- **Reference**: [`run_agent.py:4971, 4978`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L4971-L4978), [`agent/turn_finalizer.py:574, 816`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L574-L816)
- **Description**:
  In [`native_agent.rs:596`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L596):
  ```rust
  if !response.is_empty() {
      // triggers host.turn_complete(...)
  }
  ```
  This only checks `len() > 0`. If the model outputs whitespace (newlines, spaces, or reasoning tokens scrubbed into empty text), `response` is non-empty.
  In [`rust_extension_host.py:turn_complete`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L370-L381):
  ```python
  user_text = _summarize_user_message_for_log(original, sep="\n")
  response_text = _summarize_user_message_for_log(final_response, sep="\n")
  if not (user_text and response_text):
      return
  ```
  [`_summarize_user_message_for_log`](file:///home/eins0fx/development/hermes-agent-port/agent/codex_responses_adapter.py#L221-L267) returns string inputs as-is without calling `.strip()`. In Python, non-empty whitespace is truthy (`bool("  \n  ") == True`).
  Therefore, `if not (user_text and response_text)` evaluates to false.
- **Impact**: Blank and whitespace-only assistant responses are submitted to `manager.sync_all` and `manager.queue_prefetch_all`, polluting external memory backends with empty conversational turns.
- **Remediation**:
  1. In Rust, check `if !response.trim().is_empty()`.
  2. In Python, check `if not (user_text.strip() and response_text.strip()): return`.

---

#### DEFECT-4: `load_history` Inversion Erases Storage-Layer Boundary
- **Location**: [`rust/crates/hermes-gateway/src/session_db.rs:1564, 1570`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1564-L1570)
- **Reference**: [`agent/turn_context.py:140-147, 166-187`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py#L140-L187), [`hermes_state.py:14146-14153`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L14146-L14153), [`rust/crates/hermes-gateway/src/chat_message_projection.rs:151-166`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/chat_message_projection.rs#L151-L166)
- **Description**:
  In [`SessionDb::load_history`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1556-L1580), the query was modified to:
  ```sql
  SELECT role, COALESCE(NULLIF(api_content, ''), content) FROM messages
  ```
  The resulting text is mapped directly into [`HistoryMessage.content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L73).
  In Python, stored messages retain the clean user input in `content`, and the injected text in `api_content`. When messages are loaded from SQLite, both fields are returned distinctly. Only [`substitute_api_content`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py#L166-L187) swaps `api_content` into `content` specifically on the model-wire copy right before the API call.
- **Impact**:
  1. Any component that calls `load_history` (e.g. conversation summaries, transcript export, or compaction) receives the `<memory-context>` block embedded inside the user's message.
  2. [`chat_message_projection::substitute_api_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/chat_message_projection.rs#L151-L166) (which is executed in `NativeAgentClient::apply_provider_extras`) becomes a dead code path because `api_content` was already collapsed into `content` during the database read.
- **Remediation**: Add `pub api_content: Option<String>` to [`HistoryMessage`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L71-L75). Select `role, content, api_content` in `load_history`. Pass `api_content` through to `build_messages_with_content` so that [`chat_message_projection::substitute_api_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/chat_message_projection.rs#L151-L166) performs the substitution only on wire copies.

---

### Medium Severity

#### DEFECT-5: `Client::turn_complete` Omits `interrupted: bool` Parameter
- **Location**: [`rust/crates/hermes-gateway/src/extension_host.rs:273-292`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L273-L292)
- **Reference**: `rust/analysis/external-memory-lifecycle-map-agy.md:414`
- **Description**:
  The public typed Rust method on [`Client`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L112) is:
  ```rust
  pub async fn turn_complete(&self, user_message: &Value, final_response: &str, messages: &[Value]) -> Result<()>
  ```
  It hardcodes `"interrupted": false` in the JSON-RPC payload.
  In [`extension_host.rs:931-946`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L931-L946), the author was unable to use `turn_complete` to test interruption and had to call the private `client.request(...)` method directly.
- **Impact**: Any upstream caller or test attempting to signal an interrupted turn via the typed client cannot do so.
- **Remediation**: Add `interrupted: bool` to [`Client::turn_complete`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L273-L292).

---

#### DEFECT-6: `session_end` RPC Timeout is Too Short (5s vs 15s)
- **Location**: [`rust/crates/hermes-gateway/src/extension_host.rs:298`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L298)
- **Reference**: `rust/analysis/external-memory-lifecycle-map-agy.md:436`, [`agent/memory_provider.py:263-271`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_provider.py#L263-L271)
- **Description**:
  [`Client::session_end`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L293-L302) uses `TURN_COMPLETE_TIMEOUT` (5.0s). The design document specified 15.0s. Memory providers like Honcho or Hindsight execute end-of-session fact extraction, clustering, or graph consolidation across the entire transcript inside `on_session_end(messages)`.
- **Impact**: Calling `session_end` on conversations with long transcripts will trigger a 5s timeout, causing `exchange` to terminate the Python child process mid-extraction.
- **Remediation**: Define `const SESSION_END_TIMEOUT: Duration = Duration::from_secs(15);` and use it in `Client::session_end`.

---

#### DEFECT-7: Whitespace-Only User Prompts Synced to Memory
- **Location**: [`hermes_cli/rust_extension_host.py:377-384`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L377-L384)
- **Reference**: [`run_agent.py:4978`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L4978), [`agent/memory_manager.py:773`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L773)
- **Description**:
  In `rust_extension_host.py:turn_complete`, when `original` is `"   "`, `_summarize_user_message_for_log` returns `"   "`. Because it is truthy, `user_text` is `"   "`. In [`MemoryManager::sync_all`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L744-L801), `clean_user_content = self._strip_skill_scaffolding(user_content)` returns `"   "` unchanged.
- **Impact**: `sync_turn` is invoked with a whitespace user string. While `queue_prefetch_all` is skipped because `is_trivial_prompt("   ")` is True, `sync_all` still persists the blank user input.
- **Remediation**: In `turn_complete`, check `if not (user_text.strip() and response_text.strip()): return`.

---

#### DEFECT-8: Fallback Turn Numbering Mismatch on Truncated History
- **Location**: [`rust/crates/hermes-gateway/src/native_agent.rs:537-542`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L537-L542)
- **Reference**: [`rust/crates/hermes-gateway/src/session_db.rs:137, 199`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L137-L199)
- **Description**:
  When computing `turn_number`:
  ```rust
  let fallback_turn = history.iter().filter(|item| item.role == "user").count() + 1;
  let turn_number = context
      .database
      .and_then(|database| database.user_turn_count(&session_id).ok())
      .unwrap_or(fallback_turn);
  ```
  In [`session_db::begin_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L179-L208), `history` is loaded with `limit = HISTORY_LIMIT` (which is 40). If `context.database` is `None` or `user_turn_count` encounters an error on long sessions, `fallback_turn` will count at most ~20 user messages, under-reporting the turn number to `on_turn_start`.
- **Impact**: When running in database-less configurations or on transient DB errors, turn counters drift and can trigger premature or delayed periodic provider maintenance.
- **Remediation**: Maintain an atomic turn counter on `NativeAgentClient` (or `ConversationAgent`) that survives history windowing.

---

### Low Severity / Hygiene

#### DEFECT-9: Redundant Imports on the Hot Per-Turn Path
- **Location**: [`hermes_cli/rust_extension_host.py:337, 356, 374, 375`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L337-L375)
- **Description**:
  `from agent.memory_provider import is_trivial_prompt`, `from agent.turn_context import compose_user_api_content`, and `from agent.codex_responses_adapter import _summarize_user_message_for_log` are imported inside method bodies on every turn.
- **Remediation**: Import these symbols at module level or during `initialize()`.

#### DEFECT-10: Row ID Discard in `begin_turn` Forces Heuristic SQL Subquery
- **Location**: [`rust/crates/hermes-gateway/src/session_db.rs:206`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L206), [`rust/crates/hermes-gateway/src/session_db.rs:1460-1470`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1460-L1470)
- **Description**:
  [`SessionDb::append_message`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1442-L1446) returns the inserted SQLite `i64` row ID. However, [`begin_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L179-L208) discards it (`let _ = ...`). As a consequence, [`set_latest_user_api_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1454-L1471) must execute a subquery matching `session_id`, `role = 'user'`, and `content = ? ORDER BY id DESC LIMIT 1`.
- **Remediation**: Return `(Vec<HistoryMessage>, i64)` from `begin_turn` and update by row ID directly (`UPDATE messages SET api_content = ?1 WHERE id = ?2`).

---

## 4. Deep Architectural Traces & Invariants

### 4.1 Prompt-Cache Byte Stability
- **Inbound Construction**: Recalled memory is formatted exclusively via [`compose_user_api_content`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py#L131-L164) in Python. The resulting `<memory-context>` block, separator newlines (`\n\n`), and system note characters match Python byte-for-byte.
- **Wire Projection**: In Turn 1, the model receives `content = "<query>\n\n<memory-context>..."`. In Turn 2, [`SessionDb::load_history`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1556-L1580) re-reads Turn 1's wire content via `COALESCE(NULLIF(api_content, ''), content)`. This ensures that what Turn 1 placed on the wire is replayed identically in Turn 2, preserving prompt-cache prefixes.
- **Alternation**: Injected memory context remains inside the `user` role's content block. No synthetic `system` messages are inserted between user turns, preserving strict `[system, user, assistant, user]` alternation.

### 4.2 `api_content` Persistence Ordering & Migration Safety
- **Persistence Timing**: In [`NativeAgentClient::run_native_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L524-L614), `database.set_latest_user_api_content(...)` runs and commits before `run_model_turn` initiates the HTTP request to the LLM. If the process crashes mid-generation, SQLite already contains the user turn with its `api_content`.
- **Schema Migration**: [`SessionDb::ensure_message_schema`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1144-L1159) uses `TransactionBehavior::Immediate` to inspect `PRAGMA table_info(messages)` and executes `ALTER TABLE messages ADD COLUMN api_content TEXT` if absent. This transaction is short, executes with SQLite WAL concurrency protection, and executes zero external network or IPC calls under the lock.

### 4.3 Multimodal Flattening
- **Inbound `turn_start`**: If `user_message` is a structured list (`Value::Array`), `isinstance(user_message, str)` is False. `query` becomes `""`, `is_trivial_prompt("")` returns True, and `compose_user_api_content` returns `None`. This matches Python's contract where multimodal inputs bypass memory recall.
- **Outbound `turn_complete`**: When `user_message` is multimodal, `_summarize_user_message_for_log(original, sep="\n")` formats it to `"[N image(s)] <text>"`. This string is passed to `manager.sync_all` and tested in [`extension_host.rs:953-956`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L953-L956).

### 4.4 Lock Boundaries
- Neither `SessionDb.conn` (SQLite connection lock) nor `ConversationAgent.clients` (client map mutex) is held across subprocess IPC calls.
- In [`ConversationAgent::run_turn_with_context`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L78-L112), the map lock is dropped before `client.run_turn_with_context` is awaited.
- In [`NativeAgentClient::run_native_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L524-L614), `database.user_turn_count` and `database.set_latest_user_api_content` acquire and release the SQLite mutex independently before and after the async IPC `host.turn_start(...)` call.

---

## 5. Test Coverage Audit

### Existing Tests in Diff
1. [`extension_host.rs:903-973`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L903-L973): Verifies `turn_start` injection formatting, trivial prompt skipping (`"thanks"`), multimodal flattening on `turn_complete`, pending queue flush, and `session_end`.
2. [`session_db.rs:3360-3428`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L3360-L3428): Verifies `api_content` persistence on matching user row, replay via `load_history`, and schema migration on legacy DB without `api_content` column.
3. [`main.rs:1330-1570`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1330-L1570): Verifies end-to-end integration with real Python `.venv`, plugin memory provider registration, memory prefetch during turn, and post-turn sync marker files.

### Critical Missing Tests
1. **Multi-Turn Byte Stability Oracle**: The tests only execute a single turn. There is no test that runs Turn 1 (injecting memory), runs Turn 2, and verifies that the captured Turn 2 HTTP request payload contains Turn 1's injected `api_content` byte-for-byte.
2. **Empty / Whitespace-Only Skip Test**: No test checks that whitespace-only responses (e.g. `"  \n  "`) skip `turn_complete`.
3. **Turn Error / Interrupt Isolation in `run_native_turn`**: No test verifies that if the model HTTP request fails with an error or is aborted, `turn_complete` is skipped.
4. **Subprocess Timeout Fail-Open Test**: No test verifies behavior when `turn_start` times out or fails with an error.

---

## 6. Defect Matrix & Remediation Summary

| Defect ID | Severity | File & Location | Core Issue | Recommended Fix |
| :--- | :--- | :--- | :--- | :--- |
| **DEFECT-1** | Critical | [`native_agent.rs:598`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L598) | Tool calls and responses omitted from `messages` in `turn_complete` | Return full message sequence from `run_tool_loop_with_content` to `turn_complete` |
| **DEFECT-2** | Critical | [`extension_host.rs:500`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L500) | `turn_start` timeout (10s) kills child process permanently | Bound prefetch internally in Python to ~7.5s; increase Rust transport timeout to 15s |
| **DEFECT-3** | High | [`native_agent.rs:596`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L596), [`rust_extension_host.py:377`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L377) | Whitespace-only assistant responses synced to memory | Add `.trim().is_empty()` check in Rust and `.strip()` checks in Python |
| **DEFECT-4** | High | [`session_db.rs:1564`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1564) | `load_history` merges `api_content` directly into `HistoryMessage.content` | Keep clean `content` and `api_content` distinct on `HistoryMessage` |
| **DEFECT-5** | Medium | [`extension_host.rs:285`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L285) | `Client::turn_complete` hardcodes `interrupted: false` | Expose `interrupted: bool` on `Client::turn_complete` |
| **DEFECT-6** | Medium | [`extension_host.rs:298`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L298) | `session_end` uses 5s timeout instead of 15s | Define and use 15s timeout for `session_end` RPC |
| **DEFECT-7** | Medium | [`rust_extension_host.py:377`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L377) | Whitespace-only user messages synced to memory | Filter whitespace user queries with `.strip()` in `turn_complete` |
| **DEFECT-8** | Medium | [`native_agent.rs:537`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L537) | Fallback turn counter limited by 40-message history window | Maintain persistent turn counter on client struct |
| **DEFECT-9** | Low | [`rust_extension_host.py:337`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L337) | Redundant imports on hot per-turn path | Move imports to module top level |
| **DEFECT-10** | Low | [`session_db.rs:206`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L206) | `begin_turn` discards inserted row ID | Return row ID from `begin_turn` to update row directly |
