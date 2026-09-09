# Full-compression structural no-op backoff oracle (#93022)

Deterministic, source-executed Python oracle for the transient structural
no-op backoff that guards full context compression. It drives the **real**
current `ContextCompressor` methods and records their genuine state and
returns. No backoff decision is reimplemented.

Artifacts:

- `rust/tools/compression-structural-backoff-oracle.py` - the generator.
- `rust/tools/compression-structural-backoff-goldens.json` - 24 cases.
- this report.

## What the backoff is

A full-compression attempt can find nothing eligible to compress inside the
protection window (too few messages, no compressible window, only
already-summarized handoffs), or a fired compaction can return the transcript
byte-for-byte unchanged. That is "nothing to compress right now", not an
ineffective attempt. Counting it as an ineffective strike would permanently
disarm auto-compaction on short sessions (the anti-thrash breaker latches at
`>= 2`). Instead the compressor arms a transient in-memory timer
(`_structural_no_op_backoff_until = monotonic() + 300s`) that defers retries so
a transcript that cannot shrink does not re-fire the scan (and re-summarize)
every turn (#40803), while auto-compaction resumes on its own once the backoff
lapses or the transcript outgrows the window.

## Authoritative sources executed

All line numbers are current local source at generation time.

- `agent/context_compressor.py`
  - `_STRUCTURAL_NO_OP_BACKOFF_SECONDS = 300.0` class constant (line 3346).
  - `__init__` field init to `0.0` (line 3663); `on_session_reset` (line 2365),
    `on_session_end` (line 2669), `bind_session_state` (line 2702) each
    re-zero it.
  - `_record_structural_no_op(reason)` (lines 2923-2946) - arms the timer,
    logs, touches no counter.
  - `record_completed_compaction(...)` (lines 2973-3015) - zeroes the backoff
    at line 2989 before any streak bookkeeping, including on the
    `feasibility_skip` branch (the clear precedes its early return).
  - `_compression_block_reason` (lines 3954-3983) - reports
    `cooldown:<s>` > `structural_backoff:<s>` > `ineffective` > `None`, in that
    check order.
  - `_automatic_compression_blocked_locally(ignore_cooldown=...)`
    (lines 4031-4140) - the gate; the structural leg (lines 4056-4066) is
    evaluated regardless of `ignore_cooldown`.
  - `_automatic_compression_blocked(ignore_cooldown=...)` (lines 4009-4029) -
    the public gate that refreshes durable guards then re-evaluates locally.
  - `should_compress_info(prompt_tokens)` (lines 3921-3952) - the caller-facing
    `(should, reason)` tuple; short-circuits `(False, None)` under threshold.
  - `compress(..., force=...)` (line 8038) - the three in-method caller reasons
    that arm the backoff: `insufficient_messages` (lines 8123-8136),
    `no_compressible_window` (lines 8182-8201), `empty_post_handoff_window`
    (lines 8317-8339); and the `force=True` override that zeroes the backoff
    (line 8119).
- `agent/conversation_compression.py`
  - the commit-layer dead-loop breaker (lines 4452-4493) invokes
    `_record_structural_no_op("compaction returned the transcript unchanged
    (no_progress)")` when a fired compaction returns an equal transcript.
  - `_automatic_gate_blocked(...)` (lines 2022-2038) and
    `_mark_compression_blocked_transient(...)` (lines 2065-2095) classify the
    `structural_backoff:*` reason as a **transient** guard alongside
    `cooldown:*` (contrast the permanent `ineffective`).

## Coverage: runtime vs pure decision

**Driven end-to-end through the real `compress()` method (runtime coverage):**

- `caller_insufficient_messages` - a 2-message transcript returns unchanged and
  arms the backoff with `failure_class=insufficient_messages`, breaker
  untouched.
- `caller_no_compressible_window` - `_find_tail_cut_by_tokens` pinned so
  `compress_start >= compress_end`; arms with `no_compressible_window`.
- `caller_empty_post_handoff_window` - a standalone handoff fills the window,
  `_generate_summary` is asserted **not** called, arms with
  `empty_post_handoff_window`.
- `forced_compress_overrides` - `compress(force=True)` with a scripted
  `call_llm` clears the backoff and commits a real boundary (8 -> 6 messages).

**Driven through the real gate / status methods (runtime coverage):**

- `initialization` - `__init__`, `on_session_reset`, `on_session_end`,
  `bind_session_state` all observed re-zeroing an armed field.
- `status_while_active`, `expiry` - `_compression_block_reason`,
  `_automatic_compression_blocked_locally`, and `should_compress_info` read
  under a monkeypatched monotonic clock across the 300s window (arm, midwindow,
  exact boundary, past deadline).
- `overflow_bypass` - `_automatic_compression_blocked_locally` /
  `_automatic_compression_blocked` with `ignore_cooldown=True`.
- `non_interaction` - reason precedence and breaker independence via the real
  reason method.

**Executed real method but caller plumbing is a pure/deferred decision:**

- `caller_no_progress_commit_layer` - the recorder itself
  (`_record_structural_no_op`) is executed with the exact reason string the
  commit path passes. The surrounding `conversation_compression` commit/rotate
  plumbing (the `compressed == messages_before_compression` equality leg, the
  session split, the telemetry emit) is **deferred to Rust integration**: it is
  a several-thousand-line generator and the only backoff-relevant effect is this
  one recorder call, which is covered directly. `record_completed_compaction`
  is likewise the real method but is invoked directly rather than through a full
  boundary commit.

## Determinism

- Only `time.monotonic` is patched, to a settable `Clock` at base `10000.0`, so
  every `_structural_no_op_backoff_until` (`10300.0`) and every
  remaining-seconds read is stable. `time.time` is left real; no wall-clock
  value is recorded.
- The model-window probe (`get_model_context_length`) is pinned to `100000`
  during construction so no `/models` call fires.
- Persistence is left unbound (`session_db=None`): the durable-guard refresh in
  `_automatic_compression_blocked` no-ops, which is the exact shape the
  in-memory backoff must survive.
- No network, timestamps, randomness, absolute paths, or credentials enter a
  golden. `--check` regenerates in memory and byte-compares against the
  checked-in fixture.

## Parity traps for Rust

1. **Strictly greater, not greater-or-equal.** Both the reason and the gate use
   `remaining > 0` where `remaining = until - monotonic()`. At exactly the
   deadline `remaining == 0`, so the backoff has **lapsed**
   (`expired_exact_boundary_lapsed`). A `>=` port would hold one extra tick.

2. **Absolute deadline, not additive.** Re-arming sets
   `until = now + 300`, replacing any prior deadline; it does not add 300 to the
   remaining time (`record_rearm_slides_deadline`). A later arming slides the
   deadline forward to the new `now + 300`.

3. **Reason-string precedence is fixed: cooldown > structural > ineffective.**
   `_compression_block_reason` checks cooldown first, then structural, then the
   latched breaker. When several are active the earliest-checked wins the
   string (`cooldown_precedes_structural_in_reason`,
   `structural_precedes_ineffective_in_reason`). The remaining seconds are
   formatted `:.0f` (e.g. `structural_backoff:300`), so port the rounding, not a
   raw float.

4. **Overflow bypass ignores the cooldown but NOT the structural backoff.**
   `ignore_cooldown=True` (provider-proven overflow recovery, #100661) only
   drops the cooldown leg. The structural leg is evaluated unconditionally, so
   an armed backoff still blocks the overflow attempt
   (`overflow_bypass_still_blocked_by_structural` vs
   `overflow_bypass_clears_cooldown_only`).

5. **The backoff is purely in-memory and transient; the breaker is durable and
   permanent.** `_record_structural_no_op` never touches
   `_ineffective_compression_count` or `_fallback_compression_streak`, has no DB
   persistence leg, and needs no recovery-probe ladder (unlike the anti-thrash
   breaker's `_ANTI_THRASH_RECOVERY_SECONDS` deadline). Once the timer lapses a
   still-latched breaker keeps blocking
   (`ineffective_survives_structural_expiry`). Do not fold the two into one
   counter.

6. **Both a completed boundary and manual `/compress` clear it, structural
   no-ops arm it.** `record_completed_compaction` (including the
   `feasibility_skip` branch) and `compress(force=True)` zero the field; the
   three in-`compress` structural branches plus the commit-layer no-progress
   path arm it. Keep every arm site and every clear site wired.

7. **The reason is classified transient for exhaustion semantics.**
   `structural_backoff:*` (like `cooldown:*`) is a transient defer; the overflow
   loop must NOT count it toward `compression_exhausted` (which would let a real
   `context_length_exceeded` wipe a merely-deferred session, #97488). Only the
   permanent `ineffective` breaker feeds exhaustion. Port the prefix
   classification in `_mark_compression_blocked_transient` exactly.

## Verification

- `.venv/bin/python rust/tools/compression-structural-backoff-oracle.py` writes
  24 cases; `--check` returns OK against the checked-in corpus.
- No em dashes, no timestamps, no absolute paths, no monotonic base leak beyond
  the recorded fixed constants in any of the three files.
- Relevant Python tests: 46 passed across
  `test_context_compressor_structural_backoff`, `test_compaction_anti_thrash`,
  `test_compression_anti_thrash_persistence`, `test_estimator_parity_84371`
  (no-progress dead-loop breaker), and `test_context_compressor_summary_continuity`
  (empty-post-handoff structural no-op).
