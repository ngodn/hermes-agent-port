# External-Memory Turn Lifecycle Architecture and Seam Map

## 1. Executive Summary and Seam Scope

This analysis designs the next Rust port checkpoint: wiring the real Python external-memory turn lifecycle into native Rust conversations through the persistent extension host introduced in commit `a97232664e`.

Commit `a97232664e` landed the conversation-scoped extension host (`hermes_cli/rust_extension_host.py` and `rust/crates/hermes-gateway/src/extension_host.rs`). That host successfully establishes a persistent Python subprocess per cached conversation, initializes the configured memory provider and plugins with profile isolation and scoped environment protection, captures static prompt contributions on fresh conversation initialization, normalizes schemas, and routes tool executions asynchronously.

However, turn-level lifecycle hooks remain entirely unwired:
- No turn start notification or prefetch query is dispatched before model execution.
- No ephemeral memory context is injected into the user turn for LLM reasoning.
- No post-turn sync or background prefetch warming occurs upon turn completion.
- No session end extraction runs when conversations end or are torn down.
- Interrupted and empty responses are not filtered to prevent memory pollution.

This checkpoint defines the smallest complete JSONL protocol extension and Rust ownership modifications required to make external memory operational per turn while strictly adhering to the following hard system invariants:
1. Verbatim prompt reuse: Stored conversation system prompts must remain byte-immutable and never trigger network calls, provider queries, or subprocess child calls during prompt restoration.
2. Message alternation: Injected memory context must not insert synthetic intermediary roles or break user-assistant-user alternation in Chat Completions payloads.
3. Lock boundaries: Subprocess IPC and memory provider network calls must never be executed while holding SQLite database connection mutexes or conversation cache map locks.
4. Non-fatal fault isolation: External memory provider failures, slow responses, or subprocess crashes must never block or abort user chat turns.

---

## 2. Production Call Sites and Contracts in Python

A rigorous trace of the Python reference codebase reveals the exact contracts and call sites governing external memory.

### 2.1 Prefetch and Turn Start

Production Call Site: `agent/turn_context.py:1568-1598`
```python
# Notify memory providers of the new turn (BEFORE prefetch_all).
if agent._memory_manager:
    try:
        _turn_msg = original_user_message if isinstance(original_user_message, str) else ""
        agent._memory_manager.on_turn_start(agent._user_turn_count, _turn_msg)
    except Exception:
        pass

ext_prefetch_cache = ""
if agent._memory_manager:
    try:
        _query = original_user_message if isinstance(original_user_message, str) else ""
        if not is_trivial_prompt(_query):
            ext_prefetch_cache = agent._memory_manager.prefetch_all(_query) or ""
    except Exception:
        pass
    if ext_prefetch_cache:
        try:
            _recall_indicator = agent._memory_manager.describe_recall()
            if _recall_indicator:
                agent._emit_status(_recall_indicator)
        except Exception:
            pass
```

Contract details:
1. `on_turn_start(turn_number, message, **kwargs)`: Notifies memory providers of the turn index and incoming user query. Providers use this for turn counting and state maintenance (`agent/memory_provider.py:254-261`).
2. `is_trivial_prompt(text)` (`agent/memory_provider.py:90-108`): Evaluates whether the prompt warrants memory recall. Matches empty input, whitespace, slash commands (e.g. `/reset`), or trivial conversational words and acknowledgements (e.g. "ok", "yes", "hi", "thanks", "cool", "done", "lgtm"). If trivial, prefetch is skipped entirely, saving a network round trip and preventing context contamination.
3. `prefetch_all(query, session_id="")` (`agent/memory_manager.py:594-670`):
   - Strips skill scaffolding (`agent/memory_manager.py:577-593`) via `extract_user_instruction_from_skill_message`.
   - External providers run inside a dedicated daemon thread `memory-prefetch-{provider.name}`.
   - If an existing prefetch thread for the provider is still running from a prior query, the new turn skips prefetch immediately (`:646-648`).
   - Thread join is bounded by `_external_prefetch_timeout` (default 8.0s). If the thread exceeds 8.0s, the manager skips prefetch and returns an empty string (`:656-662`).
   - Exceptions inside providers are swallowed and logged as non-fatal, returning empty strings (`:610-613`).
