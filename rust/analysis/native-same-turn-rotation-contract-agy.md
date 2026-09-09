# Authoritative Python Contract: Same-Turn Rotation-Mode Context Compression

## Executive Summary

This document specifies the authoritative runtime behavior contract of the reference Python implementation for full context compression operating in rotation mode (`compression.in_place: false` or `agent.compression_in_place = False`) when compression triggers and commits mid-turn: specifically, after a batch of tool calls has completed execution and before the subsequent model provider request is issued.

In rotation mode, compression terminates the active parent session and atomically forks a continuation child session in SQLite, transferring ownership of the active conversational turn without breaking the execution loop. The model's next completion in the same turn is issued under the newly minted child session identity, reading the compacted transcript and operating under the continuing turn lease rooted at the conversation lineage ancestor.

All findings and code references herein are derived directly from the authoritative Python source code and focused test suites in this repository.

---

## Source Architecture and Exact Code References

### Primary Source Locations

1. **Active Tool Iteration Loop**:
   - [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8100-L8400): Tool-batch execution, token pressure assessment, compression trigger condition, in-loop compaction call, flush baseline re-anchoring, and loop continuation.
   - [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2289-L2650): Top of iteration loop, `/steer` drain, message argument sanitization, alternation repair, wire payload preparation (`api_messages`), and system prompt reconstruction.
   - [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L9123-L9145): Final assistant text message persistence.

2. **Agent Compression Forwarder and Marker Synchronization**:
   - [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L8588-L9006): `AIAgent._compress_context`, worker thread snapshotting, commit fence protection, progress timeout enforcement, marker synchronization across twin message lists, and thread-local/ContextVar propagation.
   - [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L2338-L2665): `AIAgent._flush_messages_to_session_db` and `_flush_messages_to_session_db_unlocked`, marker-based deduplication, bounded prefix scanning, and transactional batch appending.
   - [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L9325-L9740): Turn lease acquisition, background lease refresh, and lineage key resolution during active turn.

