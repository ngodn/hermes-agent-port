# Same-Turn Full Compression Runtime Contract

## Scope and Intent

This report traces Python's post-tool full LLM summary compression that executes after a completed tool-result batch and before the next provider request. It separates verified Python runtime behavior from Rust port recommendations. It omits proactive pruning, micro-compaction, auxiliary route configuration, and general pre-turn compression except where required to distinguish the post-tool execution path.

---

## Verified Python Behavior

### 1. Trigger Input and Sizing Precedence

Mid-turn post-tool compression executes in `agent/conversation_loop.py` lines 8212 to 8312, immediately following batch tool execution in `agent._execute_tool_calls` (`agent/conversation_loop.py:8158`).

#### Token Measurement Precedence
`agent/conversation_loop.py:8226-8259` resolves context pressure `_real_tokens` via three strict branches:

1. Provider Prompt Tokens (`_compressor.last_prompt_tokens > 0`):
   `_real_tokens = _compressor.last_prompt_tokens` (`agent/conversation_loop.py:8233`).
   Only `prompt_tokens` is read. Completion and reasoning tokens are ignored because reasoning tokens from thinking models (such as DeepSeek R1, GLM-5.1, or QwQ) inflate `completion_tokens` without consuming context window capacity (`agent/conversation_loop.py:8228-8232`).
2. Post-Compression Zero Floor (`_compressor.last_prompt_tokens == -1`):
   `_real_tokens = 0` (`agent/conversation_loop.py:8238`).
   When compression commits in `agent/conversation_compression.py:5601`, `last_prompt_tokens` is stamped with `-1`. This sentinel forces `_real_tokens = 0` so that schema-heavy rough token estimates do not trigger immediate repeat compression before the provider has reported actual post-compaction usage (`agent/conversation_loop.py:8234-8237`).
3. Rough Request Sizing Fallback (`_compressor.last_prompt_tokens == 0`):
   `_real_tokens = _midturn_request_pressure_tokens(agent, messages, active_system_prompt or "", estimate_request_tokens_rough(messages, tools=agent.tools or None))` (`agent/conversation_loop.py:8251-8258`).
   Used when `last_prompt_tokens` is 0 (e.g., following an API disconnect or a gateway restart where provider usage is unavailable). The estimate includes tool definitions from `agent.tools` because 50+ tool schemas contribute 20k to 30k tokens that a message-only estimate omits (`agent/conversation_loop.py:8240-8250`).

#### Trigger Gate
Compression fires only when all three conditions in `agent/conversation_loop.py:8260-8264` hold:
```python
agent.compression_enabled
and compression_attempts < max_compression_attempts
and _compressor.should_compress(_real_tokens)
```
In `agent/context_compressor.py:3906-3953`, `should_compress` delegates to `should_compress_info`:
- Returns `False` if `tokens < self.threshold_tokens`.
- Returns `False` if `self._automatic_compression_blocked()` is true.
- Returns `True` otherwise.

### 2. Attempt Accounting, Cooldowns, Lock Handling, and Outcome Semantics

#### Attempt Accounting and Rearm
- Increment: `compression_attempts += 1` at `agent/conversation_loop.py:8265`.
- Per-Turn Cap: `max_compression_attempts = int(getattr(agent, "max_compression_attempts", 3) or 3)` (`agent/conversation_loop.py:2275`).
- Lock-Skip Refund: If compression returns the original message list unchanged and `compression_skipped_due_to_lock(agent)` is true, `compression_attempts -= 1` (`agent/conversation_loop.py:8285-8295`). Contention on the session lock is a transient deferral, not a sign of incompressibility; refunding prevents burning the attempt budget toward premature session termination.
- Provider Usage Rearm: In `agent/conversation_loop.py:4728-4742`, when the next provider response reports `prompt_tokens`, `_should_rearm_compression_budget` (`agent/conversation_loop.py:320-339`) checks whether `completed_compaction_pending` is true and `0 < prompt_tokens < threshold_tokens`. If confirmed by the provider, `compression_attempts = 0`.