4. `describe_recall()` (`agent/memory_manager.py:671-702`):
   - Queries `provider.recall_status()`.
   - Returns a formatted string like `🧠 Honcho - recalled 3 memories` or `👁️ Hindsight - recalled relevant memory`.
   - Emitted to the user via status events so the operator knows memory was recalled regardless of whether the model references it in its response.

### 2.2 Context Injection and the `api_content` Sidecar

Production Call Sites: `agent/turn_context.py:131-164`, `agent/turn_context.py:1600-1655`, and `agent/memory_manager.py:416-430`

Contract details:
1. Sanitization: `sanitize_context(raw_context)` strips any pre-existing `<memory-context>` tags or system notes (`agent/memory_manager.py:243-249`).
2. Fencing: `build_memory_context_block(raw_context)` formats the clean context:
   ```text
   <memory-context>
   [System note: The following is recalled memory context, NOT new user input. Treat as authoritative reference data - this is the agent's persistent memory and should inform all responses.]

   {clean_context}
   </memory-context>
   ```
3. Composition: `compose_user_api_content(content, ext_prefetch_cache, plugin_user_context)`:
   - For string user content, appends `\n\n` followed by the fenced block.
   - If content is not a string (e.g. multimodal list of parts), returns `None` (plain content preserved as-is).
4. Sidecar Invariant: Clean user input remains in `msg["content"]`. Injected text is attached to `msg["api_content"]`.
5. Crash Resilience: The user row is persisted to SQLite with its final `api_content` before the first model API call (`turn_context.py:1656-1679`).
6. Wire Projection: `substitute_api_content` (`turn_context.py:166-187` and Rust `chat_message_projection.rs:151-164`) swaps `api_content` into `content` solely for the outgoing Chat Completions payload. This guarantees prompt cache prefix byte stability across turns.

### 2.3 Completed-Turn Sync, Write, and Queue Prefetch

Production Call Sites: `agent/turn_finalizer.py:802-808`, `run_agent.py:4935-5000`, and `agent/memory_manager.py:744-801`

Contract details:
1. Entry point: `_sync_external_memory_for_turn`:
   - Inputs: `original_user_message`, `final_response`, `interrupted`, `messages`.
   - Flattens user and assistant responses to strings.
   - Guard: If `interrupted` is True, returns immediately.
   - Guard: If `final_response` or `original_user_message` is empty/missing, returns immediately.
2. Background Execution: `sync_all` and `queue_prefetch_all` are submitted to a dedicated background executor (`_sync_executor` in `MemoryManager`).
   - A single-worker executor is used. This serializes writes strictly FIFO: turn N write finishes before turn N+1 write starts (`memory_manager.py:455-457`).
   - Because work runs in the background, `sync_all` returns almost immediately to the main conversation loop. Slow or wedged memory provider network calls never block user response delivery or leave the agent stuck in a "running" state.
3. Queue Prefetch: If the user query was not trivial, `queue_prefetch_all` queues background retrieval for the next turn.
4. Tool Write Mirroring (`agent/tool_executor.py:2184-2195` and `agent/memory_manager.py:1268-1324`):
   - When the built-in `memory` tool runs mutating actions (`add`, `replace`, `remove`), `notify_memory_tool_write` mirrors the modification to external providers via `on_memory_write(action, target, content, metadata)`.

### 2.4 Interrupted or Empty Response Behavior

Production Call Sites: `run_agent.py:4969-4979` and `agent/turn_finalizer.py:784-808`

Contract details:
1. Interrupted Turns: Turns aborted due to cancellation signals, timeouts, model errors, or user steer interrupts skip `_sync_external_memory_for_turn` completely (`run_agent.py:4969-4970`). Partial assistant outputs or aborted tool chains are not durable conversational truth. Syncing them would corrupt future memory recall with uncommitted assistant actions.
2. Queue Prefetch Skipping: Interrupted turns also skip `queue_prefetch_all`. The user next turn is likely a retry of the same intent, and warming cache against an interrupted turn would query on stale context.
3. Empty Responses: Responses that are empty or contain only whitespace are rejected by `if not (user_text and response_text): return`.

