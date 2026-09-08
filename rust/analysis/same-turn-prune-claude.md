# Same-turn proactive tool-result prune: Python runtime wiring

Scope: how proactive tool-result pruning is driven inside an active tool loop.
Covers the call site in `agent/conversation_loop.py`, the driver
`ContextCompressor.prune_tool_results_only` in `agent/context_compressor.py`, and
the persistence handshake (`archive_and_compact`, `stamp_db_persisted_markers`).
Excluded by request: the pure pruning algorithm (`_prune_old_tool_results` and its
passes), provider usage parsing, micro-compaction, and general compression
summaries. Paths and line numbers are repo-relative and were read at the time of
writing.

Observed vs inferred is marked per point. "Observed" means read directly from the
named lines. "Inference" means a conclusion drawn from those lines plus the
documented contract.

---

## 1. When the prune runs relative to everything else

Observed. The prune lives at `agent/conversation_loop.py:8354-8380`, inside the
tool-call branch of the conversation loop (the branch entered when
`assistant_message.tool_calls` is truthy). By that point in the same iteration:

- The assistant tool-call row and every tool-result row for this round have
  already been appended to the live `messages` list and incrementally flushed to
  the session DB (per the prior-session trace: assistant row flushed before tools
  run, each result flushed before the next dispatch). The prune sees a transcript
  whose tool rows are already durable.
- The next provider request has NOT happened yet. The prune sits before the
  `continue` at `agent/conversation_loop.py:8397`, which loops back to the next
  `model.step`/completion call. So the order per tool round is: execute tools ->
  persist results -> (optional) compress OR prune -> touch activity -> next
  request.

Observed, branch structure. The compression decision at
`agent/conversation_loop.py:8260-8264` is:

```
if (agent.compression_enabled
    and compression_attempts < max_compression_attempts
    and _compressor.should_compress(_real_tokens)):
    ... full compression (LLM summary) ...
elif agent.compression_enabled:
    ... blocked-overflow warning ...
    ... proactive prune ...  # lines 8341-8380
```

So the prune runs only in the `elif` arm: compression is enabled but full
compression did NOT run this iteration (threshold not met, per-turn attempts
exhausted, or blocked by cooldown/anti-thrash). Full compression and the prune are
mutually exclusive within one iteration. Inference: on large-window models
`should_compress` (~50% of window) rarely fires, so the `elif` arm is the common
path and the prune is the routine per-iteration reclaimer, exactly as the docstring
at `agent/context_compressor.py:4524-4530` states.

Observed, iteration accounting. The prune does NOT touch `compression_attempts`.
That counter is incremented only in the full-compression arm
(`agent/conversation_loop.py:8265`) and refunded on lock-skip
(`:8295`). The prune has no per-turn attempt budget; its own internal gates
(threshold, rearm, min-reclaim) throttle it instead.

Observed, hooks. The only hook-like side effects around the prune are logging and
`agent._warn_context_overflow_blocked` (`:8336`) for the blocked-compression case,
which runs before the prune and is independent of it. After committing, the loop
sets `agent._session_messages = messages` (`:8383`) and calls
`agent._touch_activity(...)` (`:8395`) so the gateway inactivity monitor does not
kill the session during post-tool processing. Then `continue`.

Observed, final response delivery. The prune is strictly a mid-loop, tool-round
event. It cannot run on the no-tool-call (final-response) branch, which begins at
`agent/conversation_loop.py:8399`. Final assistant persistence happens later in
`turn_finalizer.py` (per prior trace), untouched by the prune.

---

## 2. State in, state out, and how the live transcript adopts the pruned generation

Observed, call. `agent/conversation_loop.py:8357-8359`:

```
_pruned_msgs, _pruned_n = _prune(messages, current_tokens=_real_tokens)
```

Input: the live `messages` list object and `current_tokens=_real_tokens`.
`_real_tokens` is computed at `:8226-8258` from `_compressor.last_prompt_tokens`
(the provider-billed prompt token count) with a rough overhead-aware fallback when
usage is unavailable.

Observed, return contract. `prune_tool_results_only`
(`agent/context_compressor.py:4519-4654`) returns `tuple[list, int]`:
`(pruned_msgs, pruned_count)`. On every no-op path it returns the INPUT object
`messages` and `0` (`:4571, :4573, :4577, :4586, :4599, :4610, :4620, :4647`). On a
committed prune it returns the NEW list `pruned_msgs` and a non-zero count
(`:4654`).

Observed, caller adopt gate. `agent/conversation_loop.py:8368`:

```
if _pruned_n and _pruned_msgs is not messages:
    messages = _pruned_msgs
```

Identity check `is not messages` plus non-zero count is the standard no-op caller
contract. The loop does NOT rebuild `conversation_history` here and deliberately
does not call `conversation_history_after_compression`
(`agent/conversation_loop.py:8369-8379` comment).

Observed, no duplicate persistence. The commit path calls
`session_db.archive_and_compact(session_id, pruned_msgs, model_config_patch=...)`
(`agent/context_compressor.py:4634-4640`), which atomically soft-archives the prior
active rows and inserts `pruned_msgs` as the new active set. Immediately after a
successful commit it calls `stamp_db_persisted_markers(pruned_msgs)` (`:4650`).
`stamp_db_persisted_markers` (`agent/context_compressor.py:423-447`) sets
`_DB_PERSISTED_MARKER` on those exact dict instances. Because the loop then makes
`messages` point at that same `pruned_msgs` list, the next append-only flush
(`_persist_session` -> `_flush_messages_to_session_db_unlocked`) sees the marker
and skips those rows instead of re-INSERTing the whole pruned transcript.

Inference. This is the whole adoption mechanism: the committed pruned generation IS
the live in-memory transcript (same object), pre-stamped as durable, so no separate
history rebuild and no second write are needed. The docstring at
`agent/context_compressor.py:433-443` confirms this stamp site is shared with the
in-place batch commit and micro-compaction, and warns that an unstamped committed
set doubles the transcript on the next persist walk.

---

## 3. Runtime gates (threshold, rearm, minimum-reclaim, error, no-op)

All observed in `agent/context_compressor.py:4570-4654`, evaluated in this order:

1. Disabled gate (`:4570`): `if self.proactive_prune_tokens <= 0: return messages, 0`.
   Config-driven; default `proactive_prune_tokens=0` (constructor `:3485`, stored
   `:3525`), so the feature is off unless configured.
2. Trigger threshold (`:4572`): `if current_tokens is not None and current_tokens
   < self.proactive_prune_tokens: return messages, 0`. Below the configured
   proactive threshold, no-op. Note this is far below the full-compression
   `threshold_tokens` used by `should_compress`.
3. Tail-only gate (`:4575`): if
   `len(messages) <= protect_last_n + _protect_head_size(messages) + 1`, nothing
   lives outside the protected head+tail, so warn (`prune:tail_only`) and no-op.
   `protect_last_n` defaults to 20 (`:3472, :3522`).