#### Cooldown and Breaker Gates
Evaluated both before lock acquisition (`agent/conversation_compression.py:3502-3517`) and under the acquired session lease (`agent/conversation_compression.py:3971-3991`):
1. Summary Failure Cooldown: `_compression_failure_cooldown_until > time.time()`. Triggered for 600 seconds on LLM summary error or SQLite split failure (`agent/conversation_compression.py:5471-5474`).
2. Ineffective Compression Breaker: `_ineffective_compression_count >= 2`. On the second strike (two consecutive passes saving < 10%), an anti-thrashing deadline is armed for 300 seconds (`_ineffective_recovery_deadline`). Before the deadline, compression is skipped. At or after the deadline, a single probe attempt is admitted.
3. Structural No-Op Backoff: `_structural_no_op_backoff_until > time.time()`. Armed when transcript cannot shrink (e.g., too few messages or no compressible window) to prevent re-firing on identical history.

#### Fail-Open Versus Fail-Closed Outcomes
- SessionDB Lock Availability: Structural absence of `try_acquire_compression_lock` on legacy or test-double `_lock_db` objects fails open (`_legacy_session_db_without_lock_api`, `agent/conversation_compression.py:3684-3698`) to prevent software version skew from wedging auto-compression. However, an unexpected exception from an existing lock method fails closed (`agent/conversation_compression.py:3759-3780`), skipping compression for that cycle without corrupting lineage.
- Summary Generation: Terminal access/quota errors (401, 402, 403), transient network disconnects, truncated outputs, or HTTP 200 empty responses fail closed (`agent/context_compressor.py:8453-8526`). Compression aborts, `_last_compress_aborted = True` is set, and the input transcript is returned completely unmodified.
- Anti-Growth Guard: If post-summary tokens exceed pre-summary tokens (`_rough_out > _rough_in`, `agent/conversation_compression.py:4836-4929`), `salvage_grown_transcript` attempts recovery. If still larger, compression is refused, an ineffective compaction strike is recorded, and the original transcript is preserved.
- Transaction Abort: If SQLite publication raises in `archive_and_compact`, in-memory messages are rolled back to `messages_before_compression` (`agent/conversation_compression.py:5404-5447`), and a 600-second cooldown is recorded.

### 3. Transcript State Passed to Compression

At the moment `agent._compress_context(messages, system_message, approx_tokens=_real_tokens, task_id=effective_task_id)` is invoked (`agent/conversation_loop.py:8280-8284`):
- All prior conversation turns are present in `messages`.
- The current turn user message is present.
- The assistant tool-calling message with `tool_calls` is present.
- Every tool result dictionary corresponding to each `tool_call_id` is present.
- Tool batch persistence has already executed: `_flush_session_db_after_tool_progress` in `agent/tool_executor.py:211-240` invoked `agent._flush_messages_to_session_db(messages)` before tool completion was projected to any interface.
- Every message dictionary in `messages` already has `_DB_PERSISTED_MARKER = True` stamped by the flush loop (`run_agent.py:2475-2482`).
- The base `system_message` is not inside `messages`; it is passed as an independent argument. In `run_agent.py:8708-8731`, a deep copy of `messages` is handed to the compression worker to isolate the caller's live transcript during summary generation.

### 4. Phase-One Pruning, Boundaries, Summary, and In-Place Publication

`ContextCompressor.compress` (`agent/context_compressor.py:8038-8913`) executes a four-phase pipeline:

#### Phase 1: Deterministic Tool-Result Pruning
- `_prune_old_tool_results` (`agent/context_compressor.py:4146-4243`) replaces bulky tool results outside the protected tail with concise single-line summaries (e.g., `[terminal] ran ... -> exit 0`).
- Deduplicates identical file/tool outputs across the history.
- Strips trailing blank user echoes (`agent/context_compressor.py:8149-8158`).
- Pruning changes persist even if Phase 3 summary generation subsequently aborts.