### 2.5 Session End and Shutdown

Production Call Sites: `run_agent.py:4882-4909` (`shutdown_memory_provider`), `run_agent.py:4910-4934` (`commit_memory_session`), and `agent/memory_manager.py:1339-1418` (`shutdown_all`)

Contract details:
1. Turn loop separation: `on_session_end` and `shutdown_all` are never called at the end of a single turn (`turn_finalizer.py:832-838`). Calling them per turn would kill providers before the next turn.
2. Real Session End: Fired at actual session boundaries (CLI exit, session reset `/reset`, session `/new`, gateway session expiration/eviction).
3. `on_session_end(messages)`: Passes full conversation history to providers for end-of-session fact extraction, summarization, and graph consolidation.
4. `shutdown_all()`:
   - Drains the background sync executor with a 5.0s timeout (`_SYNC_DRAIN_TIMEOUT_S`).
   - Cancels unstarted tasks and logs abandoned tasks.
   - Calls `provider.shutdown()` on all providers in reverse registration order.

### 2.6 Queue Draining and Durability

Production Call Site: `agent/memory_manager.py:878-900` (`flush_pending`)
- `flush_pending(timeout)` submits a sentinel task (`lambda: None`) to the single-worker executor and awaits its completion.
- Since the executor is single-worker, the sentinel completes only after all previously enqueued writes and prefetches have executed.
- Used at session boundaries and in tests requiring deterministic assertions on provider storage.

### 2.7 Exception Handling and Non-Fatal Semantics

Production Call Sites: `agent/turn_context.py:1573,1586,1597`, `agent/turn_finalizer.py:829`, and `run_agent.py:4998`
- External memory operations are strictly best-effort.
- Prefetch failure logs debug/warning and yields empty context; the model turn proceeds normally.
- Recall status failure logs and yields empty string; response generation proceeds normally.
- Turn sync failure logs warning in background; user response is delivered regardless.
- Subprocess transport failures must fail open for user chat rather than terminating the gateway.

### 2.8 Timeouts

Production Constants:
- `_EXTERNAL_PREFETCH_TIMEOUT_S = 8.0` (`agent/memory_manager.py:81`): Maximum duration for a synchronous prefetch attempt.
- `_SYNC_DRAIN_TIMEOUT_S = 5.0` (`agent/memory_manager.py:80`): Maximum duration allowed for background write queue draining on shutdown.

### 2.9 Identity Arguments

Production Call Sites: `agent/agent_init.py:1971-2014` and `agent/memory_provider.py:135-157`
- Fixed arguments: `session_id`, `hermes_home`, `platform`, `agent_context`, `agent_identity`, `agent_workspace`.
- Platform peer identity arguments: `session_title`, `user_id`, `user_id_alt`, `user_name`, `chat_id`, `chat_name`, `chat_type`, `thread_id`, `gateway_session_key`.

---

## 3. Live vs Absent Seam Map in Rust (Commit a97232664e)

The table below contrasts what commit `a97232664e` already implements in Rust against what is currently missing for external memory turn lifecycles.

