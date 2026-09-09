# Same-turn full LLM compression oracle (claude)

Deterministic, source-executed Python oracle for the post-tool full LLM
compression decision and transcript adoption: the gate that runs after a
completed tool-result batch and before the next provider request. The oracle
drives the real decision functions and records their outputs as a stable
reference corpus for the Rust port. No decision logic is reimplemented.

- Generator: `rust/tools/same-turn-full-compression-oracle.py`
- Corpus: `rust/tools/same-turn-full-compression-goldens.json` (25 cases)
- Regenerate: `.venv/bin/python rust/tools/same-turn-full-compression-oracle.py`
- Verify: `.venv/bin/python rust/tools/same-turn-full-compression-oracle.py --check`

## Where the decision lives

The gate is inline in `agent/conversation_loop.py`, in the `run_conversation`
tool-result branch (the block that computes `_real_tokens`, then checks
`agent.compression_enabled and compression_attempts < max_compression_attempts
and _compressor.should_compress(_real_tokens)`, calls `agent._compress_context`,
and on return runs the lock-skip refund, `conversation_history_after_compression`
adoption, and the reference-only handoff skip). That block sits inside a
multi-thousand-line generator, so executing the whole loop is impractical. The
oracle instead drives the narrowest real functions the gate delegates to, in
the same order the loop calls them, and records the observable results. The
purely inline glue (attempt `+= 1` / `-= 1`, the `break`, next-request
ordering, SQLite session split) is listed under Deferred coverage below.

## What is patched, and why it stays faithful

- Summary LLM I/O: `agent.context_compressor.call_llm` is replaced by a
  scripted stand-in (one reply per call). The summary call is the only network
  reach in `ContextCompressor.compress` -> `_generate_summary`
  (`agent/context_compressor.py:5548`), so scripting it pins the state machine,
  not the summarizer. `summary_model` is left equal to the main model so the
  one-shot main-model fallback in `_generate_summary` never fires; every
  failure path stays at exactly one summary call.
- Model window: `get_model_context_length` is pinned to 8000 so construction
  and budget resolution never probe `/models`.
- Clock: `agent.context_compressor.time.monotonic` is pinned only for the
  cooldown and structural-backoff gate cases, so their `reason` strings
  (`cooldown:1000`, `structural_backoff:500`) are stable. No other case
  touches the clock.
- Persistence: `_session_db` is deliberately left unbound. That makes the
  durable-guard refresh in `should_compress_info` a no-op, which is the exact
  shape the in-memory trigger decision must survive.

No network, timestamps, randomness, absolute paths, or credentials enter a
golden. `append_message` stamps a timestamp on the restored user row in the
reference-handoff restore case; the oracle records only the stable
`{role, content}` projection.

## Exact oracle coverage (real functions executed)

Authoritative sources are cited by file and function; line numbers are current
local source at time of writing.

### trigger - `ContextCompressor.should_compress_info` (`agent/context_compressor.py:3921`)
- `prompt_usage_over_threshold_triggers`: real prompt usage above the resolved
  threshold returns `(True, None)`. Threshold for the 8000 window at 0.50 is
  6800 (small-context floor applied in `threshold_tokens`).
- `prompt_usage_below_threshold_no_compress`: `threshold_tokens - 1` returns
  `(False, None)`.
- `over_threshold_cooldown_blocks`: over threshold with the summary-failure
  cooldown armed returns `(False, "cooldown:1000")`. The loop's admit gate is
  then false, so no summary call is issued.
- `over_threshold_structural_backoff_blocks`: over threshold with a structural
  no-op backoff armed returns `(False, "structural_backoff:500")`. Same
  no-summary-call outcome. Both block reasons come from
  `_compression_block_reason` (`agent/context_compressor.py:3954`).

### token_selection - fallback sizing feeding `should_compress`
- `zero_usage_falls_back_to_request_sizing`: with no provider usage the loop
  sizes the request via `_midturn_request_pressure_tokens`
  (`agent/conversation_loop.py:133`) over `estimate_request_tokens_rough`
  (`agent/model_metadata.py:3965`). Both are executed for real; on a
  non-codex agent with no tools the fallback equals the rough estimate.
- `real_usage_used_verbatim` and `awaiting_real_usage_sentinel_zero` mirror the
  loop's inline three-way branch (`last_prompt_tokens > 0`, `== -1`, else). The
  branch selection is loop code, recorded here for the port with the value the
  loop would feed to `should_compress`.

### compress - `ContextCompressor.compress` (`agent/context_compressor.py:8038`)
- `successful_inplace_compression`: over a durable tool-batch transcript
  (user -> assistant(tool_calls) -> tool -> assistant), one summary call
  returns a new transcript (identity changed), shrinks 81 -> 13 messages,
  carries exactly one summary marker (one summary/ack pair), leaves no orphaned
  tool groups (checked both directions), preserves the newest tail user turn
  verbatim, sets `_last_compression_made_progress = True`. Tool-group
  completeness verified by `tool_group_orphans` in the oracle.
- `empty_summary_leaves_transcript_unchanged`: an HTTP-200 empty-content reply
  routes through the empty-content guard (`agent/context_compressor.py:5589`);
  compress aborts (`_last_compress_aborted`,
  `_last_summary_empty_content_failure`) and returns the transcript unchanged
  after one summary call.
