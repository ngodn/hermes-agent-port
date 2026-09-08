# Native tool-call history: persistence and replay (Python oracle)

Scope: how authoritative Python Hermes persists a native agent's tool-call
history across turns and replays it on resume. Traced through `run_agent.py`,
`agent/conversation_loop.py`, `agent/turn_finalizer.py`, `agent/tool_executor.py`,
`agent/tool_dispatch_helpers.py`, `agent/chat_completion_helpers.py`,
`agent/agent_runtime_helpers.py`, and `hermes_state.py`. All paths repo-relative.

"Observed" = read directly in the source. "Inference" = concluded from
surrounding code, not stated in one place.

---

## 1. Which messages persist, and when relative to the final reply

The tool loop persists **incrementally, mid-turn**, not once at the end. A single
successful tool round produces this write order (observed):

1. Assistant tool-call row is built (`agent/chat_completion_helpers.py:2354`
   `build_assistant_message`) and appended to the live list
   (`agent/conversation_loop.py:8082`, also `:8747`).
2. **Before any tool executes**, the whole turn tail is flushed:
   `agent._flush_messages_to_session_db(messages, conversation_history)`
   (`agent/conversation_loop.py:8104-8112`). Comment at `:8106-8109`: persist the
   assistant tool-call turn before side effects so a destructive tool that
   restarts Hermes still finds the exact tool-call block that ran.
   If the flush returns `False`, the loop breaks with
   `_turn_exit_reason="session_persistence_failed"`, `final_response=""`,
   `failed=True`, and **no tools run** (`:8126-8138`).
3. Tools execute (`agent/conversation_loop.py:8158`
   `agent._execute_tool_calls`).
4. Each tool result row is appended and **flushed before the next dispatch and
   before any UI projection** via `_flush_session_db_after_tool_progress`
   (`agent/tool_executor.py:211-239`, called at `:1858` sequential, and in the
   concurrent/segmented paths). The tool-result message is built by
   `make_tool_result_message` (`agent/tool_dispatch_helpers.py:586`).
5. After execution, if `agent._incremental_persistence_failed` is set, the loop
   breaks with the same `session_persistence_failed` abort (`:8160-8167`).

The **final assistant reply** is persisted last, at turn finalization:
`agent._persist_session(messages, conversation_history)` in
`agent/turn_finalizer.py:473`. Before that persist, finalize_turn guarantees the
transcript ends on an assistant row (see section 4). `_persist_session` fans out
to the JSON session log and `_flush_messages_to_session_db`
(`run_agent.py:2240-2278`).

Idempotency (observed): every written message dict is stamped with an intrinsic
`_DB_PERSISTED_MARKER` (`run_agent.py:334`, stamped via
`sync_flushed_message_markers` at `run_agent.py:2651-2653`). Re-flushing from a
later exit path skips already-marked dicts (`run_agent.py:2475-2482`), so the
mid-turn flushes and the finalize flush do not double-write. Rows written from a
durable load are born marked (`hermes_state.py:14137`).

Batching (observed): a turn-boundary flush writes all new rows in one
transaction via `append_messages_batch` (`run_agent.py:2636-2650`,
`hermes_state.py:12839`). All-or-nothing: on failure no rows land and no markers
stamp, so the next flush re-scans and re-writes the tail
(`run_agent.py:2629-2662`).

---

## 2. Fields stored and replayed

### Row dict assembled per message (`run_agent.py:2578-2624`)

`role`, `content`, `tool_name`, `tool_calls`, `tool_call_id`, `finish_reason`,
`reasoning`, `reasoning_content`, `reasoning_details`, `codex_reasoning_items`,
`codex_message_items`, `_compressed_summary`, `timestamp`, `api_content`,
`display_kind`, `display_metadata`, `platform_message_id`, and `_row_id` when
present.

Content normalization before write (observed):
- Multimodal tool results are collapsed to a text summary; image parts are
  dropped and replaced with `[screenshot]` (`run_agent.py:2556-2569`). Base64
  images are never persisted.
- `api_content` sidecar is the exact bytes sent on the wire when they differ
  from the clean `content` (persist-override text, sanitize divergence). Stored
  only when it actually differs (`run_agent.py:2485-2555`). Replayed verbatim,
  no sanitize, no strip (`hermes_state.py:14146-14153`).
- `tool_calls` is normalized to a list of `{name, arguments}` when the message
  carries SDK objects, else passed through as the already-normalized list
  (`run_agent.py:2570-2577`).