| Lifecycle Capability | Status in Rust `a97232664e` | Exact Location | Gap to Bridge |
| :--- | :--- | :--- | :--- |
| Subprocess Spawn & Handshake | Live | `extension_host.rs:167-231` | None. Spawns persistent Python child with env allowlist and profile secrets. |
| Memory Provider Resolution | Live | `rust_extension_host.py:152-196` | None. Reads `config["memory"]["provider"]` and checks availability. |
| Toolset Exposure Gating | Live | `rust_extension_host.py:247-261` | None. Memory tools gated by `memory_provider_tools_enabled`. |
| Static System Prompt Assembly | Live | `rust_extension_host.py:271-286`, `main.rs:658-688` | None. Captures `memory_prompt` during fresh prompt construction. |
| Verbatim Stored Prompt Reuse | Live | `main.rs:641-690`, `conversation_prompt.rs` | None. Resumed sessions reuse stored bytes and skip extension snapshot. |
| Tool Execution Routing | Live | `extension_host.rs:241-248`, `rust_extension_host.py:288-316` | None. Dispatches tool calls to `MemoryManager.handle_tool_call`. |
| Subprocess Shutdown Drain | Live | `extension_host.rs:336-347`, `rust_extension_host.py:318-330` | Calls `MemoryManager.shutdown_all()`. Missing `on_session_end(messages)`. |
| Turn Start (`on_turn_start`) | Absent | None | Rust never sends turn counter or user query to Python before turn. |
| Dynamic Turn Prefetch | Absent | None | Rust never calls `prefetch_all`; user query context is never recalled. |
| Recall Status Indicator | Absent | None | Rust never queries `describe_recall()` or emits memory recall status events. |
| Context Injection (`api_content`) | Absent | `native_agent.rs:476-510` | User content passed clean to model without `<memory-context>` block or `api_content` sidecar. |
| Completed-Turn Sync (`sync_turn`) | Absent | None | Rust never sends completed user/assistant pair to Python after turn. |
| Queue Prefetch Warming | Absent | None | Rust never notifies Python to warm prefetch for subsequent turns. |
| Turn Interruption Filter | Absent | None | Rust does not track or communicate turn interruption to memory host. |
| Session End Extraction | Absent | None | Rust never invokes `on_session_end(messages)` with transcript at session close. |
| Queue Drain Barrier | Absent | None | Rust has no RPC to trigger `flush_pending` for testing or boundaries. |

---

## 4. Architectural Invariants to Preserve

Implementing the turn lifecycle must preserve three foundational architecture rules established in previous checkpoints.

### 4.1 Immutable Per-Conversation System Prompt
The system prompt assembled during conversation initialization represents the frozen configuration and baseline identity of the agent.
- `system_prompt_block()` from the memory provider provides static instructions (e.g. description of memory capabilities). This was solved in commit `a97232664e` and is stored in `sessions.system_prompt`.
- Dynamic recalled context from `prefetch_all()` must NEVER be appended to or interpolated into the system prompt.
- Mutating the system prompt turn-by-turn would invalidate provider-side prompt caches (Anthropic, OpenAI, DeepSeek, Gemini) and violate the exact-byte restoration invariant verified by the 12-case conversation restore oracle.

### 4.2 Strict Message Alternation
Modern LLM chat completion APIs enforce strict message sequencing:
- Top-level roles must alternate: optional `system`, then `user`, `assistant`, `user`, `assistant`.
- Recalled memory context must NEVER be injected as an artificial `system` message between user and assistant turns, nor as a separate preceding `user` message.
- Python injects memory context directly into the current turn user message API copy (`api_content`), demarcated by `<memory-context>` tags.
- This preserves the single `user` role for the turn, maintains alternation, and allows `chat_message_projection::substitute_api_content` to project the exact wire bytes cleanly.

### 4.3 Lock Boundaries and Zero-Network Restoration
- Zero-network prompt restoration: Resuming a conversation from SQLite must remain a pure, local read. No subprocess calls, provider initializations, or prefetch queries may run during prompt restoration.
- Lock boundary: Subprocess JSONL calls (like model HTTP calls) cross OS process and potential network boundaries. They must never be executed while holding:
  1. `SessionDb.conn` (the SQLite connection mutex).
  2. `ConversationAgent.clients` (the active client cache mutex).
  3. Channel receiver or sender locks.

---

## 5. Smallest Complete JSONL Protocol Design

To avoid chatty IPC with 6 separate round-trips per turn, we coalesce the turn prologue and epilogue into cohesive, atomic JSONL RPC methods.

The protocol retains the JSON-RPC-like envelope established in `extension_host.rs`:
```json
{"id": <u64>, "method": "<str>", "params": { ... }}
{"id": <u64>, "ok": true, "result": { ... }}
{"id": <u64>, "ok": false, "error": "<str>"}
```

### 5.1 Protocol Methods