3. **Core Compression Engine and Session Splitting**:
   - [`agent/conversation_compression.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L3356-L5678): `compress_context`, compression lock acquisition, durable parent adoption check, auxiliary summarizer invocation, todo store and skill notice injection, user turn preservation, anti-growth guard, parent pre-flush, child publication, session identity mutation, state migration, memory and context-engine callbacks, failure rollback, and lock release.
   - [`agent/conversation_compression.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L2843-L2880): `conversation_history_after_compression`, determining the flush baseline (`None` for rotation mode).
   - [`agent/conversation_compression.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L3280-L3330): `_notify_context_engine_compression_complete`, context-engine boundary notification.

4. **Durable SQLite Storage Layer**:
   - [`hermes_state.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L8247-L8450): `SessionDB.publish_compression_child`, atomic transaction inserting child session row, inserting compacted handoff messages, cloning concurrent foreign tail messages, and stamping parent closure (`ended_at`, `end_reason='compression'`).
   - [`hermes_state.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L9255-L9305): `SessionDB._session_turn_lease_key_on_conn`, lineage walk across compression parent links to the root conversation ID.

5. **Turn Finalization and Gateway Handoff**:
   - [`agent/turn_finalizer.py`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L138-L863): `finalize_turn`, synthetic scaffolding stripping, assistant tail filling, session persistence, output transforms, post-LLM hooks, context engine turn completion, memory sync, and background review spawning.
   - [`gateway/run.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7305-L7350): `run_sync`, history offset resolution (`_effective_history_offset = 0` on rotation).
   - [`gateway/run.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L23317-L23350): Post-turn gateway `SessionEntry` rebind, turn lease tracking rebind, peer mapping update, and Telegram topic binding sync.

---

## Ordered State-Transition Table (Same-Turn Rotation)

The following table specifies the exact chronological sequence of operations when context compression runs in rotation mode between tool completion and the subsequent model provider request:

| Sequence | Phase | Operation / State Change | Code Reference | Invariants and Observable Contracts |
|---|---|---|---|---|
| 1 | Tool Execution | Tool batch executes; results appended to `messages` | `conversation_loop.py:8158` | All tool calls in current assistant batch are executed; results present in memory. |
| 2 | Pressure Eval | Check real tokens against compression threshold | `conversation_loop.py:8226-8264` | `agent.compression_enabled and compression_attempts < max and should_compress(_real_tokens)`. |
| 3 | Lock Acquisition | Acquire `compression_locks` for `parent_session_id` | `conversation_compression.py:3722` | Prevents concurrent compression by background review agents or competing processes. |
| 4 | Watermark Capture | Capture `_commit_watermark = MAX(id)` on parent | `conversation_compression.py:3732` | Establishes the floor for concurrent tail append cloning during slow aux LLM call. |
| 5 | Memory Pre-Check | Dispatch `memory_manager.on_pre_compress(...)` | `conversation_compression.py:4148-4172` | Checkpoint probed; provider-supplied summary guidance captured in `memory_context`. |
| 6 | Summary Generation | Invoke `context_compressor.compress(messages, ...)` | `conversation_compression.py:4300` | Aux LLM generates summary; returns new message list `compressed`. |
| 7 | Handoff Structuring | Todo refresh, skill notices, real user turn anchor | `conversation_compression.py:4626-4736` | Injects todo store items, reload notices; guarantees at least one user turn remains. |
| 8 | Anti-Growth Guard | Verify `rough_out <= rough_in`; run salvage pass | `conversation_compression.py:4822-4895` | Rejects candidate if summary expands token count; arms anti-thrash strike if refused. |
| 9 | Prompt Rebuild | Rebuild system prompt and reload dynamic tool schemas | `conversation_compression.py:4750-4794` | Dynamic tools refreshed; cached system prompt invalidated and rebuilt. |
| 10 | Memory Commit | Trigger `agent.commit_memory_session(messages)` | `conversation_compression.py:4805` | Memory extraction extracts knowledge from pre-compaction turns before rotation. |
| 11 | Pre-Publish Parent Flush | Flush unpersisted current turn to parent session | `conversation_compression.py:5022-5105` | Appends user ask, assistant tool-calls, and tool results to parent in SQLite. |
| 12 | Watermark Ceiling | Capture `_foreign_tail_ceiling = MAX(id)` on parent | `conversation_compression.py:5089-5093` | Excludes agent's own just-flushed tool rows from concurrent clone range. |
| 13 | Atomic Publication | `SessionDB.publish_compression_child(...)` | `hermes_state.py:8247-8450` | In 1 SQLite transaction: insert child session, insert handoff, clone concurrent tail, close parent. |
| 14 | Marker Stamping | Stamp `_DB_PERSISTED_MARKER = True` on handoff dicts | `conversation_compression.py:5156-5267` | Handoff dicts stamped durable; mirrored to caller lists via `_sync_persisted_markers`. |
| 15 | Identity Rebind | Rebind `agent.session_id = new_session_id` | `conversation_compression.py:5268-5282` | Updates agent identity, clears flush prefix, rebinds ContextVar and logging session context. |
| 16 | State Migration | Re-key goals, heartbeats, loops; copy title | `conversation_compression.py:5288-5353` | Persistent `/goal` and `/loop` re-keyed; title copied with exact source provenance. |
| 17 | Cursor Tracking Reset | Set `_last_flushed_db_idx = len(compressed)`, `_flushed_db_message_session_id = new_sid` | `conversation_compression.py:5362-5363` | Points flush tracking at child session; cursor set to end of compacted handoff. |
| 18 | Boundary Callbacks | Context engine, memory manager, and `session:compress` | `conversation_compression.py:5517-5584` | Non-blocking observer callbacks receive `new_session_id` and `old_session_id`. |
| 19 | Dedup Cache Reset | `reset_file_dedup(task_id)`, `reset_skill_view_dedup(task_id)` | `conversation_compression.py:5634-5645` | Advances deduplication generation so subsequent file reads return full content. |
| 20 | Token State Reset | Set `last_prompt_tokens = -1`, `awaiting_real_usage = True` | `conversation_compression.py:5592-5614` | Clears usage anchors; prevents premature compression on rough estimates. |
| 21 | Lock Release | Release compression lock on `old_session_id` | `conversation_compression.py:5667-5677` | Parent compression lock released in `finally` block via `_release_lock()`. |
| 22 | Loop Baseline Update | `conversation_history = conversation_history_after_compression(...)` | `conversation_loop.py:8297-8299` | In rotation mode, returns `None`; subsequent flushes rely on `_DB_PERSISTED_MARKER`. |
| 23 | Loop Continuation | Loop `continue`; prepare next model request | `conversation_loop.py:8397`, `2441-2636` | Sanitizes tool arguments, repairs sequence, strips markers to build `api_messages`. |
| 24 | Provider Request | Issue next model completion request | `conversation_loop.py:2700+` | Executed under existing turn lease; request contains compacted history and rebuilt prompt. |
| 25 | Final Flush | Model returns final answer; flush to child | `conversation_loop.py:9123-9141` | Only new assistant turn is written to child DB; handoff skipped by marker. |
| 26 | Turn Finalization | Scaffolding cleanup, post-LLM hooks, memory sync | `turn_finalizer.py:138-863` | Scaffolding stripped; `on_turn_complete`, `post_llm_call`, `on_session_end` run on child ID. |
| 27 | Gateway Rebind | Gateway updates `SessionEntry`, turn lease, and JSONL | `gateway/run.py:7310`, `23321-23340` | `history_offset = 0` persists full transcript; peer mapping and turn lease re-registered. |

---

## Detailed Behavioral Areas

### 1. `session_id` and Route Identity

- **Generation**: A new session identifier is minted using the timestamp and random hex suffix format:
  `f"{datetime.now().strftime('%Y%m%d_%H%M%S')}_{uuid.uuid4().hex[:6]}"` ([`conversation_compression.py:5122-5125`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5122-L5125)).
- **Atomic Registration**: The child session row is committed to SQLite via `SessionDB.publish_compression_child(...)` ([`hermes_state.py:8247-8450`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L8247-L8450)). The row copies the parent's `session_key`, `user_id`, `chat_id`, `chat_type`, `thread_id`, `display_name`, `origin_json`, and `profile_name`. This preserves the route identity directly in SQLite before memory updates occur.
- **In-Memory Rebind**:
  - `agent.session_id = new_session_id` ([`conversation_compression.py:5268`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5268)).
  - `agent._db_flush_scan_prefix = None` ([`conversation_compression.py:5269`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5269)).
  - `gateway.session_context.set_current_session_id(agent.session_id)` ([`conversation_compression.py:5271-5275`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5271-L5275)).
  - `os.environ["HERMES_SESSION_ID"] = agent.session_id` ([`conversation_compression.py:5275`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5275)).
  - `hermes_logging.set_session_context(agent.session_id)` ([`conversation_compression.py:5277-5281`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5277-L5281)).
  - When `compress_context` runs on a background worker thread (via `run_compress_context_with_progress_timeout`), these thread-local contexts and ContextVars are explicitly rebound on the caller thread upon worker completion ([`run_agent.py:8967-8992`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L8967-L8992)).
- **Post-Turn Gateway Propagation**: When `run_conversation` returns, the gateway detects that `agent_result["session_id"] != session_entry.session_id`. It updates `session_entry.session_id`, calls `self._rebind_turn_lease`, persists the session store, and updates peer mappings ([`gateway/run.py:23321-23340`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L23321-L23340)).

### 2. Durable Transcript Target

- **Parent Transcript Integrity**: Before terminating the parent session, any unpersisted current-turn messages (including the user prompt, assistant tool calls, and the newly executed tool results) must be committed to the parent session in `state.db`. This is achieved by calling `agent._flush_messages_to_session_db(messages, conversation_history=persisted_history)` ([`conversation_compression.py:5022-5105`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5022-L5105)), using `messages[:_persist_user_message_idx]` as the history boundary so only current-turn messages are appended.
- **Watermark Ceiling**: Immediately prior to the pre-publish parent flush, `_foreign_tail_ceiling = agent._session_db.get_active_message_watermark(agent.session_id)` is captured ([`conversation_compression.py:5089-5093`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5089-L5093)). Any row with `id <= _foreign_tail_ceiling` and `id > _commit_watermark` is considered a foreign concurrent append and cloned into the child. The agent's own just-flushed tool turns exceed `_foreign_tail_ceiling` and are not cloned, avoiding duplication with the compacted handoff.
- **Child Transcript Inception**: `publish_compression_child` inserts the compacted handoff messages directly into `messages` for `child_session_id` within the atomic transaction ([`hermes_state.py:8393-8395`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L8393-L8395)).
- **Marker Stamping and Deduplication**:
  - Every message dictionary in `compressed` has `_DB_PERSISTED_MARKER = True` set ([`conversation_compression.py:5265-5267`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5265-L5267)).
  - The anchor source message in `messages` and `agent._session_messages` is stamped by identity/content match ([`conversation_compression.py:5173-5264`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5173-L5264)).
  - `_sync_persisted_markers` synchronizes these stamps back to the caller lists ([`run_agent.py:8951-8966`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L8951-L8966)).
- **Flush Baseline Re-Anchoring**:
  - `conversation_history = conversation_history_after_compression(agent, messages, conversation_history)` ([`conversation_loop.py:8297-8299`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8297-L8299)).
  - For rotation mode (`attempt_in_place is False`), `conversation_history_after_compression` returns `None` ([`conversation_compression.py:2875`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L2875)).
  - Because all handoff messages in `messages` carry `_DB_PERSISTED_MARKER = True`, subsequent calls to `_flush_messages_to_session_db(messages, None)` skip the entire handoff via marker check ([`run_agent.py:2475`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L2475)), appending only newly generated messages (such as the subsequent assistant reply) into the child session.
- **Gateway JSONL Offset**: In `gateway/run.py:7310-7312`, `_effective_history_offset = 0 if (_session_was_split or _compacted_in_place) else len(agent_history)`. Because the session rotated (`_session_was_split = True`), the offset is forced to 0, ensuring the entire compacted transcript is written to the child's JSONL file on disk.

### 3. Turn/Lineage Lease Ownership

- **Turn Lease Invariant**: The turn lease (`session_turn_leases` table in `state.db`) is acquired at the beginning of the turn on the parent session ([`run_agent.py:9442`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L9442)).
- **Lineage Key Resolution**: The lease key in SQLite is resolved via `_session_turn_lease_key_on_conn(conn, session_id)` ([`hermes_state.py:9255-9291`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L9255-L9291)). This helper traverses parent links `WHERE parent_session_id IS NOT NULL AND end_reason = 'compression'` back to the lineage root.
- **Seamless Lease Continuity**: When rotation commits, the child's lease key resolves to the exact same root conversation ID as the parent. The in-memory holder token `self._active_session_turn_lease_holder` remains unchanged ([`run_agent.py:9516`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L9516)). The periodic background refresher `_refresh_durable_turn_lease` continues refreshing against `getattr(self, "session_id", None) or session_id` ([`run_agent.py:9697`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L9697)), which seamlessly resolves to the root conversation lease key without losing ownership or raising contention errors.
- **Compression Lock Lifecycle**: Unlike the turn lease, the compression lock (`compression_locks` table) is scoped to the specific session being compressed (`old_session_id`). It is acquired before summarization ([`conversation_compression.py:3722`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L3722)), validated during `publish_compression_child` ([`hermes_state.py:8307-8315`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L8307-L8315)), and released in the `finally` block of `compress_context` ([`conversation_compression.py:5673`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5673)). Subsequent compressions in later turns will acquire the lock on `new_session_id`.

### 4. Queued/Pending Tool State

- **Complete Batch Draining**: In `conversation_loop.py`, compression evaluates at line 8260 only after `agent._execute_tool_calls(...)` has returned at line 8158. Every tool call dispatched in that turn has already completed execution and its result has been appended to `messages`. There are no queued or pending tool calls from the batch awaiting execution when compression runs.
- **Tool Result Persistence**: Because tool results are already in `messages`, the pre-publish parent flush ([`conversation_compression.py:5100`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5100)) commits both the assistant tool-call turn and the tool-result turn to the parent before rotation.
- **Tool Schema Refresh**: At the admitted-commit boundary, `_refresh_agent_tool_definitions(agent)` ([`conversation_compression.py:4751`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L4751)) reloads dynamic tool schemas into `agent.tools`, ensuring any configuration modifications (e.g., model swaps, tool delegations) take effect before the next provider request.
- **Deduplication Cache Reset**:
  - `reset_file_dedup(task_id)` ([`conversation_compression.py:5636`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5636)) increments the file-read deduplication generation, ensuring subsequent tool calls can re-read previously inspected files if needed after compaction.
  - `reset_skill_view_dedup(task_id)` ([`conversation_compression.py:5643`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5643)) resets skill view deduplication.

### 5. Usage Accounting

- **Compacted Rough Tokens**: The rough token estimate of the compacted transcript and system prompt is computed and stored on `agent.context_compressor.last_compression_rough_tokens` ([`conversation_compression.py:5595-5600`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5595-L5600)).
- **Post-Compression Sentinel**:
  - `agent.context_compressor.last_prompt_tokens = -1` ([`conversation_compression.py:5601`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5601)).
  - `agent.context_compressor.last_completion_tokens = 0` ([`conversation_compression.py:5602`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5602)).
  - `agent.context_compressor.awaiting_real_usage_after_compression = True` ([`conversation_compression.py:5603`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5603)).
  - When the loop continues to the next iteration, line 8234 inspects `last_prompt_tokens == -1` and sets `_real_tokens = 0`. This prevents schema-heavy rough estimates from immediately re-triggering compression before provider-reported usage is received.
- **Anchor Invalidation**: `agent._usage_anchor = None` and `agent._turn_base_usage_anchor = None` ([`conversation_compression.py:5609-5610`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5609-L5610)) force context estimation to re-baseline.
- **Cumulative Session Tokens**: Cumulative metrics on `AIAgent` (`session_input_tokens`, `session_output_tokens`, `session_total_tokens`, `session_estimated_cost_usd`) represent conversational totals and are intentionally NOT zeroed upon rotation. They continue accumulating across the entire turn.
- **Compaction Effectiveness Record**: If the transcript shrank, `agent.context_compressor.record_completed_compaction(...)` is invoked ([`conversation_compression.py:5622`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5622)) to satisfy anti-thrashing and verification state.

### 6. Current Messages

- **Worker Snapshot Isolation**: The background worker thread executes against `copy.deepcopy(messages)` ([`run_agent.py:8721`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L8721)). The caller list is never modified directly by asynchronous worker execution.
- **Replaced Message List**: Upon successful compression, `messages` in `conversation_loop.py` is rebound to the returned `compressed` list ([`conversation_loop.py:8280`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8280)).
- **Synchronized Session Messages**: `agent._session_messages` is updated to point to `messages` ([`conversation_loop.py:8383`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8383)).
- **Reference Handoff Skip Check**:
  - Lines 8300-8311 evaluate `_should_skip_model_call_for_reference_handoff(messages, user_message)`.
  - If the resulting compressed history consists only of a reference handoff without actionable user instructions, the post-tool model call is skipped, `final_response = _HANDOFF_SKIP_FINAL_RESPONSE`, `_turn_exit_reason = "compaction_handoff_not_actionable"`, and the loop breaks immediately.
- **Wire Preparation for Subsequent Request**: When the loop continues to line 2289:
  - Tool arguments are sanitized ([`conversation_loop.py:2453`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2453)).
  - Role alternation is verified and repaired via `repair_message_sequence_with_cursor` ([`conversation_loop.py:2504`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2504)).
  - `api_messages` is constructed by deep-cloning each message via `_clone_message_for_send(msg)` ([`conversation_loop.py:2520`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2520)), stripping internal metadata (`api_content`, `display_kind`, `_row_id`, `_thinking_prefill`, etc.) before transmission.

### 7. Memory and Context-Engine Callbacks

- **Pre-Compaction Memory Checkpoint**: `memory_manager.on_pre_compress(...)` runs before auxiliary summarization ([`conversation_compression.py:4148-4172`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L4148-L4172)), providing external memory systems an opportunity to extract checkpoint data or inject summary guidance.
- **Pre-Split Memory Extraction**: `agent.commit_memory_session(messages)` is called at line 4805 immediately before database mutation, ensuring knowledge extraction runs over the pre-compaction turns.
- **Context-Engine Boundary Notification**:
  - Evaluated when `_context_engine_boundary_committed` is true ([`conversation_compression.py:5523`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5523)).
  - Calls `_notify_context_engine_compression_complete(agent, new_session_id=agent.session_id, old_session_id=_boundary_parent)` ([`conversation_compression.py:3280-3320`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L3280-L3320)).
  - Invokes `relay_runtime.SESSION_COORDINATOR.notify_session_compacted(...)`.
  - Invokes `agent.context_compressor.on_session_start(new_session_id, boundary_reason="compression", old_session_id=old_session_id, platform=..., conversation_id=...)`.
  - Observer semantics: any exception raised by context-engine hooks is caught and logged at debug level without aborting the committed compression.
- **Memory Manager Session Switch**:
  - `agent._memory_manager.on_session_switch(agent.session_id, parent_session_id=_boundary_parent, reset=False, reason="compression")` ([`conversation_compression.py:5545-5550`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5545-L5550)).
  - Notifies providers (e.g., Hindsight) that session identity has rotated while conversation continuity persists (`reset=False`). Handled inside `try/except` best-effort.

### 8. Hooks

- **`session:compress` Event Hook**:
  - Dispatched via `agent.event_callback("session:compress", payload)` ([`conversation_compression.py:5575-5581`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5575-L5581)).
  - Payload fields:
    ```python
    {
        "platform": agent.platform or "",
        "session_id": agent.session_id,      # Child ID
        "old_session_id": _old_sid or "",     # Parent ID
        "in_place": False,                    # Rotation mode
        "compression_count": agent.context_compressor.compression_count,
    }
    ```
  - Exceptions from `event_callback` are caught and logged at debug level.
- **`step_callback`**: Fired at the beginning of each iteration ([`conversation_loop.py:2365`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2365)) reporting `api_call_count` and previous tool executions.
- **Activity Heartbeats and Touches**:
  - Mid-compression heartbeat touches agent activity to satisfy gateway liveness monitors ([`conversation_compression.py:4201`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L4201)).
  - Post-tool activity touch: `agent._touch_activity("tool results posted, continuing iteration #...")` ([`conversation_loop.py:8395`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8395)).
  - Pre-API activity touch: `agent._touch_activity("starting API call #...")` ([`conversation_loop.py:2328`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2328)).
- **Post-Turn Lifecycle Hooks**: Deferred until turn completion in `turn_finalizer.py`:
  - `transform_llm_output` ([`turn_finalizer.py:623-635`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L623-L635)).
  - `post_llm_call` ([`turn_finalizer.py:646-658`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L646-L658)).
  - `_notify_context_engine_turn_complete` / `on_turn_complete` ([`turn_finalizer.py:673-684`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L673-L684)).
  - `on_session_end` ([`turn_finalizer.py:844-857`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L844-L857)).
  - All post-turn hooks observe `session_id = child_session_id`.

### 9. Final Response Persistence

- **Mid-Loop Text Completion**: When the model finishes generating its text response without requesting further tools:
  - `final_msg = agent._build_assistant_message(assistant_message, finish_reason)` ([`conversation_loop.py:8879`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8879)).
  - Appended to memory: `append_message(messages, final_msg)` ([`conversation_loop.py:9123`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L9123)).
  - Flushed to SQLite: `agent._flush_messages_to_session_db(messages, conversation_history)` ([`conversation_loop.py:9133`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L9133)).
- **Deduplication Mechanics in Child Session**:
  - `conversation_history` is `None`.
  - `agent.session_id` is `child_session_id`.
  - Inside `_flush_messages_to_session_db_unlocked` ([`run_agent.py:2460-2650`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L2460-L2650)), all handoff messages are skipped because they have `_DB_PERSISTED_MARKER = True`.
  - Only `final_msg` lacks the marker. It is written to `messages` under `child_session_id` via `append_messages_batch`, and `sync_flushed_message_markers` marks it durable.
- **Turn Finalizer Safety Net**:
  - `finalize_turn` calls `agent._persist_session(messages, conversation_history)` ([`turn_finalizer.py:473`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L473)).
  - Because `final_msg` is already marked durable, this call is completely idempotent and emits zero duplicate writes.

### 10. Session Metadata, Title, and CWD

- **SQLite Metadata Inheritance**: `publish_compression_child` copies the parent session attributes to the child row ([`hermes_state.py:8355-8391`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L8355-L8391)):
  - `cwd = cwd or parent["cwd"]` (working directory preserved).
  - `git_branch = parent["git_branch"]` and `git_repo_root = parent["git_repo_root"]` (git context preserved).
  - `profile_name = profile_name or parent["profile_name"] or self._own_profile_name()`.
  - `user_id`, `session_key`, `chat_id`, `chat_type`, `thread_id`, `display_name`, `origin_json` are all inherited verbatim.
- **Title Propagation**:
  - The parent session title is fetched: `old_title = agent._session_db.get_session_title(agent.session_id)` ([`conversation_compression.py:5121`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5121)).
  - Provenance is read: `_src = agent._session_db.get_session_title_source(old_session_id)` ([`conversation_compression.py:5326`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5326)).
  - Written to child: `agent._session_db.set_session_title(agent.session_id, old_title)` ([`conversation_compression.py:5334`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5334)).
  - Provenance restored: `agent._session_db.set_session_title_source(agent.session_id, _src)` ([`conversation_compression.py:5345`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5345)).
  - Invariant: titles do not renumber (`#2`, `#3`) across compression boundaries.
- **CLI Subsystem Migrations**:
  - `/goal` state: `migrate_goal_to_session(old_session_id, agent.session_id, reason="compression")` ([`conversation_compression.py:5290`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5290)).
  - `/heartbeat` state: `migrate_heartbeat_to_session(old_session_id, agent.session_id)` ([`conversation_compression.py:5296`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5296)).
  - `/loop` state: `migrate_loop_to_session(old_session_id, agent.session_id, reason="compression")` ([`conversation_compression.py:5304`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5304)).
- **Parent Label Cleanup**:
  - `_clear_labels(_labels_db, _old_sid)` ([`conversation_compression.py:5509`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5509)) removes terminal activity labels from the closed parent to avoid advertising stale activity.

### 11. Publication or Callback Failure Semantics

- **Pre-Publication / Summarization Abort**:
  - If the auxiliary summarizer fails, raises, or is cancelled by commit fence:
  - `compress_context` restores compressor attempt snapshot ([`conversation_compression.py:4316-4322`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L4316-L4322)).
  - `messages` is restored to `copy.deepcopy(messages_before_compression)`.
  - Telemetry recorded as `commit_status="aborted"`.
  - Compression lock released; original messages and existing prompt returned.
  - Session is not rotated.
- **Pre-Publish Precondition Failure**:
  - If `_parent_already_ended` is true with a deliberate end reason ([`conversation_compression.py:5079`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5079)), `RuntimeError` is raised before any write to the parent occurs.
- **Atomic Publication Failure Rollback**:
  - If `publish_compression_child` raises (e.g., SQLite lock error, lost lease, constraint violation):
  - Execution enters `except Exception as e:` ([`conversation_compression.py:5365-5480`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5365-L5480)).
  - Rollback condition is satisfied: `not in_place and locals().get("old_session_id") and agent.session_id == old_session_id`.
  - `old_session_id` is reset to `None` to indicate rollback.
  - `messages[:] = copy.deepcopy(messages_before_compression)` restores the in-memory transcript.
  - `compressed = messages` restores the return reference.
  - `_compression_made_progress = False`.
  - Proactive prune rearm runway is restored from `_compressor_attempt_snapshot`.
  - `split_status = "aborted"`.
  - Failure cooldown armed: `_record_compression_failure_cooldown(_SPLIT_FAILURE_COOLDOWN_SECONDS, f"session_split_failed: {e}")` ([`conversation_compression.py:5471`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5471)).
  - `_context_engine_boundary_committed` evaluates to `False` (because `_old_sid` was cleared to `None`).
  - Downstream boundary callbacks (context engine, memory manager, `session:compress` event) are completely suppressed.
  - `agent.session_id` remains the parent ID.
  - `conversation_history_after_compression` observes `attempt_in_place is None` and returns `previous_history` ([`conversation_compression.py:2876`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L2876)).
  - The conversation loop resumes on the parent session without losing conversational state.
- **Post-Publication Callback Failure Isolation**:
  - If any callback or state migration fails after `publish_compression_child` has successfully committed in SQLite:
  - `migrate_goal_to_session`, `migrate_heartbeat_to_session`, `migrate_loop_to_session`: wrapped in `try/except Exception`, logged at debug level ([`conversation_compression.py:5291-5306`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5291-L5306)).
  - `set_session_title`: wrapped in `try/except (ValueError, Exception)`, logged at debug level ([`conversation_compression.py:5337`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5337)).
  - `_notify_context_engine_compression_complete`: caught internally at lines 3299 and 3312; logged at debug level.
  - `agent._memory_manager.on_session_switch`: wrapped in `try/except Exception`, logged at debug level ([`conversation_compression.py:5551`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5551)).
  - `agent.event_callback("session:compress", ...)`: wrapped in `try/except Exception`, logged at debug level ([`conversation_compression.py:5583`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5583)).
  - Invariant: post-publication callback failures never abort or roll back a committed session rotation.

---

## Mid-Turn Invariants vs. Post-Turn Bookkeeping

The system strictly divides behavior that must commit before the next provider API request from behavior deferred to post-turn finalization:

### Requirements Prior to the Next Provider Call (Mid-Turn Contract)

1. **Tool Execution Completeness**: All tool calls in the batch must finish execution and their result messages must be in `messages`.
2. **Parent Closure & Child Publication**: The parent session in SQLite must contain the full current-turn transcript (including tool results) and be marked ended. The child session and compacted handoff must be committed in SQLite.
3. **Identity & Context Rebind**: `agent.session_id`, `gateway.session_context`, `os.environ["HERMES_SESSION_ID"]`, and `hermes_logging` must point to the child ID on both worker and caller threads.
4. **Flush Baseline Re-Anchoring**: `conversation_history` in `conversation_loop.py` must be cleared to `None` and all handoff dicts must carry `_DB_PERSISTED_MARKER = True`.
5. **Deduplication Reset**: File-read and skill-view deduplication caches must be advanced to their new generation.
6. **Token Usage Reset**: `last_prompt_tokens` must be set to `-1`, `awaiting_real_usage_after_compression` must be `True`, and prompt usage anchors must be invalidated.
7. **Prompt Rebuilding**: Dynamic tool definitions must be refreshed and system prompt rebuilt before building outgoing request payload.
8. **Wire Sanitization**: Wire payload `api_messages` must be cloned from `messages` with all internal markers, sidecars, and reasoning metadata stripped.

### Post-Turn Bookkeeping (Deferred to Turn Finalizer & Gateway)

1. **Scaffolding Removal**: Stripping trailing empty-response retry sentinels, thinking prefills, and synthetic verification nudges from durable memory.
2. **Final Answer Persistence**: Appending and committing the final assistant text turn to SQLite under the child session ID.
3. **Turn Completion Diagnostics**: Logging turn exit reason, token metrics, and tool execution counts.
4. **Advisory Footers**: Formatting file-mutation verification footers or turn completion explainers if abnormal exit occurred.
5. **Output Hooks**: Executing `transform_llm_output` and `post_llm_call` hooks.
6. **Observation Hooks**: Executing context engine `on_turn_complete` observation callback.
7. **External Memory Sync**: Calling `agent._sync_external_memory_for_turn(...)` and spawning background review agents if triggered.
8. **Session End Hook**: Calling `on_session_end` lifecycle hook.
9. **Turn Lease Termination**: Stopping turn lease refresher, deactivating liveness watchdog, and deleting lease from `session_turn_leases`.
10. **Gateway Store Sync**: Updating gateway `SessionEntry.session_id`, re-registering lease tracking, saving JSON store, updating peer mapping, and writing JSONL transcript with `history_offset = 0`.

---

## Existing Python Oracle Tests

The following tests in the repository establish the authoritative behavioral oracle for rotation-mode compression:

1. **Session Rotation & State Migration**:
   - [`tests/agent/test_compression_rotation_state.py:111`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L111): `TestGoalMigratesOnRotation.test_goal_follows_compression_rotation` (persistent `/goal` migrates to child).
   - [`tests/agent/test_compression_rotation_state.py:137`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L137): `TestOrphanRollbackOnCreateFailure.test_rolls_back_to_parent_when_child_create_fails` (publication failure rolls back in-memory transcript to parent without leaving orphan child).
   - [`tests/agent/test_compression_rotation_state.py:175`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L175): `TestWorkspaceMetadataFollowsRotation.test_child_row_inherits_cwd_repo_and_origin_on_rotation` (child session row inherits `cwd`, `git_branch`, `git_repo_root`, and gateway routing keys).
   - [`tests/agent/test_compression_rotation_state.py:1911`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L1911): `TestArchivedParentActivityLabelsCleared.test_archived_parent_activity_labels_cleared_on_rotation` (parent session activity labels cleared after rotation).
   - [`tests/agent/test_compression_rotation_state.py:1954`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L1954): `TestAbortedRotationDoesNotGrowParent.test_aborted_rotation_does_not_append_duplicate_rows_to_parent` (aborted rotation does not duplicate parent transcript).

2. **Deduplication and Marker Mechanics**:
   - [`tests/agent/test_compression_rotation_state.py:215`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L215): `TestRotationChildFlushDedup.test_summary_handoff_row_is_persisted_once_in_child`.
   - [`tests/agent/test_compression_rotation_state.py:248`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L248): `TestRotationChildFlushDedup.test_rotation_flush_of_original_live_list_keeps_user_once_when_handoff_already_contains_user`.
   - [`tests/agent/test_compression_rotation_state.py:294`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L294): `TestRotationChildFlushDedup.test_failed_publish_leaves_live_user_unmarked_for_later_flush`.
   - [`tests/agent/test_compression_rotation_state.py:336`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L336): `TestRotationChildFlushDedup.test_mid_tool_loop_rows_do_not_duplicate_after_failed_parent_flush`.
   - [`tests/agent/test_compression_rotation_state.py:395`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_rotation_state.py#L395): `TestRotationChildFlushDedup.test_mid_tool_loop_rows_do_not_duplicate_after_failed_parent_flush_direct_path`.
   - [`tests/run_agent/test_compression_persistence.py:278`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_compression_persistence.py#L278): `TestCompressionPersistence.test_rotation_child_session_flushes_full_compressed_transcript_with_markers` (`_db_persisted` marker propagation allows clean child flush).

3. **Transcript Cold Resume & Flush Boundaries**:
   - [`tests/agent/test_rotation_flush_persisted_boundary_68196.py:81`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_rotation_flush_persisted_boundary_68196.py#L81): `test_rotation_flush_does_not_duplicate_persisted_prefix` (parent flush respects durable boundary).
   - [`tests/agent/test_session_rotation_flush_cold_resume_68454.py:53`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_session_rotation_flush_cold_resume_68454.py#L53): `test_rotation_flush_without_history_boundary_is_safe`.

4. **Gateway History Offset**:
   - [`tests/run_agent/test_compression_persistence.py:338`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_compression_persistence.py#L338): `TestGatewayHistoryOffsetAfterSplit.test_history_offset_zero_on_session_split` (`history_offset = 0` on rotation).

5. **Atomic Lineage and Concurrency**:
   - [`tests/conformance/persistence/test_cell3_rotation_atomicity.py`](file:///home/eins0fx/development/hermes-agent-port/tests/conformance/persistence/test_cell3_rotation_atomicity.py): Atomic parent closure + child creation.
   - [`tests/test_compression_watermark_commit.py`](file:///home/eins0fx/development/hermes-agent-port/tests/test_compression_watermark_commit.py): Concurrent parent appends cloned into child via watermark.

---

## Missing Oracle Cases

The following edge cases exist in the production Python contract but lack explicit, end-to-end integration tests in the test suite:

1. **Multi-Iteration Tool Execution Post-Rotation Within Same Turn**:
   - *Scenario*: Model executes tool A; post-tool compaction triggers rotation to child session; loop continues in the same turn (`continue`); model receives compressed history, executes tool B; tool B result is flushed; model produces final text answer; final answer is flushed.
   - *Current Coverage Gap*: Tests verify isolated parts (`compress_context` or unit flushes), but do not assert that tool B and its assistant call are flushed exclusively to the child session while tool A remains exclusively in the parent session, without duplicating any handoff rows.

2. **Background Turn Lease Refresh Race During Mid-Turn Rotation**:
   - *Scenario*: Background thread runs `_refresh_durable_turn_lease` at the exact millisecond `agent.session_id` is updated from parent to child while `publish_compression_child` commits.
   - *Current Coverage Gap*: No test verifies concurrency between `_session_turn_lease_key_on_conn` walking the uncommitted/committed parent link and the background lease refresher.

3. **Tool Batch Large Stdout Combined with Concurrent Foreign Append**:
   - *Scenario*: A tool call generates large output committed during the pre-publish parent flush, while an external writer appends a concurrent row between `_commit_watermark` and `_foreign_tail_ceiling`.
   - *Current Coverage Gap*: Verifying that the cloned foreign tail in the child session captures the external append while excluding the agent's own large tool result.

4. **Context-Engine Callback Failure Post-Publication**:
   - *Scenario*: Plugin context engine raises an unhandled exception inside `on_session_start(boundary_reason="compression")` after `publish_compression_child` has already committed.
   - *Current Coverage Gap*: Verifying that `conversation_loop` swallows the error, preserves the child identity, and successfully delivers the next model completion without falling back to the parent.

5. **Reference Handoff Early Turn Exit Post-Tool Compaction**:
   - *Scenario*: Compression following a tool-result batch leaves only a reference-only handoff that makes the user turn non-actionable (`_should_skip_model_call_for_reference_handoff`).
   - *Current Coverage Gap*: Verifying that `final_response` is populated with `_HANDOFF_SKIP_FINAL_RESPONSE`, the loop terminates cleanly, and the child session finalizes with that terminal response.
