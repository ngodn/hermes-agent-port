# Implementation Map: Native Tool Prefix Freeze & Frozen Plugin Section Restoration

## Executive Summary

This document maps the next Rust port checkpoint for the Hermes gateway (`rust/crates/hermes-gateway`): **making `conversation_prompt::Resolution.restore_frozen_sections` real for native routed conversations**.

Upstream LLM provider prefix caching (e.g. Anthropic prompt caching, OpenAI prompt prefix caching) requires that both the **system prompt text** and the **`tools[]` parameter array** remain byte-stable across conversation turns. When an agent instance is evicted from memory (or reconstructed across process restarts), the gateway must restore the exact prompt text and the exact `tools[]` order that the session already established. Furthermore, volatile plugin prompt sections (`<!-- hermes-plugin-sections:start -->...`) are rendered exclusively on session creation; on continuation, their bytes must be extracted directly from the persisted prompt **without re-executing plugin callbacks**.

### Baseline Invariants
1. **Tool Prefix Cache Invariant**: The order of `tools[]` sent to the provider model must match the session's prior turns. A fresh agent reconstruction must fold its live tools onto the saved name order:
   - Preserving the exact slot order of previously sent tools.
   - Retaining still-registered tools whose live availability probe flapped (e.g., transient daemon failure).
   - Dropping deregistered tools that were removed from the catalog.
   - Appending genuinely new tools strictly to the tail of the array.
2. **Callback-Free Plugin Restoration**: Resuming a session with a cached prompt must never evaluate plugin code or execute dynamic callbacks. Restoring plugin sections recovers the frozen bytes directly from the persisted prompt.
3. **Per-Conversation Lifetime Matching**: The resolved system prompt bytes, the frozen plugin sections, and the frozen `tools[]` array share the exact same lifetime, bound immutably to the conversation client in [`ConversationAgent`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L28-L33).
4. **Pre-Provider I/O Persistence**: Freshly established tool orders and refreshed prompts must be persisted to SQLite before initiating provider model I/O.
5. **SQLite, Lock, and I/O Boundaries**:
   - SQLite queries occur strictly outside model I/O and outside prompt assembly.
   - The [`ConversationAgent`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L28-L33) client map lock is dropped before factory execution and before model I/O.
   - Profile and session isolation are maintained per `(home, session_id)`.
   - Single-flight client initialization via [`tokio::sync::OnceCell`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L25) remains retryable on failure.

---

## 1. Python Reference Architecture & Source Trace

### 1.1 Fresh Session Path: Tool Order & Prompt Persistence
- **Prompt Assembly & Persistence Boundary**: [`agent/conversation_loop.py:1182-1228`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L1182-L1228)
  When a session is newly initialized (or when recovering from a broken/stale stored prompt):
  ```python
  # 1. Build fresh prompt from scratch
  agent._cached_system_prompt = agent._build_system_prompt(system_message)
  ...
  # 2. Fire new-session-only lifecycle hooks
  _invoke_hook("on_session_start", session_id=agent.session_id, ...)
  ...
  # 3. Persist prompt and tool names atomically before turn I/O
  if agent._session_db:
      agent._session_db.update_system_prompt(agent.session_id, agent._cached_system_prompt)
      from tools.mcp_tool import persist_agent_tool_names
      persist_agent_tool_names(agent)
  ```