### Column write (`hermes_state.py`)

Both writers use the same 22-column INSERT: `append_message`
(`hermes_state.py:12783-12813`) and `_insert_message_rows`
(`hermes_state.py:13228-13258`), the latter driven by `append_messages_batch`
(`hermes_state.py:12900-12912`). Reasoning/codex columns are **role-gated to
assistant** at write time (`hermes_state.py:13200-13206`, `:13245-13246`);
non-assistant rows store NULL there.

### Assistant tool-call row fields (`build_assistant_message`)

Built at `agent/chat_completion_helpers.py:2354-2608`:
- `content` (think-blocks stripped, surrogates scrubbed, secrets redacted:
  `:2410-2422`), `reasoning` (`:2438`), `finish_reason` (`:2439`), `timestamp`
  via `stamp_message_timestamp` (`:2435`).
- `reasoning_content` set from SDK field, else padded to `" "` for
  thinking-mode providers on tool-call turns, else promoted from streamed
  `reasoning` (`:2442-2486`).
- `reasoning_details`, `anthropic_content_blocks`, `bedrock_content_blocks`,
  `codex_reasoning_items`, `codex_message_items` carried when present
  (`:2488-2537`). Note: `anthropic_content_blocks` / `bedrock_content_blocks`
  are built on the dict but are **not** in the DB column set, so they do not
  round-trip through state.db (inference from the column list at
  `hermes_state.py:13229-13232`).