#### 5.1.1 Method: `turn_start`
Combines turn start notification, trivial prompt evaluation, prefetch retrieval, and recall status calculation in a single non-blocking IPC round-trip.

Request:
```json
{
  "id": 2,
  "method": "turn_start",
  "params": {
    "user_message": "Where did we leave off on the database migration?",
    "turn_number": 3
  }
}
```

Python Extension Host Behavior:
1. Validates that `_memory_manager` is present. If absent, returns `{"context": null, "recall_indicator": null}`.
2. Calls `_memory_manager.on_turn_start(turn_number, user_message)`.
3. Evaluates `is_trivial_prompt(user_message)`. If True, returns `{"context": null, "recall_indicator": null}`.
4. Executes `context = _memory_manager.prefetch_all(user_message, session_id=self._session_info["session_id"])`.
5. If `context` is non-empty, calls `indicator = _memory_manager.describe_recall()`.
6. Returns the result.

Success Response:
```json
{
  "id": 2,
  "ok": true,
  "result": {
    "context": "Previous migration paused at step 4: foreign keys pending.",
    "recall_indicator": "🧠 Honcho - recalled 1 memory"
  }
}
```

#### 5.1.2 Method: `turn_complete`
Notifies the memory manager of a completed turn, delegating post-turn sync and background prefetch queuing.

Request:
```json
{
  "id": 3,
  "method": "turn_complete",
  "params": {
    "user_message": "Where did we leave off on the database migration?",
    "assistant_response": "We paused at step 4 pending foreign keys.",
    "interrupted": false,
    "messages": [
      {"role": "user", "content": "Where did we leave off on the database migration?"},
      {"role": "assistant", "content": "We paused at step 4 pending foreign keys."}
    ]
  }
}
```

Python Extension Host Behavior:
1. If `_memory_manager` is absent, returns `{"synced": false}`.
2. If `interrupted` is True, returns `{"synced": false}` without touching providers.
3. If `user_message` or `assistant_response` is empty, returns `{"synced": false}`.
4. Calls `_memory_manager.sync_all(user_message, assistant_response, session_id=self._session_info["session_id"], messages=messages)`.
   Because Python `MemoryManager.sync_all` submits to its internal single-worker `_sync_executor`, this method returns immediately without waiting for provider network I/O.
5. If `not is_trivial_prompt(user_message)`, calls `_memory_manager.queue_prefetch_all(user_message, session_id=self._session_info["session_id"])`.
6. Returns `{"synced": true}`.

Success Response:
```json
{
  "id": 3,
  "ok": true,
  "result": {
    "synced": true
  }
}
```

#### 5.1.3 Method: `session_end`
Delivers full conversation history for end-of-session summarization and fact extraction.

Request:
```json
{
  "id": 4,
  "method": "session_end",
  "params": {
    "messages": [
      {"role": "user", "content": "Hello"},
      {"role": "assistant", "content": "Hi there"}
    ]
  }
}
```

Python Extension Host Behavior:
1. Calls `_memory_manager.on_session_end(messages)`.
2. Returns `{"completed": true}`.

Success Response:
```json
{
  "id": 4,
  "ok": true,
  "result": {
    "completed": true
  }
}
```

#### 5.1.4 Method: `flush_pending`
Barrier synchronization for tests and graceful teardown.

Request:
```json
{
  "id": 5,
  "method": "flush_pending",
  "params": {
    "timeout_seconds": 5.0
  }
}
```

Python Extension Host Behavior:
1. Calls `success = _memory_manager.flush_pending(timeout=params.get("timeout_seconds", 5.0))`.
2. Returns `{"flushed": success}`.

Success Response:
```json
{
  "id": 5,
  "ok": true,
  "result": {
    "flushed": true
  }
}
```

#### 5.1.5 Updated Method: `shutdown`
Existing `shutdown` RPC is enhanced to optionally accept a final transcript so that `on_session_end` can be executed automatically if not called prior to process termination.

Request:
```json
{
  "id": 0,
  "method": "shutdown",
  "params": {
    "messages": []
  }
}
```