#### Phase 2: Protected Head and Tail Boundaries
- Head boundary: `compress_start = self._protect_head_size(messages)` (`agent/context_compressor.py:6563-6586`). Protects system message (if present) plus `_effective_protect_first_n(messages)`. `_effective_protect_first_n` decays to 0 once `compression_count >= 1` or an existing summary handoff is detected (`agent/context_compressor.py:6528-6561`).
- Forward boundary alignment: `_align_boundary_forward(messages, compress_start)` (`agent/context_compressor.py:6503-6512`) advances `compress_start` past any `role == "tool"` rows to avoid starting mid-group.
- Tail cut: `compress_end = self._find_tail_cut_by_tokens(messages, compress_start)` (`agent/context_compressor.py:7089-7245`). Accumulates tokens backward from the tail up to `soft_ceiling = int(token_budget * 1.5)`. Enforces `min_tail` floor.
- Tail anchors and group alignment:
  - `_align_boundary_backward(messages, cut_idx)` (`agent/context_compressor.py:6588-6610`) pulls `cut_idx` before any parent assistant message whose tool results span the boundary.
  - `_ensure_last_user_message_in_tail` (`agent/context_compressor.py:6743-6815`) anchors the most recent actionable user request in the tail.
  - `_ensure_last_assistant_message_in_tail` (`agent/context_compressor.py:6685-6742`) anchors the most recent visible assistant reply in the tail.
  - `_align_boundary_forward` re-aligns if the minimum tail floor bumped `cut_idx` into a tool group.
- If `compress_start >= compress_end`, the middle window is empty. It records a structural no-op backoff and returns `messages` unchanged (`agent/context_compressor.py:8182-8201`).

#### Phase 3: Summary Generation
- Slice: `turns_to_summarize = messages[compress_start:compress_end]`.
- Existing summaries: If older summary handoffs are detected (`agent/context_compressor.py:8234-8298`), they are stripped from the slice and rehydrated into `_previous_summary` for iterative updating.
- Pre-LLM feasibility skip: If `_ineffective_compression_count >= 1` and middle tokens are below 30% of the threshold (`_FEASIBILITY_SKIP_MIDDLE_FRACTION = 0.30`, `agent/context_compressor.py:8384-8411`), LLM summarization is skipped and deterministic fallback dropping is used.
- LLM call: `_generate_summary` (`agent/context_compressor.py:8419-8424`) calls the auxiliary provider model.
- Abort handling: If generation fails, it aborts if `abort_on_summary_failure` is true or if error was auth/network/truncated/empty (`agent/context_compressor.py:8453-8526`). Otherwise it falls back to `_build_static_fallback_summary`.

#### Phase 4: Assembly and In-Place Publication
- Assembles: protected head + summary message + preserved tail.
- Cleans tool pairs via `_sanitize_tool_pairs` (`agent/context_compressor.py:6392-6470`), protecting in-flight tool chains (`agent/context_compressor.py:6439-6470`).
- Restates in-flight user tasks via `_reappend_inflight_user_task` (`agent/context_compressor.py:6871-6974`).
- Strips historical images before the latest image turn (`_strip_historical_media`, `agent/context_compressor.py:8822`).
- Strips existing persistence markers via `_strip_persistence_markers(compressed)` (`agent/context_compressor.py:8860`).
- In `agent/conversation_compression.py:4963-5000`, `archive_and_compact` executes in SQLite:
  - Soft-archives pre-compaction rows (`active = 0, compacted = 1`).
  - Writes `compressed` as active rows (`active = 1, compacted = 0`) under the same `session_id`.
  - Stamps live returned message dicts with `stamp_db_persisted_markers(compressed)` (`agent/conversation_compression.py:4989`).
  - Resets `agent._flushed_db_message_ids = set()`.
  - Updates system prompt in SQLite: `agent._session_db.update_system_prompt(agent.session_id, new_system_prompt)`.
  - Sets `compacted_in_place = True`, `_last_flushed_db_idx = 0`.
  - Sets `_compressor.last_prompt_tokens = -1` and `awaiting_real_usage_after_compression = True`.

