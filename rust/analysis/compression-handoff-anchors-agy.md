# Compression Handoff Anchors, Synthetic Rows, and Tail Invariants

## 1. Scope and Architectural Intent

This document maps the exact Python runtime behavior for context compression handoffs, boundary anchoring, synthetic user rows, todo snapshot coupling, strict role alternation, and restart suppression. It focuses on the concrete contracts implemented in [agent/context_compressor.py](../../agent/context_compressor.py), [agent/conversation_compression.py](../../agent/conversation_compression.py), [agent/conversation_loop.py](../../agent/conversation_loop.py), [agent/turn_context.py](../../agent/turn_context.py), and [agent/agent_runtime_helpers.py](../../agent/agent_runtime_helpers.py), alongside regression suites.

The analysis is strictly read-only and architectural. It establishes the decision order, metadata field lifecycle, mutation boundaries, persistence constraints, and safe seams for native Rust porting. Structural no-op backoffs and provider routing mechanisms are out of scope.

---

## 2. Core Architectural Mechanisms

### 2.1 Full-Compression Synthetic User Rows

#### Purpose and Problem Statement
Strict inference engines (OpenAI-compatible backends like vLLM and Qwen, as well as Anthropic and Bedrock) enforce hard constraints on user-role presence:
1. OpenAI-compatible APIs reject any request containing zero `role: "user"` turns with an unretryable `400 No user query found in messages` (#58753).
2. Anthropic and Bedrock require that the first visible message in `messages[]` must have `role: "user"` (#52160).
3. In autonomous sessions (such as `hermes kanban` workers or scheduled cron tasks), an initial user prompt may be compressed into the middle history or absent from `messages[]` entirely (when the system prompt is prepended separately). If unhandled, this leaves the post-compression transcript with zero user turns.

#### Taxonomy of Synthetic User Rows
Python distinguishes between genuine human intent and multiple classes of synthetic user rows:
1. **Compaction Summary Carrier (`role: "user"`)**:
   - Pinned to `role: "user"` when `_force_user_leading` triggers in [agent/context_compressor.py:8682-8687](../../agent/context_compressor.py#L8682-L8687).
   - Identified in memory by `_compressed_summary: True` (`COMPRESSED_SUMMARY_METADATA_KEY`), `_compressed_summary_has_user_turn: bool` (`COMPRESSED_SUMMARY_HAS_USER_TURN_KEY`), and text matching `SUMMARY_PREFIX` or `_is_context_summary_content`.
2. **Synthetic Continuation Fallback**:
   - String constant: `COMPRESSION_CONTINUATION_USER_CONTENT = "[Context compaction: please review the summary above and continue the task.]"` ([agent/context_compressor.py:270-272](../../agent/context_compressor.py#L270-L272)).
   - Legacy variant: `_LEGACY_COMPRESSION_CONTINUATION_USER_CONTENT = "[Context compaction: earlier conversation history has been summarized above. Please continue assisting the user based on the summary.]"` ([agent/context_compressor.py:274-277](../../agent/context_compressor.py#L274-L277)).
   - Appended by `_ensure_compressed_has_user_turn` when no human ask and no steer marker exist in the entire input history ([agent/conversation_compression.py:3250-3257](../../agent/conversation_compression.py#L3250-L3257)).
3. **In-Flight User Task Re-Statement (`_reappend_inflight_user_task`)**:
   - Header: `_INFLIGHT_TASK_REPLAY_HEADER = "[Compacted turn: the original task below remains active. Respond to this task; avoid repeating completed work:]"` ([agent/context_compressor.py:543-546](../../agent/context_compressor.py#L543-L546)).
   - When a single active instruction in the protected head precedes the summary (#100818), it is re-appended after the handoff. If the transcript ends on user, it is merged onto the summary carrier with `_inflight_replay_merged: True` ([agent/context_compressor.py:6964-6968](../../agent/context_compressor.py#L6964-L6968)).
4. **Runtime Recovery Nudges**:
   - `MAX_ITERATIONS_SUMMARY_REQUEST` ([agent/context_compressor.py:5988](../../agent/context_compressor.py#L5988)).
   - `_CODEX_INCOMPLETE_NUDGE`, `_CODEX_ACK_CONTINUATION_NUDGE`, `_DROPPED_TOOLCALL_NUDGE_CONTENT`, `_EMPTY_TOOL_RESPONSE_NUDGE`, length-continuation network stubs, and output limit stubs ([agent/context_compressor.py:5989-5994](../../agent/context_compressor.py#L5989-L5994)).
   - Tagged in memory with `_SYNTHETIC_USER_FLAGS = ("_todo_snapshot_synthetic", "_empty_recovery_synthetic", "_verification_stop_synthetic", "_pre_verify_synthetic", "_dropped_toolcall_nudge")` ([agent/conversation_compression.py:2904-2910](../../agent/conversation_compression.py#L2904-L2910)).
5. **Background Process Notifications**:
   - Prefix: `_BACKGROUND_PROCESS_NOTIFICATION_PREFIX = "[IMPORTANT: Background process "` ([agent/context_compressor.py:540](../../agent/context_compressor.py#L540)).
6. **Todo Snapshot Injections**:
   - Header: `TODO_INJECTION_HEADER = "[Your active task list was preserved across context compression]"` ([tools/todo_tool.py:34](../../tools/todo_tool.py#L34)).

#### Classification Predicates
- `ContextCompressor._is_synthetic_compression_user_turn(message)` ([agent/context_compressor.py:5955-6002](../../agent/context_compressor.py#L5955-L6002)): Authoritative recognizer surviving database round-trips. Checks metadata flags, summary prefixes, exact nudge constants, and prefix strings.
- `_is_real_user_message(message)` ([agent/conversation_compression.py:2913-2934](../../agent/conversation_compression.py#L2913-L2934)): Requires `role == "user"`, absence of `_SYNTHETIC_USER_FLAGS`, non-empty text, absence of `_SYNTHETIC_USER_PREFIXES`, and `not _is_synthetic_compression_user_turn(message)`.
- `ContextCompressor._transcript_has_real_user_turn(messages)` ([agent/context_compressor.py:5940-5952](../../agent/context_compressor.py#L5940-L5952)): Scans transcript for any non-synthetic user message.
- `_is_actionable_user_turn(message)` ([agent/context_compressor.py:6067-6085](../../agent/context_compressor.py#L6067-L6085)): Rejects messages with `display_kind` (e.g., `internal_notification`, `hidden`), summary metadata, summary content, or blank content.

---

## 2.2 Multi-User Tail Anchoring

#### Anchor Pipeline in `_find_tail_cut_by_tokens`
Tail cut calculation ([agent/context_compressor.py:7089-7245](../../agent/context_compressor.py#L7089-L7245)) follows a strict order:
1. **Token Budget Backward Accumulation**:
   - Token budget: `token_budget = self.tail_token_budget` (defaults to 20% of context window).
   - Ceiling: `soft_ceiling = int(token_budget * 1.5)`.
   - Minimum tail floor: `min_tail_floor = max(3, min(self.protect_last_n, _MAX_TAIL_MESSAGE_FLOOR))`, capped at `compressible_tail_cap = max(3, available_tail - 2)` ([agent/context_compressor.py:7117-7125](../../agent/context_compressor.py#L7117-L7125)).
   - Accumulates tokens backward from `len(messages) - 1` to `head_end`. Breaks if `accumulated + msg_tokens > soft_ceiling and (n - i) >= min_tail`.
2. **Whole-Transcript Soft Ceiling Re-Walk**:
   - If the entire transcript fits within `soft_ceiling`, re-walks with raw `token_budget` to ensure a non-empty compressible middle section is identified ([agent/context_compressor.py:7166-7188](../../agent/context_compressor.py#L7166-L7188)).
3. **Head Clamp and Initial Tool-Group Alignment**:
   - `fallback_cut = n - min_tail`; `cut_idx = min(cut_idx, fallback_cut)`.
   - If `cut_idx <= head_end`: `cut_idx = max(fallback_cut, head_end + 1)`.
   - Backward align: `cut_idx = self._align_boundary_backward(messages, cut_idx)` ([agent/context_compressor.py:7199](../../agent/context_compressor.py#L7199)).
4. **Anchor 1: Last Actionable User Turn (`_ensure_last_user_message_in_tail`)**:
   - Finds latest actionable, non-synthetic user turn via `_find_last_user_message_idx(messages, head_end)` ([agent/context_compressor.py:6616-6632](../../agent/context_compressor.py#L6616-L6632)).
   - If `last_user_idx >= cut_idx`, no-op.
   - If `last_user_idx < cut_idx`, pulls `cut_idx = last_user_idx`. Note: user rows are clean boundaries, so `_align_boundary_backward` is not called here.
   - **Causal Coupling Guard (#22523)**: If `adjusted = max(last_user_idx, head_end + 1) > last_user_idx` (i.e. user sits exactly at `head_end`), clamping backward would split the user from its assistant reply. Pushes forward to `pair_end = self._find_turn_pair_end(messages, last_user_idx)` ([agent/context_compressor.py:6800-6815](../../agent/context_compressor.py#L6800-L6815)).
5. **Anchor 2: Last Content-Bearing Assistant Reply (`_ensure_last_assistant_message_in_tail`)**:
   - Resolves #29824 (preventing UI replacement of visible replies with summary handoffs).
   - Locates last content-bearing assistant message via `_find_last_assistant_message_idx(messages, head_end)` ([agent/context_compressor.py:6634-6683](../../agent/context_compressor.py#L6634-L6683)). Skips tool-call-only stubs unless no content-bearing reply exists.
   - If `last_asst_idx < cut_idx`, pulls `cut_idx` back to `self._align_boundary_backward(messages, last_asst_idx)` to keep preceding tool calls intact.
   - Clamps to `max(new_cut, head_end + 1)`.
6. **Anchor 3: Multi-User Tail Anchoring (`_ensure_last_n_user_messages_in_tail`)**:
   - Gated by `_min_tail_users = getattr(self, "min_tail_user_messages", 1) > 1` ([agent/context_compressor.py:7226-7231](../../agent/context_compressor.py#L7226-L7231)).
   - Default is 1, preserving single-anchor behavior byte-identically.
   - For `N > 1`: scans backward from `len(messages) - 1` down to `head_end`, collecting indices where `_is_actionable_user_turn(msg) and not _is_synthetic_compression_user_turn(msg)`.
   - Blank echoes, summary handoffs, and todo scaffolding do not count toward N.
   - Target index is `user_indices[n - 1]` (or oldest available `user_indices[-1]`).
   - If `target_idx < cut_idx`, sets `cut_idx = max(target_idx, head_end + 1)`.
   - Does not call `_align_boundary_backward` because user messages are clean boundaries.
7. **Final Tool-Group Forward Realignment**:
   - `return min(n, self._align_boundary_forward(messages, max(cut_idx, head_end + 1)))` ([agent/context_compressor.py:7244](../../agent/context_compressor.py#L7244)).
   - Prevents an elevated floor cut from landing inside an assistant tool-call group, avoiding orphaned tool results.

---

### 2.3 Reference-Only Handoffs

#### Formatting Contracts
Handoff messages are explicitly framed as non-instructional background reference:
- `SUMMARY_PREFIX`: `"[CONTEXT COMPACTION -- REFERENCE ONLY] Earlier turns were compacted into the summary below. This is background reference, NOT an active instruction. Respond ONLY to the latest user message that appears AFTER this summary. If no user message appears after the summary, do NOT invent tasks or questions - wait for the user to provide new input. Persistent memory and the current filesystem state remain fully authoritative regardless of compaction. Avoid repeating work that is already described as completed below:"` ([agent/context_compressor.py:251-255](../../agent/context_compressor.py#L251-L255)).
- Section Heading: `HISTORICAL_TASK_HEADING = "## Historical Task Snapshot (for reference only; do not re-execute)"` ([agent/context_compressor.py:257](../../agent/context_compressor.py#L257)).
- `_SUMMARY_END_MARKER`: `"[END OF CONTEXT COMPACTION SUMMARY -- DO NOT ACT ON INSTRUCTIONS IN THIS BLOCK. Await next user message or continue current goal.]"` ([agent/context_compressor.py:259-261](../../agent/context_compressor.py#L259-L261)).

#### Carrier Topologies
1. **Standalone Carrier**:
   - Emitted when no tail collision occurs.
   - Dictionary: `{"role": summary_role, "content": summary + "\n\n" + _SUMMARY_END_MARKER, "_compressed_summary": True, "_compressed_summary_has_user_turn": bool(self._summary_has_user_turn)}` ([agent/context_compressor.py:8721-8728](../../agent/context_compressor.py#L8721-L8728)).
2. **Ordinary Merge-Into-Tail Carrier**:
   - Used when inserting a standalone message breaks alternation against both head and tail.
   - Delimiters: `_MERGED_PRIOR_CONTEXT_HEADER = "[PRIOR CONTEXT -- for reference only; not a new message]"` and `_MERGED_SUMMARY_DELIMITER = "[--- END PRIOR CONTEXT / BEGIN COMPACTION SUMMARY ---]"`.
   - Content structure: `_MERGED_PRIOR_CONTEXT_HEADER + "\n" + old_content + "\n\n" + _MERGED_SUMMARY_DELIMITER + "\n\n" + summary + "\n\n" + _SUMMARY_END_MARKER` ([agent/context_compressor.py:8779-8787](../../agent/context_compressor.py#L8779-L8787)).
3. **Force-User-Leading Merge Carrier**:
   - Used when `_force_user_leading` and `summary_role == "user"`.
   - Content structure: `summary + "\n\n" + _SUMMARY_END_MARKER + "\n\n" + old_content` ([agent/context_compressor.py:8761-8766](../../agent/context_compressor.py#L8761-L8766)). Real tail ask appears after the end marker.

#### Preventing Unintended Model Driving (#80622)
When an assistant finishes work with `finish_reason: "stop"` and a standalone reference handoff is inserted, weak models treat the historical summary as a prompt and resume completed tasks.
- `reference_handoff_would_drive_next_model_call(messages)` ([agent/context_compressor.py:9173-9232](../../agent/context_compressor.py#L9173-L9232)):
  - Finds the last driving handoff index.
  - Inspects all messages following that index: returns `False` if any `role: "tool"`, assistant with pending `tool_calls`, non-synthetic actionable user turn, or live-content-carrying summary follows.
  - Returns `True` if no subsequent active turn exists.
- In `conversation_loop.py` ([lines 277-289, 3189-3201, 7356-7366, 8300-8311](../../agent/conversation_loop.py#L277-L289)):
  - Calls `_should_skip_model_call_for_reference_handoff(messages, user_message)`.
  - Attempts restore via `_restore_user_after_reference_handoff(messages, user_message)` ([agent/conversation_loop.py:245-274](../../agent/conversation_loop.py#L245-L274)). If a real `user_message` was buffered, it is appended and execution proceeds.
  - If no restorable ask exists: skips provider call, sets `final_response = _HANDOFF_SKIP_FINAL_RESPONSE` (`"Context was compacted. The previous response is complete - awaiting your next message."`), sets `_turn_exit_reason = "compaction_handoff_not_actionable"`, refunds iteration budget, and ends the turn.

#### Projections and Views
- `is_user_originated_turn(message)` ([agent/context_compressor.py:9235-9246](../../agent/context_compressor.py#L9235-L9246)): Returns `True` only if the message is human-authored (not pure compaction scaffolding).
- `split_user_originated_turn(message)` ([agent/context_compressor.py:9018-9086](../../agent/context_compressor.py#L9018-L9086)): Splits composite user rows into `(handoff_only, live_view)`.
- `history_before_user_originated_turn(messages, index)` ([agent/context_compressor.py:9094-9112](../../agent/context_compressor.py#L9094-L9112)): Rewind support preserving hidden handoff scaffold while rolling back to live user turns.
- `reanchor_current_turn_user_idx(messages, user_message)` ([agent/turn_context.py:338-375](../../agent/turn_context.py#L338-L375)): Re-anchors current-turn user index after compaction rebuilds `messages`, preferring exact string match, then `user_originated_turn_view`, never landing on standalone handoffs.

---

### 2.4 Todo Snapshot Insertion and Retention Parity

#### Execution Seam in `compress_context`
Todo snapshot handling executes in `agent/conversation_compression.py:4626-4737` immediately after `agent.context_compressor.compress` returns:
1. **Extraction and Authority**:
   - `todo_snapshot = agent._todo_store.format_for_injection()` ([agent/conversation_compression.py:4626](../../agent/conversation_compression.py#L4626)).
   - Authority check: `_todo_store_is_authoritative = bool(agent._todo_store.has_items())` ([agent/conversation_compression.py:4634-4643](../../agent/conversation_compression.py#L4634-L4643)).
2. **Stale Snapshot Cleanup**:
   - If store is authoritative, scans backward for `role: "user"` carrying `TODO_INJECTION_HEADER`.
   - Uses `_strip_stale_todo_snapshot` ([agent/conversation_compression.py:3009-3045](../../agent/conversation_compression.py#L3009-L3045)).
   - If `_todo_message.get("_todo_snapshot_synthetic")` and `_todo_snapshot_is_only_content`, pops the message from `compressed` and calls `agent._repair_message_sequence(compressed)`.
   - If merged into user content, replaces content with stripped text and pops `_todo_snapshot_synthetic`.
3. **Pruned-Skill Reload Notice Coupling (#84718)**:
   - Header: `_PRUNED_SKILL_RELOAD_NOTICE_HEADER = "[Skills pruned during compression -- reload before acting on these tasks]"` ([agent/conversation_compression.py:3080-3082](../../agent/conversation_compression.py#L3080-L3082)).
   - Scans `compressed` for `[SKILL_PRUNED: <name>]` markers via `_pruned_skill_reload_notice` ([agent/conversation_compression.py:3085-3119](../../agent/conversation_compression.py#L3085-L3119)), capped at 5 skills.
   - Formats explicit instruction: `Before executing any preserved task that depends on these skills, reload them first: skill_view(name='...')...`
   - Appended to `todo_snapshot`: `todo_snapshot = f"{todo_snapshot}\n\n{_reload_notice}"`.
4. **Insertion and Merging Decisions**:
   - Evaluates `_tail = compressed[-1]`.
   - If `_tail.get("role") == "user"`:
     - Strips stale snapshot: `_stripped = _strip_stale_todo_snapshot(_tail.get("content"))`.
     - Probes `_is_real_user_message(_probe)`. If true, folds snapshot into tail content (`_tail["content"] += f"\n\n{todo_snapshot}"`).
     - If tail was an earlier standalone snapshot row with no other text, updates content in place and sets `_tail["_todo_snapshot_synthetic"] = True`.
   - If tail is not user (or is scaffolding), appends standalone:
     ```python
     compressed.append({
         "role": "user",
         "content": todo_snapshot,
         "_todo_snapshot_synthetic": True,
     })
     ```
5. **Post-Insertion User Anchor Guarantee**:
   - Calls `compressed_user_turn_outcome = _ensure_compressed_has_user_turn(messages, compressed)` ([agent/conversation_compression.py:4734](../../agent/conversation_compression.py#L4734)).
   - If only a standalone synthetic todo snapshot was appended, `_is_real_user_message` returns `False`.
   - `_insert_real_user_anchor` folds the human anchor from `messages` into the trailing todo scaffolding turn via `_merge_anchor_into_user_message` ([agent/conversation_compression.py:3121-3148, 3198-3203](../../agent/conversation_compression.py#L3121-L3148)), prepending the user text, preserving the todo content, and clearing `_SYNTHETIC_USER_FLAGS`.
6. **Salvage Reduction Contract**:
   - In `salvage_grown_transcript` ([agent/context_compressor.py:587-615, 631-634](../../agent/context_compressor.py#L587-L615)):
   - `_salvage_reduce_todo_snapshot`: Last resort shrink.
   - If the snapshot carries `_PRUNED_SKILL_RELOAD_NOTICE_HEADER`, it trims the todo list and retains only the reload notice. If no reload notice is present, it deletes the synthetic user row entirely.

---

### 2.5 Role Alternation and Strict Chat Template Guarantees

#### Template-Visible Roles vs Literal Roles
Mistral, Anthropic, Bedrock, and llama.cpp enforce strict `user -> assistant -> user` alternation at template evaluation time. However, tool flows are exempt:
- `_template_visible_role(message)` ([agent/context_compressor.py:360-386](../../agent/context_compressor.py#L360-L386)):
  - Returns `None` if `role == "tool"`.
  - Returns `None` if `role == "assistant"` and `message.get("tool_calls")`.
  - Returns literal `role` otherwise.
- `_last_template_visible_role(messages)` ([agent/context_compressor.py:389-401](../../agent/context_compressor.py#L389-L401)): Scans backward for the nearest non-exempt message.

#### Summary Role Selection Logic ([agent/context_compressor.py:8678-8708](../../agent/context_compressor.py#L8678-L8708))
1. Head role evaluation: `last_head_role = _last_template_visible_role(compressed)`.
2. Tail role evaluation: `first_tail_role = _template_visible_role(tail_messages[first_tail_visible_idx])`.
3. Initial assignment:
   - If `last_head_role is None` (head is purely tool flow), or `last_head_role in {"assistant", "tool"}`, or `_force_user_leading`:
     `summary_role = "user"`
   - Else: `summary_role = "assistant"`
4. Tail collision resolution:
   - If `first_tail_role is not None and summary_role == first_tail_role`:
     - Tentatively flip: `flipped = "assistant" if summary_role == "user" else "user"`.
     - If `flipped != last_head_role and last_head_role is not None and not _force_user_leading`:
       `summary_role = flipped` (role flip succeeds without colliding with head).
     - Else: Both roles collide (e.g. head is assistant, tail is user). Enable `_merge_summary_into_tail = bool(tail_messages)`.

#### Boundary Merge Mechanics ([agent/context_compressor.py:8745-8799](../../agent/context_compressor.py#L8745-L8799))
- If `_merge_summary_into_tail` is active:
  - Target index: 0 for normal alternation collisions; `first_tail_visible_idx` if `_force_user_leading`.
  - Prepend or append using `_append_text_to_content`.
  - Marks carrier with `_compressed_summary: True` and `_compressed_summary_has_user_turn: bool`.
  - Clears `api_content` via `drop_stale_api_content(msg)`.

#### Repair Sequence Invariant in `repair_message_sequence` ([agent/agent_runtime_helpers.py:893-942](../../agent/agent_runtime_helpers.py#L893-L942))
- Pass 3 merges consecutive user messages by default.
- **Exception for Compaction Carriers**:
  ```python
  from agent.context_compressor import split_user_originated_turn
  handoff, _ = split_user_originated_turn(prev)
  if handoff is not None:
      merged.append(msg)
      continue
  ```
  A summary carrier followed by a real user turn is preserved as two separate messages in durable storage. Absorbing the fresh ask into the persisted carrier dictionary would cause in-memory state to diverge from SQLite. Provider-specific serialization merges copies on the wire when necessary.

---

### 2.6 Restart and Restored-Handoff Suppression

#### Head Protection Decay Across Restarts
`protect_first_n` protects early user framing turns during the initial compaction. On subsequent cycles, it decays to 0 to prevent early turns from fossilizing into an immortal head:
- In `_effective_protect_first_n(messages)` ([agent/context_compressor.py:6528-6561](../../agent/context_compressor.py#L6528-L6561)):
  - If in-memory `compression_count >= 1` or `_previous_summary` is set: returns 0.
  - On process restart, `compression_count` resets to 0. It probes the restored transcript within `_restart_handoff_probe_bounds`:
    `first_non_system = 1 if messages[0].role == "system" else 0`
    `restart_probe_end = first_non_system + self.protect_first_n + _RESTART_HANDOFF_PROBE_EXTRA_MESSAGES` (_RESTART_HANDOFF_PROBE_EXTRA_MESSAGES = 4).
  - If any message in `messages[first_non_system:restart_probe_end]` satisfies `_is_context_summary_message`: returns 0.
  - Early protection is suppressed on restored sessions without requiring persisted counter state.

#### Stale Handoff Stripping Pipeline
To prevent handoff summaries from stacking unboundedly across repeated compactions:
1. **Full-Window Search**:
   - `summary_hits = self._find_context_summaries(messages, summary_search_start, len(messages))` ([agent/context_compressor.py:8234-8238](../../agent/context_compressor.py#L8234-L8238)).
   - Searches entire transcript to avoid missing handoffs beyond a degenerate `compress_end`.
2. **Rehydration into `_previous_summary`**:
   - Old summary bodies are extracted into `_previous_summary` for iterative summarization ([agent/context_compressor.py:8243-8246](../../agent/context_compressor.py#L8243-L8246)).
3. **Unwrapping or Dropping via `_strip_context_summary_handoff_message`**:
   - Applied to `turns_to_summarize` ([agent/context_compressor.py:8271-8290](../../agent/context_compressor.py#L8271-L8290)).
   - Applied to protected head messages `compressed[0:compress_start]` ([agent/context_compressor.py:8549](../../agent/context_compressor.py#L8549)).
   - Applied to preserved tail messages `tail_messages` ([agent/context_compressor.py:8592](../../agent/context_compressor.py#L8592)).
   - Standalone handoff rows strip to `None` and are dropped.
   - Merged handoffs unwrap to their genuine pre-delimiter user content, stripping summary text and popping `_compressed_summary`.
4. **Tail Cut Advance**:
   - If `summary_idx >= compress_end`, sets `tail_start = summary_idx + 1` ([agent/context_compressor.py:8296-8297](../../agent/context_compressor.py#L8296-L8297)).
   - In Phase 4, the tail loop starts at `max(compress_end, tail_start)`, preventing summaries beyond the cut from being duplicated into the tail.
5. **Zero-User Provenance Recovery**:
   - In `state.db`, internal metadata keys (`_compressed_summary_has_user_turn`) are stripped by SessionDB projection.
   - On resume, if metadata is absent, `ContextCompressor` inspects the summary body:
     `self._summary_has_user_turn = not (summary_body and _NO_USER_TASK_SENTINEL in summary_body)` ([agent/context_compressor.py:8256-8261](../../agent/context_compressor.py#L8256-L8261)).
   - `_NO_USER_TASK_SENTINEL = "None. No user-authored requests exist."`. If present, false provenance is recovered.
6. **Cross-Session Handoff Discard (#57835)**:
   - If `summary_hits` is empty but `_previous_summary` is set, `_previous_summary` was left over from a previous session (e.g. cron or prior `/new`).
   - Clears `self._previous_summary = None` ([agent/context_compressor.py:8298-8307](../../agent/context_compressor.py#L8298-L8307)), preventing cross-session contamination.

---

## 3. Decision Order and Mutation Boundaries

The end-to-end compression pipeline executes across two modules in 14 strict stages:

```
[Input Messages]
       │
       ▼
1. Phase 1: Tool Pruning (_prune_old_tool_results, deduplication, trailing blank echo removal)
       │
       ▼
2. Head & Tail Boundaries (_protect_head_size with restart probe decay, token budget backward walk)
       │
       ▼
3. Tail Anchoring (Anchor 1: user + causal coupling; Anchor 2: assistant; Anchor 3: multi-user N)
       │
       ▼
4. Boundary Alignment (_align_boundary_forward after floor clamp)
       │
       ▼
5. Summary Hit Scan (full-transcript search, rehydrate _previous_summary, unwrap merged turns)
       │
       ▼
6. Summary Generation (aux LLM call, validate zero-user provenance, fallback on failure)
       │
       ▼
7. Phase 4 Assembly (strip old handoffs from head/tail, determine template-visible roles)
       │
       ▼
8. Role Assignment & Collision Merge (select role, flip if colliding with tail, or merge into tail)
       │
       ▼
9. Tool Pair Sanitation & In-Flight Re-Append (_reappend_inflight_user_task)
       │
       ▼
10. Historical Media Stripping (_strip_historical_media)
       │
       ▼  (Return to compress_context)
11. Todo Snapshot Maintenance (strip stale snapshot if authoritative, append reload notice, merge/append)
       │
       ▼
12. Ensure User Turn Present (_ensure_compressed_has_user_turn: anchor real user or append placeholder)
       │
       ▼
13. Persistence Preparation & SQLite Publication (_strip_persistence_markers, archive_and_compact, stamp_db_persisted_markers)
       │
       ▼
14. Post-Compaction Turn Guard (_should_skip_model_call_for_reference_handoff in conversation loop)
```

### Stage Details and Invariants

| Stage | Owning Function / Lines | Input Mutated? | Core Invariant Enforced |
| :--- | :--- | :--- | :--- |
| 1. Phase 1 Pruning | `context_compressor.py:8080-8158` | In-place on transcript copy | Trailing blank user echoes removed if followed by assistant. Old tool results summarized. |
| 2. Head & Tail Bounds | `context_compressor.py:8160-8205` | Read-only | `protect_first_n` decays to 0 if handoff detected in `0..first_non_system+N+4`. |
| 3. Tail Anchoring | `context_compressor.py:7199-7231` | Read-only calculation of `cut_idx` | User anchor pulls cut back (pushes forward only on causal coupling). Assistant anchor pulls cut back. Multi-user N pulls cut back. |
| 4. Forward Alignment | `context_compressor.py:7244` | Read-only calculation of `cut_idx` | `cut_idx` advances past orphan tool results so parent assistant tool calls remain intact. |
| 5. Summary Hit Scan | `context_compressor.py:8234-8298` | Mutates `_previous_summary` | Full-window scan. Merged summaries unwrapped into `turns_to_summarize`. Provenance resolved. |
| 6. Summary Generation | `context_compressor.py:8419-8526` | Read-only generation | `_validate_summary_user_provenance` raises `RuntimeError` if zero-user session invents "User asked:". |
| 7. Phase 4 Assembly | `context_compressor.py:8528-8640` | Creates new `compressed` list | Stale handoffs stripped from head and tail. Template-visible roles resolved. |
| 8. Role Alternation | `context_compressor.py:8678-8799` | Mutates `compressed` / tail carrier | If both roles collide, merges into tail message. Drops `api_content`. Stamped with `_compressed_summary`. |
| 9. Tool Sanitization | `context_compressor.py:8809-8812` | Mutates list | In-flight user prompt re-appended if single ask was in protected head. Merges onto carrier if ending on user. |
| 10. Media Stripping | `context_compressor.py:8822` | Mutates list | Base64 images prior to newest image-bearing turn replaced with placeholders. |
| 11. Todo Snapshot | `conversation_compression.py:4626-4733` | Mutates `compressed` | Stale snapshot stripped. Pruned skill reload notice coupled. Folded into tail user or appended with `_todo_snapshot_synthetic`. |
| 12. User Turn Guarantee | `conversation_compression.py:4734, 3205-3258` | Mutates `compressed` | Real human ask anchored from `messages`. If none, placeholder appended. |
| 13. Persistence Commit | `conversation_compression.py:4800-5460` | Mutates message dict markers | `_strip_persistence_markers` runs before SQLite commit. `stamp_db_persisted_markers` runs after commit. |
| 14. Turn Guard | `conversation_loop.py:277-289, 8300-8311` | Can append restored user turn | If reference handoff would drive alone and no user restored, skips model call. |

---

## 4. Metadata Fields and Persistence Lifecycle

### Metadata Field Matrix

| Field Name | Type | Scope | Persistence in SQLite (`state.db`) | Purpose |
| :--- | :--- | :--- | :--- | :--- |
| `_compressed_summary` (`COMPRESSED_SUMMARY_METADATA_KEY`) | `bool` | In-memory message dict | Stripped by SessionDB projection. Rust gateway explicitly stores as boolean column `compressed_summary`. | Identifies message as compaction summary handoff. |
| `_compressed_summary_has_user_turn` (`COMPRESSED_SUMMARY_HAS_USER_TURN_KEY`) | `bool` | In-memory message dict | Stripped by SessionDB projection. In Rust, not in schema. | Retains zero-user provenance across compactions. |
| `_todo_snapshot_synthetic` | `bool` | In-memory message dict | Stripped by SessionDB projection. | Marks synthetic user row carrying todo list. |
| `_inflight_replay_merged` (`_INFLIGHT_REPLAY_MERGED_KEY`) | `bool` | In-memory message dict | Stripped by SessionDB projection. | Indicates in-flight task was merged onto summary carrier. |
| `_compaction_tail_carried` (`_COMPACTION_TAIL_MARKER`) | `bool` | In-memory message dict | Stripped by SessionDB projection. | Tags preserved tail rows for rewind classification. |
| `_db_persisted` (`_DB_PERSISTED_MARKER`) | `bool` | In-memory message dict | Stripped by `_strip_persistence_markers`; re-stamped after atomic commit. Never written to disk. | Prevents duplicate appends during unlocked flush. |
| `display_kind` | `str` | Message dict | Stored in SQLite `messages.display_kind`. | Values like `hidden`, `internal_notification`. Excludes row from actionable user turns. |
| `display_metadata` | `dict` | Message dict | Stored in SQLite `messages.display_metadata` JSON. | Filtered to durable keys (`reactions`) on projection. |
| `api_content` | `str` | Message dict | Stored in SQLite `messages.api_content`. | Dropped by `drop_stale_api_content` whenever content is rewritten. |
| `finish_reason` | `str` | Assistant message dict | Stored in SQLite `messages.finish_reason`. | Checked (`"stop"`) by `reference_handoff_would_drive_next_model_call`. |

### Persistence Invariants
1. **SessionDB Projection Stripping**: Python SessionDB projection (`hermes_state.py:SessionDB.get_messages_as_conversation`) strips all keys starting with an underscore (`_`). Therefore, all post-restart logic must either:
   - Rely on canonical text markers (`SUMMARY_PREFIX`, `TODO_INJECTION_HEADER`, `_NO_USER_TASK_SENTINEL`, `_MERGED_SUMMARY_DELIMITER`), or
   - Explicitly extend the database schema if state must survive without string parsing.
2. **Persistence Marker Sweep**: `_strip_persistence_markers` pops `_db_persisted` from every message before `archive_and_compact` to ensure newly inserted rows are recognized as fresh inserts by SQLite triggers. After commit, `stamp_db_persisted_markers` re-applies the flag to all instances in memory.

---

## 5. Ambiguities, Stale Behaviors, and Edge Cases

### 1. Divergence Between Rust and Python Tail Anchors
- In Python `context_compressor.py`, tail anchoring incorporates three distinct passes: single user anchor (with causal coupling), content-bearing assistant anchor (with backward tool alignment), and multi-user N anchoring (`min_tail_user_messages`).
- In Rust (`rust/crates/hermes-gateway/src/tool_result_prune.rs:400-520`), only single-user (`anchor_last_user`) and assistant (`anchor_last_assistant`) are implemented. `min_tail_user_messages` multi-user anchoring is completely absent.
- In Rust, `actionable_user` checks only `role == "user" && !is_summary_content && !text.is_empty()`. It does not check synthetic flags or prefixes (`TODO_INJECTION_HEADER`, `MAX_ITERATIONS_SUMMARY_REQUEST`, etc.). Consequently, a synthetic todo snapshot or retry nudge is currently mistaken for a human actionable user turn in Rust.

### 2. Double-Anchor Interaction with Causal Coupling
- In Python `_find_tail_cut_by_tokens`:
  `_ensure_last_user_message_in_tail` runs first. If the last user is at `head_end`, Causal Coupling pushes the cut *forward* to `pair_end`.
  Then `_ensure_last_assistant_message_in_tail` runs, which can pull `cut_idx` *backward* again.
  Then `_ensure_last_n_user_messages_in_tail` runs.
  Notice that `_ensure_last_n_user_messages_in_tail` does not implement Causal Coupling; it simply does `max(cut_idx, head_end + 1)`.
  Furthermore, `_min_tail_users > 1` is explicitly guarded at line 7227 because running `_ensure_last_user_message_in_tail` a second time when `N=1` would re-trigger causal coupling after the assistant anchor moved the cut.

### 3. Assistant Anchor Fallback to Tool-Call-Only Turns
- `_find_last_assistant_message_idx` searches for content-bearing assistant replies. If none has text, it falls back to `last_any` (the latest assistant turn even if it only has `tool_calls`).
- While this fallback pulls `cut_idx` before the tool-calling turn (and re-aligns backward), in a fresh session with only tool calls, anchoring on `last_any` protects the entire ongoing tool chain.

### 4. Zero-User Provenance Heuristic for Legacy Handoffs
- In `context_compressor.py:8255-8261`, if `_compressed_summary_has_user_turn` is missing (due to database projection), the compressor defaults to `self._summary_has_user_turn = not (summary_body and _NO_USER_TASK_SENTINEL in summary_body)`.
- If an older summary was generated without the exact sentinel string `_NO_USER_TASK_SENTINEL = "None. No user-authored requests exist."`, it assumes `has_user_turn = True`. This fail-safe defaults toward assuming a human user existed, avoiding accidental deletion of user attribution.

### 5. In-Flight Re-Append Header Stacking
- In `_reappend_inflight_user_task` ([agent/context_compressor.py:6920-6925](../../agent/context_compressor.py#L6920-L6925)), if a task survives across multiple compaction cycles, `task_text.rsplit(_INFLIGHT_TASK_REPLAY_HEADER, 1)[1].strip()` strips prior headers to prevent exponential header stacking.

---

## 6. Safe Rust Seams and Data Model Recommendations

To port these contracts into the Rust codebase cleanly without destabilizing existing gateway operations, the following seams are recommended:

### Seam 1: Transcript History Message Model Extension
In `rust/crates/hermes-gateway/src/session_db.rs`:
Extend `CompressionHistoryMessage` or add typed accessors to capture synthetic metadata:
```rust
pub struct CompressionHistoryMessage {
    pub id: i64,
    pub message: HistoryMessage,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_name: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_content: Option<String>,
    pub reasoning_details: Option<String>,
    pub codex_reasoning_items: Option<String>,
    pub codex_message_items: Option<String>,
    pub compressed_summary: bool,
    // Recommended additions for full parity:
    pub compressed_summary_has_user_turn: Option<bool>,
    pub todo_snapshot_synthetic: bool,
    pub display_kind: Option<String>,
    pub finish_reason: Option<String>,
}
```
*Note*: For backward compatibility with database reads, fields absent from SQLite can be populated via content sniffing (`is_synthetic_compression_user_turn`, `has_no_user_sentinel`).

### Seam 2: Synthetic User Classification Module
Create a pure classification submodule (e.g. `rust/crates/hermes-gateway/src/synthetic_user.rs`):
- `is_synthetic_compression_user_turn(msg: &CompressionHistoryMessage) -> bool`
- `is_real_user_message(msg: &CompressionHistoryMessage) -> bool`
- `is_actionable_user_turn(msg: &CompressionHistoryMessage) -> bool`
- `is_blank_user_turn(msg: &CompressionHistoryMessage) -> bool`
- Implement exact prefix and content matching for `TODO_INJECTION_HEADER`, `MAX_ITERATIONS_SUMMARY_REQUEST`, `_BACKGROUND_PROCESS_NOTIFICATION_PREFIX`, and retry recovery nudges.

### Seam 3: Pure Multi-User Tail Anchoring Function
Refactor tail cut calculation in `rust/crates/hermes-gateway/src/tool_result_prune.rs` (or a dedicated `tail_anchoring.rs`):
- Wire `min_tail_user_messages` from `AutomaticCompressionPolicy`.
- Update `anchor_last_user` to use `is_actionable_user_turn && !is_synthetic_compression_user_turn`.
- Implement `anchor_last_n_user_messages(messages: &[CompressionHistoryMessage], cut: usize, head_end: usize, n: usize) -> usize`.
- Chain `anchor_last_user` -> `anchor_last_assistant` -> `anchor_last_n_user_messages` -> `align_tool_boundary_forward`.

### Seam 4: Reference Handoff Driving Predicate
Implement `reference_handoff_would_drive_next_model_call`:
- Signature: `pub fn reference_handoff_would_drive_next_model_call(messages: &[CompressionHistoryMessage]) -> bool`
- In `rust/crates/hermes-gateway/src/message.rs` (or tool loop):
  Before invoking the model after a post-tool compaction, evaluate the predicate. If true and no new user prompt was provided, terminate the turn with the standard handoff skip notice and refund iteration units.

### Seam 5: Todo Snapshot Maintenance and Skill Reload Coupling
Implement pure transformation helpers:
- `strip_stale_todo_snapshot(content: &str) -> String`
- `pruned_skill_reload_notice(messages: &[CompressionHistoryMessage]) -> Option<String>`
- `merge_or_append_todo_snapshot(messages: &mut Vec<CompressionHistoryMessage>, snapshot: &str)`
- Execute this immediately prior to SQLite publication in the native full-compression pipeline.

---

## 7. Concrete Verification Test Matrix

The following test matrix provides regression scenarios corresponding directly to the verified Python behavior and tests:

| Test Case Identifier | Target Behavior | Python Reference Test | Rust Verification Strategy |
| :--- | :--- | :--- | :--- |
| `TC-ANCHOR-01` | Single user anchor with Causal Coupling forward push | `tests/agent/test_compressor_actionable_tail_anchor.py:131` | Set user at `head_end`. Assert cut advances to `pair_end` so user + assistant + tools stay together. |
| `TC-ANCHOR-02` | Multi-user N anchoring (`min_tail_user_messages=3`) | `tests/agent/test_context_compressor.py:3245` | Set 3 user turns separated by bulky assistant/tool turns. Assert all 3 user turns survive in tail regardless of tight budget. |
| `TC-ANCHOR-03` | Multi-user N anchoring ignores blank echoes and synthetic rows | `tests/agent/test_context_compressor.py:3337` | Insert empty user rows and todo snapshots between user turns. Assert N counts only real human turns. |
| `TC-ANCHOR-04` | Assistant reply tail anchor (#29824) | `tests/agent/test_compressor_assistant_tail_anchor.py:76` | Add visible assistant reply followed by multiple tool calls. Assert cut pulls back to assistant reply and keeps tool calls intact. |
| `TC-ANCHOR-05` | Assistant tail anchor skips tool-call-only stubs | `tests/agent/test_compressor_assistant_tail_anchor.py:77` | Place assistant message with `tool_calls` but `content: None`. Assert anchor prefers older text-bearing assistant reply. |
| `TC-SYNTH-01` | Zero-user session pins summary to `role: "user"` | `tests/agent/test_compressor_zero_user_guard.py:81` | Compact session containing only assistant and tool turns. Assert summary role is `"user"` and at least one user turn exists. |
| `TC-SYNTH-02` | Image-only user turn does not satisfy non-empty text check | `tests/agent/test_compressor_zero_user_guard.py:184` | Provide sole surviving user turn with image content parts only. Assert summary role forces `"user"` to provide non-empty query text. |
| `TC-SYNTH-03` | Zero-user provenance sentinel preservation | `tests/agent/test_context_compressor_zero_user_provenance.py:152` | Simulate round-trip through SQLite without metadata flags. Assert `_NO_USER_TASK_SENTINEL` recovers `has_user_turn = false`. |
| `TC-SYNTH-04` | Fabricated user ask rejection during summary generation | `tests/agent/test_context_compressor_zero_user_provenance.py:131` | Mock LLM producing `"User asked: 'foo'"` on a zero-user session. Assert `_validate_summary_user_provenance` rejects the output. |
| `TC-SYNTH-05` | Runtime recovery nudges recognized as synthetic | `tests/agent/test_context_compressor_zero_user_provenance.py:287` | Feed length continuation stubs, codex nudges, and process notifications. Assert `is_synthetic_compression_user_turn` returns true. |
| `TC-HANDOFF-01` | Standalone reference handoff alone does not drive next model call | `tests/agent/test_reference_handoff_active_turn.py:82` | Transcript ends with standalone handoff after assistant stop. Assert `reference_handoff_would_drive_next_model_call` is true. |
| `TC-HANDOFF-02` | Live tool call on merged carrier keeps exchange in-flight | `tests/agent/test_reference_handoff_active_turn.py:127` | Merged assistant carrier has `finish_reason: "tool_calls"`. Assert `reference_handoff_would_drive_next_model_call` is false. |
| `TC-HANDOFF-03` | Fresh user turn after handoff allows execution | `tests/agent/test_reference_handoff_active_turn.py:99` | Real user prompt follows handoff. Assert `reference_handoff_would_drive_next_model_call` is false. |
| `TC-HANDOFF-04` | Restore user after reference handoff | `tests/agent/test_reference_handoff_active_turn.py:209` | Guard receives buffered `user_message`. Assert user is appended after handoff and model call proceeds. |
| `TC-TODO-01` | Todo snapshot folded into trailing user message | `tests/agent/test_skill_todo_retention_parity.py:252` | Post-compaction tail ends in user. Assert snapshot text is appended and no adjacent user/user rows are created. |
| `TC-TODO-02` | Stale todo snapshot stripped on second compaction | `tests/agent/test_context_compressor_zero_user_provenance.py:384` | Run two consecutive compactions with active todo store. Assert earlier snapshot header is stripped, not duplicated. |
| `TC-TODO-03` | Pruned skill reload notice coupled to todo snapshot | `tests/agent/test_skill_todo_retention_parity.py:252` | Compact transcript containing `[SKILL_PRUNED: git]`. Assert todo snapshot contains `_PRUNED_SKILL_RELOAD_NOTICE_HEADER` and `skill_view`. |
| `TC-TODO-04` | Salvage reduction retains reload notice | `tests/agent/test_salvage_grown_transcript.py:85` | Trigger `salvage_grown_transcript` on candidate with reload notice. Assert todo tasks are dropped but reload notice survives. |
| `TC-ROLES-01` | Head=assistant and tail=user forces summary merge into tail | `tests/agent/test_compressor_zero_user_guard.py:109` | Both roles create alternation collisions. Assert summary is merged into tail message using `_MERGED_SUMMARY_DELIMITER`. |
| `TC-ROLES-02` | Carrier user message does not merge with subsequent user turn | `tests/agent/test_reference_handoff_active_turn.py:365` | In `repair_message_sequence`, pass 3 encounters carrier followed by new user. Assert carrier dict is not mutated in place. |
| `TC-RESTART-01` | Head protection decays to 0 on restored session | `tests/agent/test_compressor_zero_user_guard.py:81` | Simulate process restart where head contains existing summary. Assert `_effective_protect_first_n` returns 0. |
| `TC-RESTART-02` | Stale handoff stripped across entire window | `tests/agent/test_context_compressor_zero_user_provenance.py:152` | Multiple earlier handoffs present. Assert all earlier handoff banners are stripped or unwrapped, emitting exactly one summary. |