---

## 6. Rust Ownership and Architecture Changes

To implement this design, Rust components need specific ownership and signature adjustments.

### 6.1 `rust/crates/hermes-gateway/src/extension_host.rs`

1. Expose typed client methods on `Client`:
   ```rust
   pub struct TurnStartResult {
       pub context: Option<String>,
       pub recall_indicator: Option<String>,
   }

   impl Client {
       pub async fn turn_start(
           &self,
           turn_number: usize,
           user_message: &str,
       ) -> Result<TurnStartResult> {
           let value = self
               .request(
                   "turn_start",
                   json!({
                       "turn_number": turn_number,
                       "user_message": user_message,
                   }),
                   Duration::from_secs(8),
               )
               .await?;
           serde_json::from_value(value)
               .map_err(|e| Error::Other(format!("extension host turn_start decode: {e}")))
       }

       pub async fn turn_complete(
           &self,
           user_message: &str,
           assistant_response: &str,
           interrupted: bool,
           messages: &[serde_json::Value],
       ) -> Result<bool> {
           let value = self
               .request(
                   "turn_complete",
                   json!({
                       "user_message": user_message,
                       "assistant_response": assistant_response,
                       "interrupted": interrupted,
                       "messages": messages,
                   }),
                   Duration::from_secs(5),
               )
               .await?;
           Ok(value.get("synced").and_then(serde_json::Value::as_bool).unwrap_or(false))
       }

       pub async fn session_end(&self, messages: &[serde_json::Value]) -> Result<()> {
           self.request(
               "session_end",
               json!({ "messages": messages }),
               Duration::from_secs(15),
           )
           .await?;
           Ok(())
       }

       pub async fn flush_pending(&self, timeout: Duration) -> Result<bool> {
           let value = self
               .request(
                   "flush_pending",
                   json!({ "timeout_seconds": timeout.as_secs_f64() }),
                   timeout + Duration::from_secs(1),
               )
               .await?;
           Ok(value.get("flushed").and_then(serde_json::Value::as_bool).unwrap_or(false))
       }
   }
   ```

2. Format helper for context injection:
   ```rust
   pub fn build_memory_context_block(raw_context: &str) -> String {
       let trimmed = raw_context.trim();
       if trimmed.is_empty() {
           return String::new();
       }
       format!(
           "<memory-context>\n[System note: The following is recalled memory context, NOT new user input. Treat as authoritative reference data - this is the agent's persistent memory and should inform all responses.]\n\n{trimmed}\n</memory-context>"
       )
   }
   ```

### 6.2 `rust/crates/hermes-gateway/src/native_agent.rs`

1. The `NativeAgentClient` already holds `_extension_host: Option<crate::extension_host::Client>`. We rename it to `extension_host: Option<crate::extension_host::Client>`.
2. Add a turn counter `turn_count: std::sync::atomic::AtomicUsize` on `NativeAgentClient`.
3. In `AgentClient::run_turn`:
   - Before building the request body:
     - If `msg.text` is non-empty and `self.extension_host` is `Some(ref host)`:
       - Increment `self.turn_count.fetch_add(1, Ordering::Relaxed)`.
       - Call `host.turn_start(turn_number, &msg.text).await`.
       - If `turn_start` returns `recall_indicator`, emit `StreamEvent::GatewayNotice { text: indicator }` (or commentary).
       - If `turn_start` returns `Some(recalled_context)`, format it via `build_memory_context_block(&recalled_context)` and compose:
         `let api_content = format!("{}\n\n{}", msg.text, context_block);`
         Assign this to a modified user message in the outgoing payload.
   - After model step or tool loop completion:
     - If response completed normally and `!final_reply.is_empty()`:
       - Call `host.turn_complete(&msg.text, &final_reply, false, &messages).await`.
     - If turn was interrupted or errored:
       - Call `host.turn_complete(&msg.text, "", true, &[]).await` or skip entirely.

### 6.3 `rust/crates/hermes-gateway/src/session_db.rs`