### 5. Live Loop Transcript Adoption Without Duplicate Persistence

After `_compress_context` returns:
- Live message reassignment: `messages, active_system_prompt = agent._compress_context(...)` (`agent/conversation_loop.py:8280-8284`).
- Flush baseline re-anchoring:
  `conversation_history = conversation_history_after_compression(agent, messages, conversation_history)` (`agent/conversation_loop.py:8297-8299`).
- `conversation_history_after_compression` (`agent/conversation_compression.py:2843-2879`):
  When `attempt_in_place` or `_last_compaction_in_place` is true, returns `list(messages)` (a shallow copy of the compacted list).
- Duplicate Prevention:
  `_flush_messages_to_session_db_unlocked` (`run_agent.py:2431-2482`) checks:
  1. `msg.get(_DB_PERSISTED_MARKER)`: stamped on every compacted message by `stamp_db_persisted_markers`.
  2. `id(msg) in history_ids`: captured from `conversation_history`.
  Because both checks skip every compacted message, the subsequent turn flushes only genuine new turn appends. Compacted messages are never re-inserted.
- Live Session Mirror: `agent._session_messages = messages` (`agent/conversation_loop.py:8383`).

### 6. Role Alternation, Tool Groups, System Prompt, Prompt Cache, and Handoff

#### Role Alternation
- In `agent/context_compressor.py:8596-8709`, `last_head_role` and `first_tail_role` are computed using `_template_visible_role`, ignoring tool results and assistant tool-call messages.
- Pin to `"user"`: If `compress_start == 0`, `last_head_role == "system"`, or no non-empty user message survives in head or tail (`_force_user_leading`), summary role is pinned to `"user"`.
- Alternation selection: If `last_head_role` is `"assistant"` or `"tool"`, summary is `"user"`. If `"user"`, summary is `"assistant"`.
- Collision handling: If chosen role matches `first_tail_role`, it attempts to flip to the opposite role. If flipping causes collision with `last_head_role`, `_merge_summary_into_tail = True` merges the summary into the first tail message (`_MERGED_PRIOR_CONTEXT_HEADER`, `_MERGED_SUMMARY_DELIMITER`, `_SUMMARY_END_MARKER`).
- Standalone summaries receive `_SUMMARY_END_MARKER` and metadata flags `_compressed_summary = True` and `_compressed_summary_has_user_turn`.

#### Tool Group Integrity
- Assistant `tool_calls` and corresponding `tool` results are never separated by compression cuts.
- `_align_boundary_forward` slides the head boundary past orphan tool results.
- `_align_boundary_backward` pulls the tail cut before the assistant tool caller if cut falls within tool results.
- `_sanitize_tool_pairs` removes unmatched tool results and strips orphaned tool calls, while exempting trailing in-flight calls (`agent/context_compressor.py:6439-6470`).

#### Immutable System Prompt and Prompt Cache
- At the admitted commit boundary (`agent/conversation_compression.py:4738-4794`), `rebuilt_system_prompt = agent._build_system_prompt(system_message)` runs.
- If byte-identical to `cached_system_prompt`: retains cached prompt identity and calls `reconstruct_static_prefix(agent, system_message=system_message, log_label="compression keep-prompt")` (`agent/system_prompt.py:1088-1130`).
- If content drifted: replaces `_cached_system_prompt = new_system_prompt` and updates SQLite via `update_system_prompt`.
- Compaction inevitably breaks the prompt cache prefix because history is replaced with a summary, but preserving byte-level equality on unchanged prompts allows downstream provider turns to resume KV cache hits.