- `failed_summary_leaves_transcript_unchanged`: a transient connection error
  sets `_last_summary_network_failure` and aborts unchanged after one call
  (abort branch at `agent/context_compressor.py:8449`).
- `structural_no_op_no_summary_call`: too few messages hits the structural
  no-op early return (`agent/context_compressor.py:8131`); the same object is
  returned with zero summary calls.

### adoption - `conversation_history_after_compression` (`agent/conversation_compression.py:2843`)
- `adopt_inplace_shallow_copy`: an in-place boundary returns a shallow copy of
  the compacted transcript, the baseline that keeps the same-turn identity
  flush from re-persisting already-written rows (duplicate-persistence guard).
- `adopt_no_boundary_keeps_previous`: an aborted/no-op attempt
  (`attempt_in_place = None`) keeps the pre-attempt baseline.
- `adopt_legacy_rotation_clears_baseline`: legacy rotation
  (`attempt_in_place = False`) clears the baseline so the child session writes
  the full compacted list.

### lock_skip - `compression_skipped_due_to_lock` (`agent/conversation_compression.py:1934`)
- `lock_skip_holder_string` / `lock_skip_bare_true` read `True`; a holder string
  and a bare `True` both count.
- `lock_skip_cleared_none` reads `False`.
- `lock_skip_magicmock_not_hijacked`: a MagicMock agent (auto-truthy attribute)
  must read `False`. This pins the type-pinned read (#69870 x #69840) the Rust
  port must reproduce, not a bare truthiness check.

### reference_handoff - `reference_handoff_would_drive_next_model_call` (`agent/context_compressor.py:9173`) and `_should_skip_model_call_for_reference_handoff` (`agent/conversation_loop.py:277`)
- `reference_only_handoff_skips_model_call`: a sole handoff after a completed
  assistant turn would drive the next call by itself, so the loop suppresses
  the extra provider call.
- `trailing_user_turn_no_skip`: a real user turn after the handoff keeps the
  call.
- `tool_result_after_handoff_not_sole_driver`: a tool result after the handoff
  means an in-flight exchange continues, so it is not a sole driver.
- `restorable_user_ask_no_skip`: `_restore_user_after_reference_handoff`
  re-appends the turn's real ask, so the handoff no longer drives.

### gate_composition - real predicates composed into the loop's admit gate
These execute the real predicates and mirror only the loop's two-line
bookkeeping, which is documented as deferred:
- `admitted_pass_consumes_one_attempt_one_call`: `should_compress` true and
  budget available admits one pass; the real `compress` runs once and returns a
  new transcript. Attempt count goes 0 -> 1 (loop arithmetic).
- `lock_skip_refunds_attempt_keeps_identity`: a no-op that returns the input
  object plus a live lock signal refunds the attempt (1 -> 0) and keeps the
  original transcript identity.
- `exhausted_attempts_no_summary_call`: `should_compress` true but the attempt
  budget is spent, so the gate is not admitted and no summary call is issued.

## Deferred to Rust's live integration test

These are outer-loop concerns the oracle cannot own without executing the whole
generator or a real SQLite session, and they belong in Rust's live HTTP + DB
integration test:

- The inline attempt bookkeeping (`compression_attempts += 1` on admit,
  `-= 1` on lock-skip refund) and the post-compaction `break`. The oracle
  records the predicate inputs and the intended arithmetic result; the actual
  mutation is loop state.
- Adoption reaching the next provider request, and ordering of the compaction
  relative to that request and the activity/session touch.
- The SQLite session split / in-place publication done by
  `archive_and_compact` and the `_DB_PERSISTED_MARKER` flush-dedup that
  actually prevents duplicate persistence. The oracle exercises the decision
  function that computes the flush baseline, not the DB write.

## Verification

- `.venv/bin/python rust/tools/same-turn-full-compression-oracle.py` writes the
  corpus (25 cases); `--check` returns OK against the checked-in file.
- Relevant Python tests, all passing:
  - `tests/agent/test_context_compressor.py`,
    `tests/agent/test_compressed_summary_metadata.py`,
    `tests/agent/test_compression_adoption_preserves_live_tail.py` (165 passed)
  - `tests/agent/test_context_compressor_zero_user_provenance.py`,
    `tests/agent/test_context_compressor_structural_backoff.py`,
    `tests/agent/test_compaction_anti_thrash.py`,
    `tests/agent/test_compression_attempt_lifecycle.py` (43 passed)
  - `tests/agent/test_reference_handoff_active_turn.py`,
    `tests/agent/test_preflight_lock_defer.py`,
    `tests/agent/test_compress_signal_leak.py`,
    `tests/agent/test_native_preflight_estimate.py` (43 passed)

## Notes for the Rust port

- The threshold is the resolved `threshold_tokens` (small-context floor
  applied), not `window * threshold_percent`. For an 8000 window at 0.50 it is
  6800, not 4000.
- Keeping `summary_model == model` is what holds a failed compression to a
  single summary call. A distinct summary model adds a one-shot main-model
  fallback (a second call) before the abort. The port's attempt accounting and
  call-count assertions must account for that fork.
- The empty-content and truncated-summary guards treat a well-formed HTTP 200
  as a failure and abort unchanged; the port must not store an empty or partial
  summary as a compaction checkpoint.
- Lock-skip is a type-pinned read (`is True or isinstance(str)`), not bare
  truthiness. A port that treats any truthy value as lock-skip will misroute
  every mocked or partially-built agent.