1. User message persistence:
   - In `session_db::begin_turn`:
     Currently `begin_turn` records the message immediately before prefetch.
     However, to match Python crash resilience and prompt cache stability:
     The user turn must store `api_content` when memory context is present.
   - We extend `AppendOptions` to support `api_content: Option<&'a str>`.
   - In `messages` table DDL: ensure `api_content TEXT` is present (either via `CREATE TABLE IF NOT EXISTS` column list or via schema migration).
   - In `load_history`: load `api_content` so resumed sessions replay the exact wire bytes sent in turn N.

### 6.4 `rust/crates/hermes-gateway/src/dispatch.rs` and `message.rs`

1. In `Dispatcher::run_admitted_turn`:
   - Status events (`GatewayNotice`) generated during prefetch flow smoothly through `tx` into `rx`.
   - If the agent task panics or errors, mark the turn interrupted.
   - If the turn completes cleanly, record the assistant reply in `session_db::end_turn`.
2. Cancellation safety:
   - If the client drops the HTTP connection or platform stream, `agent_task` continues running to completion or until reaching an explicit cancellation point, ensuring that memory writes and session state remain coherent.

---

## 7. Exact Lifecycle Ordering

The diagram and trace below detail the exact chronological ordering across components.

```
Incoming User Message
         │
         ▼
[1] Gateway Ingress (Dispatcher / HTTP route)
    - Acquire turn lease
    - Ensure session in SQLite
    - Load prior history (zero network, pure DB read)
         │
         ▼
[2] Memory Prologue (via Extension Host)
    - Call extension_host.turn_start(turn_count, user_query)
    - Host evaluates is_trivial_prompt():
        ├─ If trivial -> returns {context: null, recall_indicator: null}
        └─ If non-trivial -> calls on_turn_start(), prefetch_all(), describe_recall()
    - Bounded by 8.0s timeout; failure is non-fatal (proceeds with empty context)
         │
         ▼
[3] Recall Notice & Injection
    - If recall_indicator present -> emit GatewayNotice to events channel
    - If context present -> build <memory-context> block
    - Attach api_content to user message
         │
         ▼
[4] Transcript Persistence (Crash Resilience)
    - Persist user message to SQLite messages table with api_content
    - SQLite connection lock released immediately
         │
         ▼
[5] Model Execution & Tool Loop
    - Wire projection applies substitute_api_content
    - System prompt remains byte-identical to frozen stored prompt
    - Model step / streaming / tool loop runs
    - If memory tool called -> routes via call_tool to Python provider
         │
         ▼
[6] Turn Finalization
    ├─ If INTERRUPTED or ERROR:
    │   - Skip turn_complete sync (no durable sync, no prefetch warming)
    │   - Deliver error or partial notice
    └─ If SUCCESSFUL and NON-EMPTY:
        - Persist assistant response to SQLite (end_turn)
        - Deliver reply to platform
        - Call extension_host.turn_complete(user_text, reply_text, interrupted=false, messages)
        - Host submits to background single-worker executor:
            ├─ provider.sync_turn(...)
            └─ provider.queue_prefetch(...) (if non-trivial)
        - RPC returns immediately (does not block for provider network I/O)
         │
         ▼
[7] Session Teardown (on Eviction, Reset, or Close)
    - Call extension_host.session_end(messages) -> provider.on_session_end()
    - Call extension_host.flush_pending(5.0s) -> drains pending writes
    - Call extension_host.shutdown() -> provider.shutdown(), process tree killed
```

---

## 8. Verification and Test Strategy

### 8.1 Focused Unit Tests

1. Context Injection Formatting:
   - `test_build_memory_context_block`: Asserts clean formatting, presence of the system note, absence of em dash characters, and proper handling of trailing whitespace.
   - `test_compose_user_api_content_string`: Asserts that plain string user content receives the `<memory-context>` block separated by `\n\n`.
   - `test_compose_user_api_content_multimodal`: Asserts that non-string content (e.g. JSON array of image parts) returns `None` and is not corrupted by string context formatting.