#### Reference-Only Handoff Suppression
- In `agent/conversation_loop.py:8300-8311`:
  `_should_skip_model_call_for_reference_handoff(messages, user_message)` calls `reference_handoff_would_drive_next_model_call(messages)` (`agent/context_compressor.py:9173-9233`).
- If a reference-only summary handoff would be the sole active user turn driving the next model call after an assistant response has finished, and `_restore_user_after_reference_handoff` (`agent/conversation_loop.py:245-275`) cannot restore a real user prompt:
  The next model call is skipped.
  `final_response` is set to `_HANDOFF_SKIP_FINAL_RESPONSE` ("Context was compacted. The previous response is complete; awaiting your next message.").
  `_turn_exit_reason = "compaction_handoff_not_actionable"`.
  The tool loop breaks immediately.

### 7. Ordering Relative to Provider Request and Activity State

The exact post-tool event sequence:
1. Tool Batch Execution: `_execute_tool_calls` runs all calls.
2. Incremental Tool Persistence: `_flush_session_db_after_tool_progress` persists tool results to SQLite.
3. Compression Decision: Context pressure `_real_tokens` evaluated against `threshold_tokens`.
4. Full Compression: `_compress_context` runs, acquires durable lock, prunes, summarizes, and commits `archive_and_compact`.
5. State Adoption: `conversation_history` updated to shallow copy; `_session_messages` updated; `last_prompt_tokens` set to -1.
6. Reference Handoff Check: Exits turn immediately if handoff is non-actionable.
7. Post-Tool Activity Touch: `agent._touch_activity("tool results posted, continuing iteration #...")` (`agent/conversation_loop.py:8395`) updates `last_activity_at` in SQLite to prevent gateway timeout kills during long turns.
8. Loop Iteration: `continue` returns to top of while-loop (`agent/conversation_loop.py:8397`).
9. Pre-Request Activity Touch: `api_call_count += 1`; `agent._touch_activity("starting API call #...")` (`agent/conversation_loop.py:2326-2328`).
10. Provider Call: Model request is sent with the newly compacted transcript.
11. Provider Response & Rearm: Provider returns `prompt_tokens`; if below threshold, `compression_attempts` resets to 0.

---

## Rust Port Recommendations

### 1. Seams in Existing Rust Codebase

#### `rust/crates/hermes-gateway/src/native_tools.rs`
- In `run_tool_loop_with_messages` (lines 725 to 789):
  After tools are executed and persisted (`model.persist_tool_loop_message(&result)?`, lines 783-784), line 786 calls:
  `model.maintain_tool_loop_messages(&mut messages, &tool_specs)`
- This is the exact seam for same-turn maintenance. Currently it only forwards to `maintain_after_tool_batch`.

#### `rust/crates/hermes-gateway/src/native_agent.rs`
- In `TranscriptModel::maintain_tool_loop_messages` (lines 97 to 109):
  Delegates to `self.inner.maintain_after_tool_batch(self.database, self.session_id, self.turn_lease_holder, messages, tools)`.
- In `NativeAgent::maintain_after_tool_batch` (lines 980 to 1113):
  Currently performs only deterministic proactive tool-result pruning (`crate::tool_result_prune::prune_old_tool_results`).
  It lacks:
  1. Full LLM summary compression logic.
  2. Provider-reported `last_prompt_tokens` tracking with the `-1` sentinel.
  3. Attempt tracking across loop iterations.
  4. Failure cooldown and ineffective breaker checks.
  5. Summary prompt generation via auxiliary model or primary model.
  6. Atomic in-place publication call to `SessionDb`.
  7. Reference-only handoff suppression to prevent issuing an unnecessary model step.

