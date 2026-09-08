# Frozen Native Conversation State Checkpoint Review

## Executive Summary

This review assesses the uncommitted implementation of the frozen native conversation state checkpoint across `rust/crates/hermes-gateway` (`conversation_prompt.rs`, `main.rs`, `native_agent.rs`, `native_tools.rs`, `plugin_prompt.rs`, and `session_db.rs`) in comparison to the Python reference implementation (`agent/conversation_loop.py`, `tools/mcp_tool.py`, `agent/system_prompt.py`, `hermes_state.py`, and `hermes_state_common.py`).

Verdict: **Two concrete semantic mismatches block this checkpoint.** Both occur in `native_tools::restore_tool_prefix` and its call site in `build_conversation_client`:
1. **Duplicate tool handling semantic inversion**: When a tool name appears more than once in `saved_names`, the current Rust implementation consumes the fresh instance on the first occurrence via `fresh.remove(name)`. The second occurrence falls back to `registered.get(name)`, assigning an older registered fallback definition instead of the fresh implementation. The accompanying unit test in `native_tools.rs` explicitly codifies this bug. In Python, all occurrences of a tool present in `fresh_defs` receive the fresh definition.
2. **Empty `saved_names` handling mismatch**: When a continuing session has `tool_names` stored as `"[]"` (for example, created when agent tools were disabled), Rust treats `&[]` as an active pin. It appends all currently available fresh tools, marks `changed = true`, and re-persists the new names to SQLite. In Python, `restore_agent_tool_prefix` guards with `if not saved_names: return False`, treating an empty list as unpinned and avoiding database writes.

All other evaluated areas (schema persistence, read/build/write ordering before provider I/O, availability-flip retention for non-duplicate tools, callback-free plugin section restoration, per-conversation client ownership, prompt byte identity, tool schema byte stability, and lock boundaries) adhere strictly to specification.

---

## Audit by Investigation Area

### 1. `sessions.tool_names` Schema and Persistence Behavior

- **Schema Definition**:
  - `session_db::ensure_recovery_schema` (`session_db.rs:1130`) includes `("tool_names", "TEXT")` in its `ALTER TABLE` loop for existing stores.
  - The base schema in `SessionDb::new` (`session_db.rs:1302`) adds `tool_names TEXT` to the initial `CREATE TABLE IF NOT EXISTS sessions`.
  - This matches `hermes_state_common.py:477` where `tool_names TEXT` resides on `sessions`.
- **Persistence Helper**:
  - `SessionDb::update_session_tool_names` (`session_db.rs:1061-1075`) serializes `Option<&[String]>`:
    - `Some(&[...])` serializes to a JSON array text payload.
    - `Some(&[])` serializes explicitly to `"[]"`.
    - `None` binds `NULL`.
  - This matches Python `hermes_state.py:9635-9653` where `json.dumps(list(tool_names)) if tool_names is not None else None` is executed via an `UPDATE` statement.
- **Query and Deserialization**:
  - `SessionDb::get_session` reads `tool_names` dynamically through `session_row_value`, returning it as `Value::String` or `Value::Null`.
  - `conversation_prompt::restore_or_build` (`conversation_prompt.rs:586-597`) reads `row["tool_names"]`, ignores empty strings, and parses via `serde_json::from_str::<Vec<String>>`. Invalid JSON logs at debug level and gracefully yields `None`.
- **Verdict**: Fully compliant.

---

### 2. `restore_or_build` Read/Build/Write Ordering and Best-Effort Failures

- **Ordering Invariants**:
  - **Read phase**: `store.session_row(session_id)` is invoked only when `has_history` is true (`conversation_prompt.rs:555-570`).
  - **Reuse branch**: When the stored prompt matches runtime identity and neither bot capabilities nor protocol upgrades are stale, the stored prompt is reused verbatim. `saved_tool_names` is extracted from the row, and `restore_or_build` returns early with `persist_attempted = false` (`conversation_prompt.rs:586-608`). Zero SQLite writes occur.
  - **Fresh build branch**: When prompt reuse is not possible, `build(snapshot).await?` executes first with no intermediate database locks.
  - **Write phase**: Immediately following prompt assembly, `store.persist_prompt(session_id, &prompt)` runs (`conversation_prompt.rs:629`). Only if prompt persistence succeeds does `store.persist_tool_names(session_id, input.tool_names)` run (`conversation_prompt.rs:631`).
  - **Boundary Guarantee**: All database reads, assembly, and persistence complete before `NativeAgentClient` begins model wire I/O.
- **Best-Effort Failure Behavior**:
  - A failure in `persist_prompt` logs a warning (`warn!`) and allows execution to proceed without failing the turn (`conversation_prompt.rs:636-638`).
  - A failure in `persist_tool_names` logs at debug level (`debug!`) and proceeds (`conversation_prompt.rs:632-634`).
  - This precisely replicates `agent/conversation_loop.py:1215-1226` and `tools/mcp_tool.py:8923-8924`.