2. Prompt Immutability and Message Alternation:
   - `test_system_prompt_unmodified_by_prefetch`: Asserts that `client.system_prompt` bytes remain identical before and after `turn_start`.
   - `test_wire_messages_preserve_alternation`: Verifies that a conversation history with injected `api_content` projects to `[system, user, assistant, user]` without consecutive same-role messages.

3. Interrupted and Empty Response Filters:
   - `test_interrupted_turn_skips_sync`: Verifies that when `interrupted = true`, `turn_complete` returns `synced = false` and invokes neither `sync_turn` nor `queue_prefetch`.
   - `test_empty_response_skips_sync`: Verifies that whitespace-only or empty assistant output skips `turn_complete`.

4. Fail-Open Fault Tolerance:
   - `test_turn_start_timeout_fails_open`: Mocks a hanging extension host response exceeding 8.0s; asserts that `run_turn` proceeds cleanly without memory context.
   - `test_turn_start_error_fails_open`: Mocks a Python exception returned as `{"ok": false, "error": "database locked"}`; asserts that `run_turn` proceeds cleanly.

### 8.2 Real Subprocess Integration Tests

These tests use the real Python interpreter in `.venv` and a custom test memory provider defined in a temporary plugin directory.

1. Subprocess Turn Prefetch and Recall Notice:
   - Provider implements `prefetch(query) -> format!("Recalled: {query}")` and `recall_status() -> RecallStatus("fixture", 1, "🧠")`.
   - Rust native agent runs a turn with `"Where is my laptop?"`.
   - Verifies that `StreamEvent::GatewayNotice` is received with `"🧠 fixture - recalled 1 memory"`.
   - Verifies that the model step mock receives the user message containing `<memory-context>\n...\nRecalled: Where is my laptop?\n</memory-context>`.

2. Subprocess Trivial Prompt Skipping:
   - User prompt is `"ok"` or `"thanks"`.
   - Verifies that `turn_start` returns `context: null` and the model receives raw `"ok"` with no `<memory-context>` tag.

3. Subprocess Post-Turn Sync and Flush:
   - Turn completes with user message `"Remember project Apollo"` and assistant response `"Stored project Apollo"`.
   - Rust calls `client.turn_complete(...)`.
   - Rust calls `client.flush_pending(Duration::from_secs(5))`.
   - Asserts that the fixture provider wrote `"Remember project Apollo"` and `"Stored project Apollo"` to its marker file on disk.

4. Subprocess Session End and Cleanup:
   - Rust calls `client.session_end(&transcript)`.
   - Asserts that the fixture provider `on_session_end` executed and recorded the transcript length.
   - Dropping the client triggers clean child process termination without orphan processes.

---

## 9. Explicit Deferrals (What Remains Deferred)

In keeping with the gateway-first strangler architecture, the following areas must remain deferred to subsequent checkpoints rather than guessed or prematurely implemented:

1. Mid-Turn Subprocess Respawn:
   If the extension host subprocess crashes or is killed mid-turn, it remains fail-closed for the lifetime of that cached client. Automatic in-flight process respawn with reconstructive state recovery is deferred.
2. Gateway Identity Field Expansion on `Message`:
   `hermes_core::Message` currently lacks `user_id_alt`, `user_name`, and `chat_name`. Extending message structures across all messaging adapters (Telegram, Slack, Discord) is deferred.
3. Native Built-In Memory Tool Mirroring (`notify_memory_tool_write`):
   The built-in `memory` tool (managing `MEMORY.md` and user profile files) is not yet ported to native Rust. When the model invokes a memory tool in native mode today, it is an external provider tool routed directly to the extension host. Built-in tool write mirroring will be wired when the native built-in memory tool is ported.
4. Pre-Compression Checkpointing (`on_pre_compress`):
   Context compression reconstruction and pre-compression evidence handoffs remain deferred to the native compression milestone.
5. Subagent Delegation Observation (`on_delegation`):
   Multi-agent delegation and subagent completion hooks remain deferred until native subagent execution is implemented.
6. Windows Job Object for Abrupt Teardown:
   Abrupt Windows descendant process tree cleanup without graceful stdin close remains deferred. Unix process groups continue to protect POSIX platforms.