#### `rust/crates/hermes-gateway/src/automatic_compression.rs`
- Contains `AutomaticCompressionPolicy` (lines 115 to 350) with `decide`, `tail_token_budget`, and `compute_effective_threshold_with_output_for`.
- Contains `compression_boundaries` (lines 420 to 560) calculating prefix end and tail start.
- Contains `compress_before_turn` (lines 714 to 970), which currently only services pre-turn ingress admission.
- The boundary and wrapping primitives (`wrap`, `SUMMARY_ACK`) should be refactored into shared helpers usable by `maintain_after_tool_batch`.

#### `rust/crates/hermes-gateway/src/session_db.rs`
- Implements `publish_gateway_in_place_compression` (lines 1973 to 2154):
  Atomically soft-archives pre-compaction rows (`active = 0, compacted = 1`), clones prefix rows, inserts compacted messages, and clones tail rows.
- Implements guard operations: `load_compression_guard_state` (lines 1228 to 1261), `record_compression_failure_cooldown` (lines 1263 to 1306), `clear_compression_failure_cooldown` (lines 1308 to 1335), and `set_compression_breaker` (lines 1928 to 1969).
- The atomic database primitives are already in place and match SQLite expectations.

### 2. Relevant Tests and Verification Anchors

#### Authoritative Python Tests
- `tests/run_agent/test_compression_persistence.py:140-193`: Verifies that after in-place compaction, `conversation_history_after_compression` shallow-copies the compacted transcript so subsequent turn flushes do not re-insert compacted rows.
- `tests/run_agent/test_compression_persistence.py:194-270`: Verifies that an aborted compression attempt following an in-place compaction retains the valid flush baseline.
- `tests/agent/test_reference_handoff_active_turn.py:81-150`: Verifies `reference_handoff_would_drive_next_model_call` and ensures reference-only handoffs do not trigger extra model calls.
- `tests/agent/test_compression_anti_thrash_recovery.py`: Verifies rearming the attempt budget when provider prompt usage drops below the threshold.
- `tests/agent/test_compression_attempt_lifecycle.py`: Verifies attempt counter tracking and lock-skip refunding.
- `tests/agent/test_compression_split_failure_cooldown.py`: Verifies split failure cooldown recording.

#### Existing Rust Gateway Tests
- `rust/crates/hermes-gateway/src/automatic_compression.rs:1725-1895`: 13 table-driven tests for policy decision logic.
- `rust/crates/hermes-gateway/src/session_db.rs:5040-5150`: In-place compression SQLite transaction tests.
- `rust/crates/hermes-gateway/src/session_db.rs:7860-7935`: Guard state and breaker database tests.
- `rust/crates/hermes-gateway/src/message.rs:2237-2300`: `proactive_tool_prune_commits_before_the_same_turn_followup_request`.

#### Parity Traps Requiring Live HTTP and SQLite Integration Tests
1. Duplicate Persistence on Next Tool Batch:
   If the in-memory `messages` vector is replaced with compacted messages that lack persistence tracking, the next call to `model.persist_tool_loop_message` or end-of-turn flush will re-insert all compacted messages into `messages` table, doubling context size.
2. Sizing Fallback Oscillation (-1 Sentinel):
   If `last_prompt_tokens` is not set to `-1` post-compaction, the fallback will evaluate rough request sizing including tool schemas on the very next iteration, immediately re-triggering compression in the same turn.
3. Ghost Model Call on Reference Handoff:
   If `reference_handoff_would_drive_next_model_call` is not implemented in the native tool loop, a standalone summary handoff will be treated as an active user request, causing the model to hallucinate or re-execute completed tasks.
4. Broken Tool Groups on Tail Slicing:
   If `align_boundary_backward` is omitted during tail budget calculation, tool results will be severed from their parent assistant call, causing the sanitizer to drop valid results.
5. Anti-Growth Latch:
   If a generated summary is larger than the middle region it replaces, failure to refuse the summary and latch an ineffective strike will cause repeated expensive LLM summary calls on every subsequent tool iteration.