4. Rearm / hysteresis gate (`:4578-4586`): `before = sum(_estimate_msg_budget_tokens
   ...)`. If `before < self._proactive_prune_rearm_tokens`, the message-body
   estimate has not regrown a full trigger-sized runway since the last commit, so
   no-op, UNLESS `_billed_basis_over_threshold(current_tokens)` is true. The billed
   basis (`:4461-4475`) is `current_tokens >= threshold_tokens`; it bypasses ONLY
   the rearm gate (never the reclaim gate) so schema overhead cannot park a
   genuinely over-threshold session below the rearm mark forever (#101889). The
   under-threshold rearm skip is silent (ordinary prompt-cache hysteresis).
5. Store capability gate (`:4591-4599`): if a `_session_db` and `_session_id` are
   bound but the store lacks a callable `archive_and_compact`, warn
   (`prune:store_cannot_persist`) and no-op before paying for the scan.
6. Nothing-eligible no-op (`:4600-4610`): run `_prune_old_tool_results`; if
   `pruned_count == 0`, warn (`prune:nothing_eligible`) and return the input.
7. Minimum-reclaim gate (`:4611-4620`): `after = sum(_estimate_msg_budget_tokens
   ...)`, `reclaimed = max(0, before - after)`. If
   `reclaimed < self.proactive_prune_min_reclaim_tokens` (default 4096, constructor
   `:3487`, clamped `:3545-3546`), warn (`prune:reclaim_below_minimum`) and no-op.
   This is the prompt-cache-break amortization gate: only commit when the rewrite
   buys a meaningful batch of tokens.
8. Error gate (`:4641-4647`): if `archive_and_compact` raises, log a warning and
   `return messages, 0` (original transcript kept, rearm NOT advanced, markers NOT
   stamped). At the caller, the `try/except` at
   `agent/conversation_loop.py:8360-8365` also catches any exception from the whole
   call, logging at debug and treating it as `(messages, 0)`.

Observed, over-threshold no-op logging. Every no-op taken while a billed reading is
over `threshold_tokens` is logged once per distinct reason via `_warn_reclamation_no_op`
(`:4477-4517`), deduped on `(reason, rearm_snapshot)` so a busy tool loop logs once
per state, not once per iteration.

Observed, rearm advance on commit. On success `next_rearm_tokens = after + runway`
where `runway = max(reclaimed, proactive_prune_tokens, proactive_prune_min_reclaim_tokens)`
(`:4625-4630`). The in-memory `_proactive_prune_rearm_tokens` is set to that value
(`:4651`) and `_last_reclaim_block_warn` is cleared (`:4653`).

---

## 4. Prompt-cache / client lifecycle action after a committed prune

Observed. There is NO explicit prompt-cache invalidation or provider-client reset
in the prune path. The prune does NOT set `last_prompt_tokens = -1` and does NOT set
`awaiting_real_usage_after_compression` (contrast: the full-compression path does,
which `should_compress` reads at `agent/context_compressor.py:3877-3888`). It does
not reset or rebuild any client. In the loop, the only post-commit actions are
`messages = _pruned_msgs` (`:8380`), `agent._session_messages = messages` (`:8383`),
and `agent._touch_activity(...)` (`:8395`).

Inference. The cache break is implicit, not commanded. A committed prune rewrites
message bodies the provider has already seen, so the cached prefix is invalidated
from the earliest rewritten message forward on the next request purely because the
wire bytes changed. The docstring states this explicitly
(`agent/context_compressor.py:4551-4558`): "a committed prune rewrites message
bodies the provider has already seen, invalidating the cached prefix from the
earliest rewritten message forward, exactly like a compression boundary." The
`proactive_prune_min_reclaim_tokens` gate and the rearm runway exist precisely so
these implicit cache breaks stay episodic (like a compression boundary) rather than
firing every tool iteration.

Observed, durable rearm as the only extra state written. The one piece of durable
state a commit writes beyond the transcript is the model-config patch
`{_proactive_prune_rearm_tokens: next_rearm_tokens}`
(`PROACTIVE_PRUNE_REARM_MODEL_CONFIG_KEY`, `agent/context_compressor.py:317`) passed
inside the same `archive_and_compact` transaction (`:4634-4640`). On a resumed
session this is restored by `_load_proactive_prune_rearm_tokens` (`:2783-2799`), and
it is cleared/reset by `_reset_proactive_prune_rearm` (`:4449-4458`) and
`_clear_durable_proactive_prune_rearm` (`:2801-2814`) on compaction, session
reset/rebind, or model recalibration.

---

## 5. Smallest safe Rust insertion seam

Target loop: `rust/crates/hermes-gateway/src/native_tools.rs:535-786`
(`run_tool_loop_with_messages`). The `Step::ToolCalls` arm appends the assistant row
and each tool result to the owned `messages: Vec<Value>` and persists each via
`model.persist_tool_loop_message` (`:668-773`). The arm ends at `:774`; control then
loops back to `model.step(&messages, &tool_specs)` at `:560`.

Seam. The Python-equivalent insertion point is at the very end of the
`Step::ToolCalls` arm, after the `for call in calls { ... }` result loop closes at
`native_tools.rs:774` and before the `for` loop iterates back to the next
`model.step`. That is the exact analog of the post-tool block that precedes
`continue` at `conversation_loop.py:8397`. It runs after results are appended and
persisted, before the next provider request. It must NOT run on the `Step::Final`
arm (`:561-591`) or the budget-exhaustion tail (`:781-785`).

Ownership.
- `messages` is owned locally by the loop (`let mut messages` at `:548`), so a prune
  can replace it in place with a returned pruned `Vec<Value>` with no borrow
  conflict, mirroring `messages = _pruned_msgs`.
- The commit must go through the `model: &dyn ChatModel` trait, which is the only
  handle the loop holds to persistence. The concrete impl `TranscriptModel`
  (`rust/crates/hermes-gateway/src/native_agent.rs:30-36`) already owns
  `database: Option<&SessionDb>`, `session_id: &str`, and
  `turn_lease_holder: Option<&str>`, so a new trait method (for example
  `prune_tool_results(&self, messages: &[Value], current_tokens: Option<u64>) ->
  Result<Option<Vec<Value>>>`) can commit via the store and hand back the pruned
  generation, keeping the store handle out of the generic loop. This parallels the
  existing `persist_tool_loop_message` delegation at `native_agent.rs:77-95`.
- `current_tokens`: there is no per-iteration billed prompt-token reading exposed to
  the loop today. Usage is captured as a whole-turn accumulator
  (`native_agent.rs:646-655`, `:729-730`), not a per-step `prompt_tokens`. So the
  first safe version should pass `current_tokens = None` (Python allows the Optional
  and simply skips the trigger-threshold check at `context_compressor.py:4572` and
  the billed-basis rearm bypass), relying on the message-body estimate for the rearm
  and min-reclaim gates. Threading a real per-step `prompt_tokens` out of
  `ChatModel::step` is a later refinement, not required for correctness.

Failure propagation. Match the Python contract, not `?`-propagation. A prune commit
failure or any prune error must be swallowed into a no-op (keep the original
`messages`, do not advance rearm, do not stamp), exactly like
`context_compressor.py:4641-4647` and the caller `try/except` at
`conversation_loop.py:8360-8365`. So the trait method should return
`Result<Option<Vec<Value>>>` where `Err` is logged and treated as "keep original",
and `Ok(None)` is the no-op (identity) case; only `Ok(Some(new))` replaces
`messages`. Do NOT let a prune failure abort the turn the way
`persist_tool_loop_message` failure does at `native_tools.rs:668/772` (that is the
tool-result persistence contract, which must be strict; the prune is best-effort
reclamation and must fail open).

Note on the existing Rust prune module. The prior lane already built the pure
algorithm at `rust/crates/hermes-gateway/src/tool_result_prune.rs` and the
persistence contract analysis at `rust/analysis/tool-prune-persistence-claude.md`
(the `publish_tool_result_prune` / wide-column insert + rearm patch design). This
seam is the caller that would drive that pure function and then that publish path.

---

## Rust integration checklist

1. Insert the prune call at the end of the `Step::ToolCalls` arm in
   `native_tools.rs` (after `:774`, before loop re-entry). Never on `Step::Final` or
   the exhaustion tail.
2. Gate it as an else-of-compression equivalent: if a full compaction runs this
   round, do not also prune that round (Python mutual exclusion). If Rust has no
   mid-loop compaction yet, the prune is simply the only reclaimer and can run each
   eligible round.
3. Route the commit through a new `ChatModel` method backed by `TranscriptModel`
   (holds `database`, `session_id`, `turn_lease_holder`); keep the store out of the
   generic loop.
4. Return `Result<Option<Vec<Value>>>`: `Ok(Some(new))` = adopt, `Ok(None)` = no-op
   keep original, `Err` = log and keep original. Fail open; never abort the turn.
5. Adopt via `messages = new;` only on `Ok(Some(_))` (identity/None-safe), mirroring
   the `is not messages` gate at `conversation_loop.py:8368`.
6. Enforce all gate order from Section 3 inside the driver: disabled -> trigger
   threshold -> tail-only -> rearm/hysteresis (with billed-basis bypass) -> store
   capability -> run algorithm -> nothing-eligible -> min-reclaim -> commit.
7. Default config so the feature is OFF unless `proactive_prune_tokens > 0`; default
   `proactive_prune_min_reclaim_tokens = 4096`, `protect_last_n = 20`.
8. Persist through an `archive_and_compact`-equivalent: soft-archive prior active
   rows and insert the pruned set in ONE transaction, plus the
   `_proactive_prune_rearm_tokens = next_rearm` model-config patch. Use the wide
   (tool-scaffolding-preserving) insert from
   `rust/analysis/tool-prune-persistence-claude.md`, not the narrow
   compression-summary insert. Reuse the existing turn-lease fence and durable-route
   CAS.
9. After a successful commit, stamp the returned rows as durably persisted (the
   `stamp_db_persisted_markers` analog) so the next incremental flush does not
   re-INSERT them and double the transcript.
10. Do NOT rebuild history, do NOT reset the provider client, and do NOT set any
    "awaiting real usage" flag. The cache break is implicit via rewritten bodies.
    The only durable side effect beyond the transcript is the rearm patch.
11. Compute `next_rearm = after + max(reclaimed, proactive_prune_tokens,
    min_reclaim)` from a message-body token estimate; hold it in memory and restore
    it from model-config on resume; clear it on compaction, session reset/rebind, and
    model change.
12. For v1 pass `current_tokens = None` (no per-step billed reading exists). Later,
    thread a real `prompt_tokens` out of `ChatModel::step` to enable the trigger
    threshold and the over-threshold rearm bypass, plus once-per-state
    over-threshold no-op warnings.
13. Rearm/min-reclaim exist to keep cache breaks episodic; do not weaken them, or the
    prune will invalidate the prompt cache every tool iteration.