- `tool_calls`: each entry is `{id, call_id, response_item_id, type,
  function:{name, arguments}}` plus optional `extra_content`
  (`:2539-2606`). Arguments are intentionally **not** redacted so replay is
  byte-stable (`:2577-2593`, refs #43083).

### Tool-result row fields (`make_tool_result_message`)

Built at `agent/tool_dispatch_helpers.py:586-637`: `role="tool"`, **both** `name`
and `tool_name` set to the tool name (`:623-624`), `content` (untrusted-wrap
applied for web/browser/mcp tools), `tool_call_id` (normalized), `timestamp`,
plus optional `_tool_output_risk` and `effect_disposition`.

Persistence note (observed): only `tool_name` has a DB column; the OpenAI wire
field `name` is not persisted. On reload only `tool_name` is restored
(`hermes_state.py:14166-14167`); the transport adapter re-derives `name` at send
time (inference).

### Replay (`get_messages_as_conversation` -> `_rows_to_conversation`)

`hermes_state.py:14014-14212`. Reconstructed keys: `role`, `content`
(user/assistant string content re-run through `sanitize_context().strip()` at
`:14123-14124`), `api_content` verbatim, `display_kind`, `display_metadata`,
`_compressed_summary` (opt-in), `timestamp`, `tool_call_id`, `tool_name`,
`effect_disposition`, `tool_calls` (JSON-decoded), `message_id` (from
`platform_message_id`), `observed`. Assistant-only: `finish_reason`,
`reasoning`, `reasoning_content`, `reasoning_details`, `codex_reasoning_items`,
`codex_message_items` (`:14188-14212`). Every restored dict is stamped
`_DB_PERSISTED_MARKER` so a later flush does not re-append the whole transcript
(`:14137`).

### Private / synthetic retry messages (observed)

Ephemeral scaffolding is **never persisted**. `_flush_messages_to_session_db`
skips any message for which `_is_ephemeral_scaffolding(msg)` is true, regardless
of position (`run_agent.py:2464-2474`). The tail stripper drops messages flagged
`_empty_recovery_synthetic` or `_empty_terminal_sentinel`
(`run_agent.py:2292-2301`), and `_thinking_prefill` messages are popped before a
tool round (`agent/conversation_loop.py:8040-8047`). These synthetic empty /
nudge / prefill turns must not survive into the durable transcript
(`run_agent.py:2464-2472`).

---

## 3. Provider failure or tool-loop failure after some tool calls completed

Two distinct outcomes:

**A. Provider / model call fails, but tool rows already landed.** The completed
tool rows were already flushed incrementally (section 1), and every error break
in the loop calls `agent._persist_session(messages, conversation_history)` before
returning (e.g. `agent/conversation_loop.py:3208, 3937, 3972, 4141, 4234, 4288,
4502`; final safety net at `:9185`; finalize at `agent/turn_finalizer.py:473`).
So the completed assistant-tool-call plus tool-result group survives (observed).
On a genuinely broken tail, `_drop_trailing_empty_response_scaffolding` first
strips synthetic scaffolding and, **only if scaffolding was present**, rewinds
past a hanging `tool` tail and its owning `assistant(tool_calls)` message so the
next user turn does not land after an orphan tool result
(`run_agent.py:2280-2331`). A normal mid-progress tool tail is left intact
(`:2309-2310`).

**B. Persistence itself fails mid-tool-loop.** If the pre-execution flush returns
`False` (`agent/conversation_loop.py:8126-8138`) or a per-tool flush sets
`_incremental_persistence_failed` (`agent/tool_executor.py:225-238`,
checked at `agent/conversation_loop.py:8160-8167`), the turn aborts with
`_turn_exit_reason="session_persistence_failed"` and `final_response=""`. In-memory
tool results are **not** sent back to the model and no later events project. The
classified cause (locked / corrupt / disk) is surfaced in the result contract
(`run_agent.py:2663-2736`).

Interrupt during a batch (observed): the sequential executor emits a cancelled
`tool` result for every un-run call via `_append_cancelled_tool_results`
(`agent/tool_executor.py:1937-1954`) so the assistant tool-call group stays
complete. finalize_turn additionally closes an interrupted tool tail with a
synthetic assistant message (`close_interrupted_tool_sequence`,
`agent/turn_finalizer.py:354-356`).

---

## 4. Role-alternation and complete-tool-group invariants

Assumed model contract (observed, `agent/agent_runtime_helpers.py:573-631`):
after the system message, user/tool alternates with assistant; no two
consecutive user messages; every `tool` result must follow an
`assistant`-with-`tool_calls`; every `tool_call_id` on an assistant message must
be answered by a following `tool` result.

Enforcement points:
- **Write-order guarantee.** Replay orders strictly `ORDER BY id` (insertion
  order), never by timestamp, because `time.time()` is non-monotonic and would
  otherwise sort a tool result before its assistant tool-call row and break
  adjacency (`hermes_state.py:14062-14070`).
- **Pre-request repair.** `repair_message_sequence` merges consecutive assistant
  turns (union of tool_calls), drops stray tool results with no matching call,
  prunes unanswered tool_calls (dropping the turn if that empties it), and merges
  consecutive user turns (`agent/agent_runtime_helpers.py:573-631`, passes 0-3).
  `id`/`call_id` are treated as a matched superset. It mutates only the
  per-request list, never the stored transcript. Live-replay restores pass
  `repair_alternation=True` (`hermes_state.py:14037-14043`).
- **Write-time repair.** `resolve_and_repair_transcript_batch` runs inside the
  batch write transaction before `_insert_message_rows`
  (`hermes_state.py:12898-12912`).
- **Complete-group on tool-call turn.** The pre-execution flush persists the
  assistant tool-call row and every already-appended tool result together; the
  loop refuses to run tools if that fails (section 1).
- **Assistant-tail invariant.** finalize_turn guarantees "delivered
  final_response implies an assistant row in the transcript": if the tail is not
  an assistant row it appends the final response; if the tail is a pure tool-call
  turn it fills that row's content instead of appending (avoiding
  assistant->assistant) (`agent/turn_finalizer.py:358-402`, refs #43849/#44100).
- **Interrupt completeness.** Cancelled results for every un-run call
  (`agent/tool_executor.py:1937-1954`) and interrupted-tail closer
  (`agent/turn_finalizer.py:354-356`).

---

## 5. Source locations and proving tests

Source:
- Incremental persist ordering: `agent/conversation_loop.py:8082, 8104-8138,
  8158-8167`; `agent/tool_executor.py:211-239, 1856-1863`.
- Row assembly / field selection: `run_agent.py:2350-2662`
  (fields `:2578-2624`).
- Assistant/tool-result builders: `agent/chat_completion_helpers.py:2354-2608`;
  `agent/tool_dispatch_helpers.py:586-637`.
- Column write: `hermes_state.py:12693-12931` (append),
  `:13175-13267` (`_insert_message_rows`).
- Replay: `hermes_state.py:14014-14212`.
- Alternation repair: `agent/agent_runtime_helpers.py:573-631`;
  `run_agent.py:2280-2331`; `agent/turn_finalizer.py:340-402`.

Tests:
- `tests/run_agent/test_tool_call_incremental_persistence.py`:
  `test_run_conversation_flushes_assistant_tool_call_before_execution` (`:125`),
  `test_failed_assistant_persist_blocks_ui_projection_and_tool_side_effects`
  (`:223`),
  `test_execute_tool_calls_sequential_flushes_each_tool_result_before_next_dispatch`
  (`:333`),
  `test_sequential_keyboard_interrupt_emits_results_for_all_calls` (`:384`),
  `test_failed_tool_result_persist_blocks_completion_projection` (`:499`),
  `test_segmented_batch_stops_before_later_segment_after_persist_failure`
  (`:536`),
  `test_execute_tool_calls_concurrent_flushes_each_tool_result_in_order`
  (`:570`),
  `test_empty_final_response_updates_already_flushed_blank_assistant_row`
  (`:619`),
  `test_flush_atomic_mixed_repair_and_append_rollback_on_failure` (`:809`).
- `tests/run_agent/test_tool_name_db_persistence.py:28`
  `test_tool_name_persisted_to_session_db` (tool_name reaches the batch flush).
- `tests/test_hermes_state.py`: `test_append_and_get_messages` (`:603`),
  `test_reasoning_persisted_and_restored` (`:716`),
  `test_append_message_with_explicit_timestamp` (`:784`),
  `test_append_message_round_trips_display_fields` (`:5183`),
  `test_append_message_survives_lone_surrogate_content` (`:5155`).
- `tests/agent/test_turn_finalizer_final_response_persistence.py`:
  `test_final_response_closes_tool_tail_before_persistence` (`:88`),
  `test_final_response_fills_pure_tool_call_tail` (`:179`).
- `tests/agent/test_compression_rotation_state.py`:
  `test_mid_tool_loop_rows_do_not_duplicate_after_failed_parent_flush` (`:336`),
  `..._direct_path` (`:395`).
- `tests/run_agent/test_81641_text_turn_incremental_persistence.py`:
  `test_completed_text_turn_is_flushed_before_finalization` (`:108`),
  `test_flush_failure_does_not_abort_the_completed_turn` (`:155`).

---

## Checklist for the Rust integrator

- [ ] Persist the assistant tool-call row and its already-appended tool results
      to the durable store **before** running any tool side effect, and refuse to
      execute tools if that write fails (abort turn as
      `session_persistence_failed`, empty final response).
- [ ] Flush **each** tool result to the store before dispatching the next tool
      and before projecting it to any UI/stream surface.
- [ ] Persist the final assistant reply last, after ensuring the tail is an
      assistant row (append if not; fill a pure tool-call tail rather than
      appending an assistant->assistant pair).
- [ ] Store, per row: `role`, `content`, `tool_name`, `tool_calls` (as
      `{name, arguments}` list), `tool_call_id`, `finish_reason`, and
      assistant-only `reasoning`, `reasoning_content`, `reasoning_details`,
      `codex_reasoning_items`, `codex_message_items`, plus `api_content`,
      `_compressed_summary`, `timestamp`, `display_kind`, `display_metadata`,
      `platform_message_id`. Role-gate the reasoning/codex fields to assistant.
- [ ] Keep `api_content` as a byte-exact wire sidecar written only when it
      differs from `content`; replay it verbatim (no sanitize, no strip).
- [ ] Collapse multimodal tool-result content to a text summary and drop base64
      images before write; never persist images to the transcript store.
- [ ] Persist the tool name into `tool_name`; re-derive the wire `name` at send
      time (do not expect a `name` column on reload).
- [ ] Never persist ephemeral scaffolding (empty-recovery / terminal-sentinel /
      thinking-prefill / nudge). Skip by intrinsic flag regardless of position.
- [ ] Order replay by insertion id, never by timestamp, so tool-call and
      tool-result adjacency holds.
- [ ] Enforce complete tool groups on both failure and interrupt: emit a result
      for every un-run tool_call_id, and close an interrupted tool tail with an
      assistant row.
- [ ] Run a pre-request alternation repair on the per-request list only (merge
      consecutive assistants uniting tool_calls, drop stray tool results, prune
      unanswered tool_calls, merge consecutive users) using an `id`/`call_id`
      matched superset. Do not mutate the stored transcript.
- [ ] Make re-persistence idempotent with a per-row persisted marker (born-marked
      on durable load) so multiple exit-path flushes never double-write.
- [ ] Write a turn's new rows in one all-or-nothing transaction; on failure land
      nothing and re-scan/re-write the tail next attempt.