- **Tool Order Snapshot Pin**: [`tools/mcp_tool.py:8912-8925`](file:///home/eins0fx/development/hermes-agent-port/tools/mcp_tool.py#L8912-L8925)
  Extracts the exact sequence of tool names exposed on the agent:
  ```python
  def persist_agent_tool_names(agent) -> None:
      db = getattr(agent, "_session_db", None)
      session_id = getattr(agent, "session_id", None)
      if not db or not session_id:
          return
      try:
          db.update_session_tool_names(
              session_id,
              [t["function"]["name"] for t in (getattr(agent, "tools", None) or [])],
          )
      except Exception:
          logger.debug("tool_names persist skipped", exc_info=True)
  ```
- **SQLite Storage**: [`hermes_state.py:9635-9653`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L9635-L9653) & [`hermes_state_common.py:477`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L477)
  The `sessions` table schema includes `tool_names TEXT`:
  ```python
  def update_session_tool_names(self, session_id: str, tool_names: Optional[List[str]]) -> None:
      payload = json.dumps(list(tool_names)) if tool_names is not None else None
      def _do(conn):
          conn.execute(
              "UPDATE sessions SET tool_names = ? WHERE id = ?",
              (payload, session_id),
          )
      self._execute_write(_do)
  ```

### 1.2 Resumed Session Path: Prefix Freeze & Plugin Restoration
- **Continuation Branch**: [`agent/conversation_loop.py:1122-1156`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L1122-L1156)
  When continuing an existing session whose stored prompt matches runtime:
  ```python
  # 1. Reuse verbatim stored prompt bytes for prefix cache hit
  agent._cached_system_prompt = stored_prompt

  # 2. Pin tools[] back to the order this session already sent (tools freeze)
  try:
      saved_tools = session_row.get("tool_names") if session_row else None
      if saved_tools:
          from tools.mcp_tool import restore_agent_tool_prefix
          restore_agent_tool_prefix(agent, json.loads(saved_tools))
  except Exception:
      logger.debug("tool prefix restore skipped", exc_info=True)

  # 3. Recover frozen plugin section bytes from stored prompt (no callbacks!)
  from agent.system_prompt import restore_plugin_prompt_sections
  restore_plugin_prompt_sections(agent, stored_prompt)

  # 4. Reconstruct static prefix for early Anthropic cache breakpoint
  from agent.system_prompt import reconstruct_static_prefix
  reconstruct_static_prefix(agent, system_message=system_message)
  return
  ```

### 1.3 Tool Prefix Restoration Algorithm (`restore_agent_tool_prefix`)
- **Source**: [`tools/mcp_tool.py:8927-8999`](file:///home/eins0fx/development/hermes-agent-port/tools/mcp_tool.py#L8927-L8999)
  When an agent is reconstructed for an existing session, live availability probes (`check_fn`) run anew. If a tool fails its check, it would normally disappear, which would mutate the prefix. The restoration algorithm preserves the prefix using four exact rules:
  1. **Slot Preservation & Schema Refresh**: A tool present in both `saved_names` and `fresh_defs` preserves its original slot in `saved_names`, taking the fresh schema definition.
  2. **Flapped Probe Retention**: A tool present in `saved_names` but missing from `fresh_defs` is checked against the global `registry`. If it is still registered (its live `check_fn` momentarily failed or timed out), it is carried forward from the registry's registered schema.
  3. **Deregistered Tool Dropping**: A tool present in `saved_names` that is absent from both `fresh_defs` and `registry` (e.g. an MCP server or plugin was genuinely uninstalled) is dropped.
  4. **Tail Appending for New Tools**: A tool present in `fresh_defs` that was not in `saved_names` is appended at the tail of the array.
  5. **Persistence on Mutation**: If `[t["function"]["name"] for t in merged] != list(saved_names)`, `persist_agent_tool_names(agent)` updates SQLite.

### 1.4 Plugin Section Recovery Algorithm (`restore_plugin_prompt_sections`)
- **Source**: [`agent/system_prompt.py:61-65, 191-276`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L61-L65)
  - Plugin prompt sections are placed in Tier 3 (volatile tail, after memory, before footer).
  - Framing syntax:
    ```
    <!-- hermes-plugin-sections:start -->
    ## Plugin Context: <id>
    <!-- hermes-plugin-section-chars:<N> -->

    <content>
    <!-- hermes-plugin-sections:end -->

    Conversation started: <ISO-8601>
    ```
  - Recovery scans backwards for `<!-- hermes-plugin-sections:start -->` and forward for `<!-- hermes-plugin-sections:end -->`.
  - Verifies that `\n\nConversation started:` immediately follows the end marker.
  - Parses each frame using regex `_PLUGIN_SECTION_FRAME_RE`.
  - Verifies character lengths (up to 4,000 chars per section, 8,000 aggregate).
  - Re-formats all restored sections and checks `format_system_prompt_sections(restored) == framed`. If not an exact match, discards (rejects partial matches or prompt injections).
  - **No plugin code is executed**. Resumed processes retain the initial render bytes verbatim.

---

## 2. Current Rust Gateway Audit & Abstraction Gaps

### 2.1 Component Audit Matrix

| Component & File | Current Implementation | Gap vs. Python Reference |
| :--- | :--- | :--- |
| `conversation_prompt.rs` ([`43-53`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L43-L53), [`577-587`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L577-L587)) | [`Resolution`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L44-L53) contains `pub restore_frozen_sections: bool`. Set to `true` on reuse, `false` on fresh build. | Flags are emitted but completely ignored by caller ([`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L488-L495)). [`PromptStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L55-L61) has no tool persistence method. |
| `session_db.rs` ([`1100-1120`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1100-L1120), [`1271-1284`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1271-L1284)) | `sessions` table lacks `tool_names TEXT` in [`ensure_recovery_schema`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1077-L1121). | Column missing from migration loop; [`SessionDb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs) has no `update_session_tool_names` method. |
| `native_tools.rs` ([`28-40`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L28-L40), [`716-735`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L716-L735)) | Defines [`Tool`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L37-L40) trait and [`CurrentTimeTool`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L717-L735). | No tool prefix merge algorithm (`restore_tool_prefix`), no catalog for registered tools, no flap retention. |
| `plugin_prompt.rs` ([`242-344`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L242-L344)) | Fully ports [`restore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L298-L344), [`format`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L281-L287), [`Snapshot`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L242-L279), and [`Registry`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L96-L183). | Not wired into conversation client construction; restored sections are never attached to the conversation client. |
| `native_agent.rs` ([`194-213`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213)) | [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213) stores `system_prompt: Option<Arc<str>>` and `tools: Vec<Arc<dyn Tool>>`. | No field or method for frozen plugin sections (`plugin_sections`); tools are attached unconditionally via `with_tools`. |
| `conversation_agent.rs` ([`15-32`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L15-L32), [`80-109`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L80-L109)) | Async factory with per-session [`tokio::sync::OnceCell`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L25) cache keyed by `(home, session_id)`. | Clean structure, but factory does not participate in tool prefix restoration or plugin section ownership. |
| `main.rs` ([`385-397`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L385-L397), [`453-495`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L453-L495)) | `build_conversation_client` calls `restore_or_build`, but ignores `restore_frozen_sections`. Tools hardcoded to [`CurrentTimeTool`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L717-L735). | Ignores saved `tool_names` on reuse, never persists tool names on fresh build, never restores plugin sections. |

### 2.2 Detailed Inspection of Native Abstraction Gaps

#### Gap A: SQLite Schema & Storage Layer
In Python, `sessions.tool_names` stores a JSON-encoded array of strings (e.g. `r#"["current_time"]"#`).
In Rust:
- [`session_db::ensure_recovery_schema`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1077-L1121) runs `ALTER TABLE sessions ADD COLUMN ...` for recovery columns (`parent_session_id`, `system_prompt`, `model_config`, etc.), but `("tool_names", "TEXT")` was omitted.
- [`session_row_value`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L251-L280) dynamically parses any text column as `Value::String`, so adding `tool_names` to the table will immediately make `session_row["tool_names"]` available in the `serde_json::Value` map returned by [`session_row(session_id)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L56).
- [`SessionDb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs) lacks the write helper `update_session_tool_names(&self, session_id: &str, tool_names: Option<&[String]>)`.
- [`conversation_prompt::PromptStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L55-L61) lacks `persist_tool_names(&self, id: &str, tool_names: &[String])`.

#### Gap B: Native Tool Catalog vs. Dynamic Tool Infrastructure
In Python:
- Tools are discovered dynamically via `tools.registry.registry`, which maintains an in-memory index of all known tool schemas and their `check_fn` availability probes.
- Dynamic MCP servers connect over stdio/SSE, and tools are unmounted if the subprocess dies.
In Rust:
- [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213) uses static `Arc<dyn Tool>` instances.
- The Rust gateway does not yet have a dynamic MCP client manager or dynamic tool plugin system.
- **Critical Insight**: While dynamic MCP management belongs to a future checkpoint, the **deterministic tool prefix merge logic** (`merge_preserving_prefix`) and **probe-flap retention** can and must be implemented now. It accepts a catalog of known native tools (`registered_tools`) and folds the saved prefix against fresh tools, providing identical semantics for static native tools today and dynamic MCP tools tomorrow.

#### Gap C: Plugin Section Restoration vs. Plugin Execution Runtime
In Python:
- Plugins register hooks (`on_session_start`) and section callbacks (`render_system_prompt_sections`).
- Resumed turns call `restore_plugin_prompt_sections`, which recovers the rendered strings without invoking plugin code.
In Rust:
- [`plugin_prompt.rs:298-344`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L298-L344) already contains the full `restore(&str) -> Vec<Section>` parser!
- However, [`Initializer::build_fresh`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L260-L378) does not assemble plugin sections, and `build_conversation_client` in [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L488-L495) never invokes `plugin_prompt::restore` when `resolution.restore_frozen_sections == true`.
- Furthermore, [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213) has no container to hold restored plugin sections, leaving them unowned.

---

## 3. Target Production Design

### 3.1 Structural Sequence Diagram

```
                      Turn Ingress (Push / HTTP)
                                │
                                ▼ [Acquire Turn Lease]
           ConversationAgent::run_turn_with_context(context, msg, history, events)
                                │
                                ▼ [Session Key: (home, session_id)]
             ClientCell::get_or_try_init (Defensive Single-Flight)
                                │
                                ▼
         ┌─────────────────────────────────────────────────────────────┐
         │ main.rs: build_conversation_client (Factory Implementation) │
         └─────────────────────────────────────────────────────────────┘
                                │
       1. Probe Live Tools ─────┤ fresh_tools = [CurrentTimeTool]
                                │ registered_tools = [CurrentTimeTool]
                                │
       2. Prompt Decision ──────┤ conversation_prompt::restore_or_build(store, session_id, ...)
                                │   │
                                │   ├── [SQLite Read] store.session_row(session_id)
                                │   │     Extracts: stored_prompt, saved_tool_names
                                │   │
                                │   ├── IF Stored Prompt Matches Runtime & Not Stale:
                                │   │     └── Return Resolution { prompt, reused: true, restore_frozen_sections: true, ... }
                                │   │
                                │   └── ELSE (New Session or Stale):
                                │         ├── build_fresh().await (Zero SQLite I/O)
                                │         ├── store.persist_prompt(session_id, &prompt) [SQLite Write]
                                │         └── store.persist_tool_names(session_id, &fresh_tool_names) [SQLite Write]
                                │
       3. Reused Branch Handling:
          IF resolution.restore_frozen_sections == true:
            ├── a. Restore Plugin Sections:
            │        frozen_sections = plugin_prompt::restore(&resolution.prompt)
            │        (ZERO callbacks executed; recovered directly from prompt bytes)
            │
            ├── b. Restore Tool Prefix:
            │        (merged_tools, changed) = native_tools::restore_tool_prefix(
            │            &saved_tool_names,
            │            fresh_tools,
            │            &registered_tools,
            │        )
            │        (Retains flapped tools from registered_tools, drops deregistered, appends new)
            │
            └── c. Persist Mutated Tool Order (if changed):
                     store.persist_tool_names(session_id, &merged_names) [SQLite Write]
                                │
       4. Client Assembly ──────┤ client = NativeAgentClient::new(...)
                                │   .with_system_prompt(resolution.prompt)
                                │   .with_tools(effective_tools)
                                │   .with_plugin_sections(frozen_sections)
                                │
                                ▼ [Cache Arc<NativeAgentClient> in OnceCell]
                                │
                                ▼ [Release Map Lock, Run Turn]
           client.run_turn(msg, history, events).await (Provider Model Wire I/O)
```

### 3.2 What Can Be Implemented NOW vs. What Must Wait

#### Implementable NOW (Smallest Production Design)
1. **SQLite Storage & Migration**:
   - Add `"tool_names"` column declaration in [`session_db::ensure_recovery_schema`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1077-L1121).
   - Add `SessionDb::update_session_tool_names` to serialize and persist JSON array strings.
   - Implement `persist_tool_names` on `PromptStore for SessionDb`.
2. **`PromptStore` & `Resolution` Enhancements**:
   - Add default `persist_tool_names(&self, id: &str, tool_names: &[String]) -> anyhow::Result<()>` to [`PromptStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L55-L61).
   - Extract `saved_tool_names: Option<Vec<String>>` from `session_row` inside [`restore_or_build`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L510-L621), returning it directly on [`Resolution`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L44-L53) to avoid redundant SQLite queries.
   - Persist `tool_names` alongside `prompt` on fresh builds before provider I/O.
3. **Deterministic Tool Prefix Restoration ([`native_tools.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs))**:
   - Implement `restore_tool_prefix(saved_names, fresh_tools, registered_tools) -> (Vec<Arc<dyn Tool>>, bool)` adhering exactly to Python's 4 merge rules:
     - Retains saved prefix slot order.
     - Retains flapped tools present in `registered_tools`.
     - Drops deregistered tools absent from `registered_tools`.
     - Appends genuinely new tools to the tail.
4. **Frozen Plugin Section Restoration**:
   - Call [`plugin_prompt::restore(&resolution.prompt)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L298-L344) on prompt reuse.
   - Attach the recovered `Vec<Section>` to [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213).
5. **Per-Conversation Owner ([`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213))**:
   - Add `plugin_sections: Option<Arc<[crate::plugin_prompt::Section]>>` to [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213).
   - Add builder `with_plugin_sections(mut self, sections: Vec<Section>) -> Self`.
   - Add getter `plugin_sections(&self) -> Option<&[Section]>`.
   - The lifetime of `tools` and `plugin_sections` directly matches `system_prompt` on [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213), managed by [`ConversationAgent`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L28-L33)'s `OnceCell`.

#### Must WAIT for Future Infrastructure
1. **Dynamic Tool Managers & MCP Subprocess Bridge**:
   - Spawning and monitoring stdio/SSE MCP server subprocesses.
   - Dynamic tool discovery via JSON-RPC `tools/list`.
2. **Dynamic Live Availability Probes (`check_fn`)**:
   - Periodic or turn-boundary polling of external daemon health (Docker socket, STT server, OAuth refresh).
3. **Native Plugin Loader**:
   - Loading external Python or shared-library plugins dynamically into the gateway.
   - Firing dynamic plugin hooks (`on_session_start`).
4. **In-Process Prompt Compression Rebuilder**:
   - Using the stored [`Snapshot`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L242-L279) during conversation context window truncation.

---

## 4. Line-Specific Implementation Plan

### Step 1: SQLite Schema & Migration in `session_db.rs`
**File**: [`rust/crates/hermes-gateway/src/session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs)

#### 1.1 Column Migration in `ensure_recovery_schema` (lines 1099-1115)
Add `("tool_names", "TEXT")` to the recovery column list:
```rust
        for (name, declaration) in [
            ("parent_session_id", "TEXT"),
            ("system_prompt", "TEXT"),
            ("system_prompt_hash", "TEXT"),
            ("ended_at", "REAL"),
            ("end_reason", "TEXT"),
            ("expiry_finalized", "INTEGER DEFAULT 0"),
            ("model_config", "TEXT"),
            ("model", "TEXT"),
            ("cwd", "TEXT"),
            ("git_repo_root", "TEXT"),
            ("git_branch", "TEXT"),
            ("tool_names", "TEXT"),
        ] {
            if !columns.iter().any(|column| column == name) {
                tx.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {name} {declaration}"),
                    [],
                )?;
            }
        }
```

#### 1.2 Table Definition in `new` (lines 1272-1284)
Add `tool_names TEXT` to the base `CREATE TABLE IF NOT EXISTS sessions`:
```rust
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                session_key TEXT,
                chat_id TEXT,
                chat_type TEXT,
                thread_id TEXT,
                started_at REAL NOT NULL,
                message_count INTEGER DEFAULT 0,
                last_activity_at REAL,
                tool_names TEXT
            )",
            [],
        )?;
```

#### 1.3 Add Method `update_session_tool_names` (around line 960)
```rust
    /// Persist the session's resolved tools[] name order as a JSON array string.
    /// Mirrored from hermes_state.py:update_session_tool_names.
    pub fn update_session_tool_names(
        &self,
        session_id: &str,
        tool_names: Option<&[String]>,
    ) -> rusqlite::Result<()> {
        let payload = tool_names.map(|names| serde_json::to_string(names).unwrap_or_default());
        self.conn.lock().unwrap().execute(
            "UPDATE sessions SET tool_names = ?1 WHERE id = ?2",
            rusqlite::params![payload, session_id],
        )?;
        Ok(())
    }

    /// Read the session's saved tool name order, if present and valid.
    pub fn get_session_tool_names(&self, session_id: &str) -> rusqlite::Result<Option<Vec<String>>> {
        let raw: Option<String> = self.conn.lock().unwrap().query_row(
            "SELECT tool_names FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        ).optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&raw).ok())
    }
```

---

### Step 2: Update `PromptStore` & `Resolution` in `conversation_prompt.rs`
**File**: [`rust/crates/hermes-gateway/src/conversation_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs)

#### 2.1 Update `Resolution` Struct (lines 43-53)
Include `saved_tool_names` in [`Resolution`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L44-L53):
```rust
#[derive(Debug, PartialEq, Eq)]
pub struct Resolution {
    pub prompt: String,
    pub stored_state: StoredState,
    pub reused: bool,
    pub restore_frozen_sections: bool,
    pub reconstruct_static_prefix: bool,
    pub refreshed_capability: bool,
    pub read_attempted: bool,
    pub persist_attempted: bool,
    /// Parsed saved tool name order from the session row, if present.
    pub saved_tool_names: Option<Vec<String>>,
}
```

#### 2.2 Update `PromptStore` Trait (lines 55-61)
Add `persist_tool_names` with a default no-op:
```rust
pub trait PromptStore: Sync {
    fn session_row(&self, id: &str) -> anyhow::Result<Option<Value>>;
    fn persist_prompt(&self, id: &str, prompt: &str) -> anyhow::Result<()>;
    fn persist_tool_names(&self, _id: &str, _tool_names: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
    fn build_snapshot(&self, _id: &str) -> anyhow::Result<BuildSnapshot> {
        Ok(BuildSnapshot::default())
    }
}
```

#### 2.3 Implement on `SessionDb` (lines 487-506)
```rust
impl PromptStore for crate::session_db::SessionDb {
    fn session_row(&self, id: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.get_session(id)?)
    }

    fn persist_prompt(&self, id: &str, prompt: &str) -> anyhow::Result<()> {
        Ok(self.update_system_prompt(id, Some(prompt))?)
    }

    fn persist_tool_names(&self, id: &str, tool_names: &[String]) -> anyhow::Result<()> {
        Ok(self.update_session_tool_names(id, Some(tool_names))?)
    }

    fn build_snapshot(&self, id: &str) -> anyhow::Result<BuildSnapshot> {
        Ok(BuildSnapshot {
            row: self.get_session(id)?,
            conversation_root: self
                .get_conversation_root(id)
                .ok()
                .filter(|root| !root.is_empty()),
            refresh_capability: false,
        })
    }
}
```

#### 2.4 Update `restore_or_build` Flow (lines 520-620)
Extract `saved_tool_names` from `row` under the existing single-read operation:
```rust
    let mut read_attempted = false;
    let row = if input.has_history {
        store.and_then(|store| {
            read_attempted = true;
            match store.session_row(session_id) {
                Ok(row) => row,
                Err(error) => {
                    tracing::warn!(%error, session_id, "Session DB system prompt read failed; rebuilding");
                    None
                }
            }
        })
    } else {
        None
    };

    let saved_tool_names = row.as_ref().and_then(|r| {
        r.get("tool_names").and_then(|v| match v {
            Value::String(s) => serde_json::from_str::<Vec<String>>(s).ok(),
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => None,
        })
    });
```
On reuse return (lines 577-587):
```rust
            if !capability_stale && !legacy_bot_upgrade {
                return Ok(Resolution {
                    prompt: stored.to_owned(),
                    stored_state: state,
                    reused: true,
                    restore_frozen_sections: true,
                    reconstruct_static_prefix: true,
                    refreshed_capability: false,
                    read_attempted,
                    persist_attempted: false,
                    saved_tool_names,
                });
            }
```
On fresh build return (lines 611-620):
```rust
    Ok(Resolution {
        prompt,
        stored_state: state,
        reused: false,
        restore_frozen_sections: false,
        reconstruct_static_prefix: false,
        refreshed_capability: capability_stale || legacy_bot_upgrade,
        read_attempted,
        persist_attempted,
        saved_tool_names: None,
    })
```

---

### Step 3: Implement Tool Prefix Restoration in `native_tools.rs`
**File**: [`rust/crates/hermes-gateway/src/native_tools.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs)

#### 3.1 Implement `restore_tool_prefix` (around line 740)
```rust
/// Restore and fold a fresh tool list onto a session's saved tool order.
///
/// Mirrors Python's `tools.mcp_tool.restore_agent_tool_prefix` and
/// `_merge_preserving_prefix`:
/// 1. Tools in both `saved_names` and `fresh_tools` keep their slot in `saved_names`,
///    taking the fresh tool instance.
/// 2. Tools in `saved_names` missing from `fresh_tools` are retained if present in
///    `registered_tools` (live availability probe flapped).
/// 3. Tools in `saved_names` missing from both `fresh_tools` and `registered_tools`
///    are dropped (deregistered).
/// 4. Genuinely new tools in `fresh_tools` append at the tail in their fresh order.
///
/// Returns `(merged_tools, names_changed)`, where `names_changed` indicates if the
/// resulting order/names differ from `saved_names`, requiring SQLite persistence.
pub fn restore_tool_prefix(
    saved_names: &[String],
    fresh_tools: Vec<Arc<dyn Tool>>,
    registered_tools: &[Arc<dyn Tool>],
) -> (Vec<Arc<dyn Tool>>, bool) {
    if saved_names.is_empty() {
        let changed = !fresh_tools.is_empty();
        return (fresh_tools, changed);
    }

    let mut fresh_map: std::collections::HashMap<String, Arc<dyn Tool>> = fresh_tools
        .iter()
        .map(|tool| (tool.spec().name, tool.clone()))
        .collect();

    let registered_map: std::collections::HashMap<String, Arc<dyn Tool>> = registered_tools
        .iter()
        .map(|tool| (tool.spec().name, tool.clone()))
        .collect();

    let mut merged = Vec::new();
    let mut consumed = std::collections::HashSet::new();

    // 1. Process saved names preserving exact prefix order
    for name in saved_names {
        if let Some(fresh_tool) = fresh_map.remove(name) {
            consumed.insert(name.clone());
            merged.push(fresh_tool);
        } else if let Some(reg_tool) = registered_map.get(name) {
            // Still registered; live probe flapped. Retain!
            consumed.insert(name.clone());
            merged.push(reg_tool.clone());
        }
        // Else: tool was deregistered; drop.
    }

    // 2. Append genuinely new tools in their original fresh order
    for fresh_tool in &fresh_tools {
        let name = fresh_tool.spec().name;
        if !consumed.contains(&name) {
            consumed.insert(name);
            merged.push(fresh_tool.clone());
        }
    }

    // 3. Determine if merged tool names differ from saved_names
    let names_changed = merged.len() != saved_names.len()
        || merged
            .iter()
            .zip(saved_names.iter())
            .any(|(m, s)| m.spec().name != *s);

    (merged, names_changed)
}
```

---

### Step 4: Add Frozen Plugin Sections to `NativeAgentClient`
**File**: [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs)

#### 4.1 Struct Definition Update (lines 205-213)
```rust
    /// Already assembled conversation prompt. Clones share the same immutable
    /// bytes, including across tool rounds; construction never reads files here.
    system_prompt: Option<std::sync::Arc<str>>,
    /// Frozen plugin prompt sections recovered from the stored prompt or rendered
    /// on session start. Lifetime matches system_prompt.
    plugin_sections: Option<std::sync::Arc<[crate::plugin_prompt::Section]>>,
    turn_limit: usize,
    max_concurrent_children: usize,
    /// When non-empty, turns run through the tool-calling loop (non-streaming);
    /// when empty, run_turn streams a plain completion.
    tools: Vec<std::sync::Arc<dyn crate::native_tools::Tool>>,
```

#### 4.2 Constructor & Methods (lines 240-250)
```rust
    pub fn with_plugin_sections(mut self, sections: Vec<crate::plugin_prompt::Section>) -> Self {
        self.plugin_sections = if sections.is_empty() {
            None
        } else {
            Some(sections.into())
        };
        self
    }

    pub fn plugin_sections(&self) -> Option<&[crate::plugin_prompt::Section]> {
        self.plugin_sections.as_deref()
    }
```

---

### Step 5: Wire Factory Ingress in `main.rs`
**File**: [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)

#### 5.1 Update `build_agent_client_for_home` (lines 385-398)
Accept tools explicitly rather than synthesizing [`CurrentTimeTool`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L717-L735) unconditionally:
```rust
fn build_agent_client_for_home(
    config: &Config,
    selected: &Value,
    model: Option<&str>,
    home: &std::path::Path,
    system_prompt: Option<String>,
    tools: Vec<Arc<dyn crate::native_tools::Tool>>,
    plugin_sections: Vec<crate::plugin_prompt::Section>,
) -> anyhow::Result<Arc<dyn AgentClient>> {
```
And apply them:
```rust
                    if !tools.is_empty() {
                        c = c.with_tools(tools);
                        tracing::info!(model, base_url, "using native agent client (tools enabled)");
                    } else {
                        tracing::info!(model, base_url, "using native agent client");
                    }
                    if let Some(prompt) = system_prompt {
                        c = c.with_system_prompt(prompt);
                    }
                    if !plugin_sections.is_empty() {
                        c = c.with_plugin_sections(plugin_sections);
                    }
                    return Ok(Arc::new(c));
```

#### 5.2 Update `build_conversation_client` (lines 450-495)
```rust
    let fresh_tools: Vec<Arc<dyn crate::native_tools::Tool>> = if config.agent_tools {
        vec![Arc::new(crate::native_tools::CurrentTimeTool)]
    } else {
        Vec::new()
    };
    let fresh_tool_names: Vec<String> = fresh_tools.iter().map(|t| t.spec().name).collect();
    let registered_tools: Vec<Arc<dyn crate::native_tools::Tool>> = if config.agent_tools {
        vec![Arc::new(crate::native_tools::CurrentTimeTool)]
    } else {
        Vec::new()
    };

    let bot = initializer.bot_inputs(home, Some(&selected));
    let resolution = conversation_prompt::restore_or_build(
        database.map(|database| database as &dyn conversation_prompt::PromptStore),
        &session_id,
        &conversation_prompt::RestoreInputs {
            has_history: !history.is_empty(),
            runtime: system_prompt::PromptRuntime {
                model: &model,
                provider: &provider,
                platform: &platform,
                cwd: &runtime_cwd,
            },
            capability_stale: false,
            legacy_bot_upgrade: false,
            bot: Some(bot),
        },
        |snapshot| {
            initializer.build_fresh(conversation_prompt::FreshPromptInputs {
                home,
                config: &selected,
                model: &model,
                provider: &provider,
                platform: &platform,
                session_id: &session_id,
                tools: &fresh_tool_names,
                snapshot,
            })
        },
    )
    .await?;

    let (effective_tools, plugin_sections) = if resolution.restore_frozen_sections {
        // 1. Reused path: Restore frozen plugin sections from prompt bytes without executing callbacks
        let restored_sections = crate::plugin_prompt::restore(&resolution.prompt);

        // 2. Reused path: Restore tool prefix, retaining flapped tools and appending new ones
        let saved_names = resolution.saved_tool_names.as_deref().unwrap_or(&[]);
        let (merged_tools, names_changed) = crate::native_tools::restore_tool_prefix(
            saved_names,
            fresh_tools,
            &registered_tools,
        );

        // 3. If prefix merge adjusted names, persist updated order to SQLite before provider I/O
        if names_changed {
            if let Some(database) = database {
                let merged_names: Vec<String> = merged_tools.iter().map(|t| t.spec().name).collect();
                if let Err(error) = database.update_session_tool_names(&session_id, Some(&merged_names)) {
                    tracing::warn!(%error, %session_id, "Failed to persist updated tool prefix order");
                }
            }
        }
        (merged_tools, restored_sections)
    } else {
        // Fresh path: persist initial tool order to SQLite before provider I/O
        if let Some(database) = database {
            if let Err(error) = database.update_session_tool_names(&session_id, Some(&fresh_tool_names)) {
                tracing::warn!(%error, %session_id, "Failed to persist initial tool name order");
            }
        }
        (fresh_tools, Vec::new())
    };

    build_agent_client_for_home(
        config,
        &selected,
        Some(&model),
        home,
        Some(resolution.prompt),
        effective_tools,
        plugin_sections,
    )
```

---

## 5. Exact Tests & Verification Strategy

### 5.1 Unit Tests for `session_db.rs`
Test file: inline in `src/session_db.rs`
1. `session_tool_names_round_trip`:
   - Initialize temporary test [`SessionDb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs).
   - Call `create_session("s_tools", "cli", ...)`
   - Call `update_session_tool_names("s_tools", Some(&["current_time".into(), "web_search".into()]))`.
   - Read via `get_session("s_tools")` and verify `row["tool_names"] == json!(r#"["current_time","web_search"]"#)`.
   - Read via `get_session_tool_names("s_tools")` and verify `Some(vec!["current_time", "web_search"])`.
2. `session_tool_names_migration_preserves_data`:
   - Construct raw SQLite database with sessions table lacking `tool_names`.
   - Run [`ensure_recovery_schema`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1077-L1121).
   - Verify `PRAGMA table_info(sessions)` contains `tool_names TEXT`.

### 5.2 Unit Tests for `native_tools::restore_tool_prefix`
Test file: inline in `src/native_tools.rs`
```rust
    #[test]
    fn restore_tool_prefix_preserves_order_and_handles_all_four_rules() {
        struct NamedTool(&'static str);
        impl Tool for NamedTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec { name: self.0.into(), description: String::new(), parameters: json!({}) }
            }
            fn call(&self, _args: &Value) -> hermes_core::Result<String> { Ok(String::new()) }
        }

        let tool_a: Arc<dyn Tool> = Arc::new(NamedTool("tool_a"));
        let tool_b: Arc<dyn Tool> = Arc::new(NamedTool("tool_b"));
        let tool_c: Arc<dyn Tool> = Arc::new(NamedTool("tool_c"));
        let tool_flapped: Arc<dyn Tool> = Arc::new(NamedTool("tool_flapped"));
        let tool_new: Arc<dyn Tool> = Arc::new(NamedTool("tool_new"));

        // Saved names from previous turn
        let saved = vec![
            "tool_a".to_owned(),
            "tool_flapped".to_owned(),
            "tool_deregistered".to_owned(),
            "tool_b".to_owned(),
        ];

        // Fresh tools from live probe: tool_b was probed before tool_a, tool_flapped is missing, tool_new is added
        let fresh = vec![tool_b.clone(), tool_new.clone(), tool_a.clone()];

        // Registered catalog contains registered tools (including the flapped one)
        let registered = vec![tool_a.clone(), tool_b.clone(), tool_c.clone(), tool_flapped.clone()];

        let (merged, changed) = restore_tool_prefix(&saved, fresh, &registered);

        let names: Vec<String> = merged.iter().map(|t| t.spec().name).collect();
        // Expected outcome:
        // 1. tool_a kept slot 0
        // 2. tool_flapped retained at slot 1 (probe flapped, still registered)
        // 3. tool_deregistered dropped (absent from fresh AND registered)
        // 4. tool_b kept slot 3 (now slot 2 after drop)
        // 5. tool_new appended at the tail
        assert_eq!(names, vec!["tool_a", "tool_flapped", "tool_b", "tool_new"]);
        assert!(changed);
    }

    #[test]
    fn restore_tool_prefix_idempotent_when_unchanged() {
        struct NamedTool(&'static str);
        impl Tool for NamedTool {
            fn spec(&self) -> ToolSpec { ToolSpec { name: self.0.into(), description: String::new(), parameters: json!({}) } }
            fn call(&self, _args: &Value) -> hermes_core::Result<String> { Ok(String::new()) }
        }
        let tool_a: Arc<dyn Tool> = Arc::new(NamedTool("current_time"));
        let saved = vec!["current_time".to_owned()];
        let fresh = vec![tool_a.clone()];
        let registered = vec![tool_a.clone()];

        let (merged, changed) = restore_tool_prefix(&saved, fresh, &registered);
        let names: Vec<String> = merged.iter().map(|t| t.spec().name).collect();
        assert_eq!(names, vec!["current_time"]);
        assert!(!changed);
    }
```

### 5.3 Unit Tests for `conversation_prompt::restore_or_build` Tool Names
Test file: inline in `src/conversation_prompt.rs`
1. `restore_or_build_extracts_saved_tool_names_on_reuse`:
   - Mock store returns a row with `"tool_names": Value::String(r#"["current_time"]"#)`.
   - Call [`restore_or_build`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L510-L621) with `has_history = true`.
   - Verify `resolution.reused == true`.
   - Verify `resolution.restore_frozen_sections == true`.
   - Verify `resolution.saved_tool_names == Some(vec!["current_time".to_string()])`.

### 5.4 End-to-End Integration Verification in `conversation_agent.rs`
Test file: inline in `src/conversation_agent.rs`
1. `multi_turn_restores_frozen_plugin_sections_and_preserves_tool_prefix`:
   - Turn 1 (Fresh Session):
     - Prompt contains framed plugin section `## Plugin Context: test_plugin`.
     - `database.update_session_tool_names` records `["current_time"]`.
     - Client created and executes Turn 1.
   - Turn 2 (Simulated Cache Eviction):
     - Agent client dropped from [`ConversationAgent`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L28-L33) cache.
     - Turn 2 triggers reconstruction.
     - `database.get_session` returns prompt and `tool_names`.
     - [`restore_or_build`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L510-L621) returns `reused: true, restore_frozen_sections: true`.
     - Verify client is reconstructed with:
       - Identical prompt byte slice (`Arc<str>`).
       - Plugin section restored verbatim with `id == "test_plugin"` without evaluating callbacks.
       - Tool array maintains exact `["current_time"]` prefix.
     - Verify provider I/O runs with zero locks held.

---

## 6. Concise Ordered Implementation Checklist

1. **Phase 1: SQLite Storage Layer ([`session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs))**
   - [ ] Add `("tool_names", "TEXT")` to [`ensure_recovery_schema`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1077-L1121) ALTER TABLE loop.
   - [ ] Add `tool_names TEXT` to base `CREATE TABLE IF NOT EXISTS sessions`.
   - [ ] Add `SessionDb::update_session_tool_names(&self, session_id: &str, tool_names: Option<&[String]>) -> rusqlite::Result<()>`.
   - [ ] Add `SessionDb::get_session_tool_names(&self, session_id: &str) -> rusqlite::Result<Option<Vec<String>>>`.
   - [ ] Add unit tests verifying `tool_names` write, read, and schema migration.

2. **Phase 2: Conversation Prompt Contract ([`conversation_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs))**
   - [ ] Add `pub saved_tool_names: Option<Vec<String>>` to [`Resolution`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L44-L53).
   - [ ] Add `fn persist_tool_names(&self, id: &str, tool_names: &[String]) -> anyhow::Result<()>` to [`PromptStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L55-L61) trait with default no-op.
   - [ ] Implement `persist_tool_names` on `impl PromptStore for SessionDb`.
   - [ ] In [`restore_or_build`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L510-L621), extract `saved_tool_names` from `row` and populate `Resolution.saved_tool_names`.
   - [ ] Add unit test verifying `saved_tool_names` recovery in `tests::restore_or_build`.

3. **Phase 3: Tool Prefix Restoration Algorithm ([`native_tools.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs))**
   - [ ] Implement `pub fn restore_tool_prefix(saved_names: &[String], fresh_tools: Vec<Arc<dyn Tool>>, registered_tools: &[Arc<dyn Tool>]) -> (Vec<Arc<dyn Tool>>, bool)`.
   - [ ] Implement the 4 prefix merge rules: slot preservation, flapped probe retention from `registered_tools`, deregistered tool dropping, and tail appending of new tools.
   - [ ] Add unit tests verifying all 4 merge rules and idempotency.

4. **Phase 4: Client Ownership ([`native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs))**
   - [ ] Add `plugin_sections: Option<Arc<[crate::plugin_prompt::Section]>>` to [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L194-L213).
   - [ ] Add builder `with_plugin_sections(mut self, sections: Vec<crate::plugin_prompt::Section>) -> Self`.
   - [ ] Add getter `plugin_sections(&self) -> Option<&[crate::plugin_prompt::Section]>`.

5. **Phase 5: Factory Wiring & Ingress ([`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs))**
   - [ ] Update `build_agent_client_for_home` signature to accept `tools` and `plugin_sections`.
   - [ ] Update `build_conversation_client`:
     - On `resolution.restore_frozen_sections == true`:
       - Restore plugin sections via [`plugin_prompt::restore(&resolution.prompt)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L298-L344).
       - Restore tool prefix via `native_tools::restore_tool_prefix`.
       - If tool order changed, update SQLite via `database.update_session_tool_names`.
     - On fresh build:
       - Persist initial tool names to SQLite via `database.update_session_tool_names` before model I/O.
   - [ ] Run `cargo check` and `cargo test -p hermes-gateway` to verify full compilation and test suite green.