- **Verdict**: Fully compliant.

---

### 3. `native_tools::restore_tool_prefix` versus Python `_merge_preserving_prefix` and `restore_agent_tool_prefix`

Two concrete semantic mismatches exist in `native_tools::restore_tool_prefix`:

#### A. Inverted Duplicate Resolution
In `rust/crates/hermes-gateway/src/native_tools.rs:59-71`:
```rust
let mut merged = Vec::new();
for name in saved_names {
    if let Some(tool) = fresh.remove(name) {
        merged.push(tool);
    } else if let Some(tool) = registered.get(name) {
        merged.push(tool.clone());
    }
}
```
- If `saved_names` contains duplicate entries for tool `"a"`, the first iteration removes `"a"` from `fresh` and pushes the fresh tool implementation.
- The second iteration finds `fresh.remove("a")` is `None`. It drops into `registered.get("a")`, pushing the fallback tool from `registered_tools`.
- If `registered_tools` holds an older or base definition of `"a"` (which is standard for catalog fallbacks), slot 0 gets the fresh version while slot 2 gets the stale registered version.
- **Python Reference**:
  In `tools/mcp_tool.py:8946-8954`, `saved_defs` resolves each name in `saved_names` against `fresh.get(name)` without mutating `fresh`. Both slots in `saved_defs` hold the fresh definition.
  Then in `_merge_preserving_prefix` (`tools/mcp_tool.py:8990-8996`), the first slot pops `"a"` from `fresh` and appends it. The second slot finds `fresh.pop("a", None)` is `None`, and falls into `elif name and name in registered_names: merged.append(entry)`. The variable `entry` is the element from `saved_defs`, which is the fresh definition.
  A live probe of Python confirms: for `saved_names = ["a", "flapped", "removed", "a"]`, Python produces descriptions `['new', 'registered', 'new', 'new']`.
- **The Rust unit test bug**:
  Line 770 in `rust/crates/hermes-gateway/src/native_tools.rs` tests:
  ```rust
  assert_eq!(merged[0].spec().description, "new");
  assert_eq!(merged[1].spec().description, "registered");
  assert_eq!(merged[2].spec().description, "old");
  ```
  The test author explicitly codified the incorrect behavior into the unit test.

#### B. Empty `saved_names` Handled as a Mutated Active Pin
- In Python `tools/mcp_tool.py:8940-8941`:
  ```python
  def restore_agent_tool_prefix(agent, saved_names: list) -> bool:
      if not saved_names:
          return False
  ```
  When `saved_names` is empty, Python returns `False` immediately, leaving `agent.tools` untouched and writing nothing to SQLite.
- In Rust `native_tools.rs:43-76` and `main.rs:514-533`:
  When `saved_names` is `&[]`:
  - `restore_tool_prefix` iterates over `saved_names` (0 items), then pushes all `fresh_order` tools into `merged`.
  - `let changed = tool_names(&merged) != saved_names` evaluates `["current_time"] != []`, which is `true`.
  - In `main.rs`, `if changed` triggers, and `database.update_session_tool_names` overwrites SQLite with `["current_time"]`.
- **Verdict**: BLOCKING. Must be corrected to preserve exact Python merge semantics.

---

### 4. Live Availability-Flip Integration in `build_conversation_client`

- **Flap from Available to Unavailable**:
  - If a session initially runs with `config.agent_tools = true` (`tool_names = ["current_time"]`), and a subsequent process restart or configuration switch sets `config.agent_tools = false`:
  - `fresh_tools` is empty.
  - On prompt reuse, `saved_tool_names` is `Some(["current_time"])`.
  - `restore_tool_prefix` matches `"current_time"` in `registered_tools`, retaining `CurrentTimeTool`.
  - `changed` evaluates to `false` because `tool_names(&merged) == ["current_time"]`.
  - The client is constructed with `CurrentTimeTool` preserved in its tool array, ensuring prefix cache stability across availability flaps.
  - Verified by integration test `main.rs:1070-1099` (`startup_tests::conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`).
- **Stateless Operation**:
  - When `database` is `None`, `database.update_session_tool_names` is skipped gracefully without errors.
- **Verdict**: Fully compliant.

---

### 5. Per-Conversation Plugin Snapshot Ownership and Callback-Free Restoration

- **Callback-Free Extraction**:
  - In `main.rs:513`, when `resolution.restore_frozen_sections` is `true`, `plugin_prompt.restore(&resolution.prompt)` is invoked.
  - `plugin_prompt::restore` (`plugin_prompt.rs:303-345`) parses the section frames using `fancy_regex::Regex`, validates character counts, and verifies that re-formatting reproduces the exact delimited block.
  - No external plugin callbacks, Python hooks, or dynamically registered render closures are executed.
- **Per-Conversation Lifetime**:
  - The resulting `plugin_prompt::Snapshot` is packed into `NativeConversationState` and passed to `build_agent_client_for_home`.
  - `NativeAgentClient` stores it in `self._plugin_prompt` (`native_agent.rs:208`).
  - The client instance is cached in `ConversationAgent`'s `tokio::sync::OnceCell` map keyed by `(home, session_id)` (`conversation_agent.rs:83-105`).
  - Its lifetime exactly matches the immutable system prompt and tools array.
- **Verdict**: Fully compliant.

---

### 6. Prompt Byte Identity, Tool Schema Byte Stability, Locks, and Provider-I/O Boundaries

- **Prompt Byte Identity**:
  - `resolution.prompt` is converted directly to `Arc<str>` on `NativeAgentClient` without string manipulation or reformatting (`native_agent.rs:248`).
  - Turns prepend this prompt verbatim as the system message.
- **Tool Schema Stability**:
  - Wire tool objects are generated from `tools.iter().map(|t| tool_spec_json(&t.spec()))` (`native_tools.rs:451`).
  - Serialized wire payloads match byte-for-byte across turns, verified by `assert_eq!(requests[0]["tools"], requests[1]["tools"])` in `startup_tests`.
- **Lock and I/O Boundaries**:
  - `ConversationAgent::clients` mutex is released before factory execution (`conversation_agent.rs:86-90`).
  - SQLite queries and transactions inside `SessionDb` acquire and release `self.conn` exclusively within helper functions prior to model I/O.
  - Zero database connections or locks are held during model HTTP streaming or tool iteration loops.
- **Verdict**: Fully compliant.

---

## Concrete Issues Blocking This Checkpoint

### Issue 1 (Blocking): Inverted Duplicate Tool Resolution in `restore_tool_prefix`
- **Location**: [`rust/crates/hermes-gateway/src/native_tools.rs:59-71`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L59-L71) and unit test at [`rust/crates/hermes-gateway/src/native_tools.rs:766-772`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L766-L772).
- **Impact**: If a tool appears multiple times in `saved_names`, the first occurrence uses the fresh definition while later occurrences take the registered catalog fallback. In Python, all occurrences of a tool take the fresh definition if available in `fresh_defs`.
- **Correction**:
  Mirror Python's resolution pattern. First build an intermediate `saved_tools` vector resolving each saved name against `fresh` (non-mutating) or fallback `registered`. Then fold onto `fresh_order`, consuming each fresh tool from `fresh` on its first placement in `merged`, while allowing subsequent duplicate slots in `merged` to retain their resolved tool from `saved_tools` if the name is in `registered`. Update the test assertion at line 770 from `"old"` to `"new"`.

### Issue 2 (Blocking): Empty `saved_names` Mutates Session State and Updates SQLite
- **Location**: [`rust/crates/hermes-gateway/src/native_tools.rs:43-76`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L43-L76) and [`rust/crates/hermes-gateway/src/main.rs:514-531`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L514-L531).
- **Impact**: When `saved_names` is empty (for example, `tool_names` is `"[]"` in SQLite), `restore_tool_prefix` treats it as a non-empty prefix that changed, returning `(fresh_tools, true)`. `main.rs` then writes `fresh_tools` to SQLite. Python explicitly returns `False` when `not saved_names`, treating an empty list as unpinned and performing no database write.
- **Correction**:
  In `native_tools::restore_tool_prefix`, return `(fresh_tools, false)` immediately if `saved_names.is_empty()`. Alternatively, in `build_conversation_client`, check `match resolution.saved_tool_names.as_deref() { Some(saved) if !saved.is_empty() => ... }`.

---

## Concise Optional Follow-ups

1. **Public Snapshot Accessor on `NativeAgentClient`**:
   Add a public getter `pub fn plugin_prompt_snapshot(&self) -> &crate::plugin_prompt::Snapshot` on `NativeAgentClient`. Currently the field `_plugin_prompt` is private with no public accessor, though the test-only helper `sections(&self)` exists on `Snapshot`.
2. **Short-Circuit on `merged == fresh_tools`**:
   Python's `restore_agent_tool_prefix` exits early with `return False` when `merged == fresh_defs`, skipping persistence even if `saved_names` contained deleted tools. Rust's `changed = tool_names(&merged) != saved_names` immediately prunes deleted tools in SQLite. While Rust's behavior is more proactive, aligning or documenting this difference will avoid subtle divergences when MCP uninstallation tests are ported.
3. **Future Compression Rebuilder Integration**:
   When prompt context truncation and compression are ported to Rust, wire `NativeAgentClient._plugin_prompt` into the compression prompt rebuilder so restored section bytes are retained across context truncations without evaluating live plugin code.
