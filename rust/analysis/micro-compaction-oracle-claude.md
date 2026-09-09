# Micro-compaction oracle

Source-executed golden corpus for the Python `ContextCompressor` micro-compaction
state machine. It records CPython behaviour used to derive the focused Rust
contract tests rather than relying on a paraphrase. Direct JSON replay from Rust
is not wired in this checkpoint.

- Generator: `rust/tools/micro-compaction-oracle.py`
- Golden: `rust/tools/micro-compaction-goldens.json` (21 cases)
- Under test: `agent/context_compressor.py`

## What micro-compaction does

Micro-compaction amortizes context compression. Instead of one long pause when the
window fills, each due turn folds the single oldest un-absorbed assistant/tool
exchange into a rolling summary, splices that span out, and leaves one cumulative
`assistant`-role summary marker in its place. User turns are never absorbed.

## Authoritative functions and constants

Everything is driven through the real methods; nothing is reimplemented in the
oracle.

- Entry point / cadence gate / defrag branch / failure accounting:
  `ContextCompressor._micro_compact` (`agent/context_compressor.py:7589`)
- Cursor derivation and resume rehydration:
  `ContextCompressor._resolve_compact_cursor` (`:7265`)
- Single-exchange (full-turn) walk with the splice-boundary guard:
  `ContextCompressor._find_one_exchange` (`:7314`)
- Auxiliary summarizer (patched at its `call_llm` seam):
  `ContextCompressor._micro_summarize_one` (`:7451`),
  `_build_micro_summary_prompt` (`:7418`), `_serialize_one_exchange` (`:7404`)
- Defrag: `_needs_defrag` (`:7520`), `_defrag_rolling_summary` (`:7525`)
- Splice, supersession, user-merge:
  `_splice_micro_compact_result` (`:7894`), `_merge_adjacent_user_turns` (`:7996`),
  `_cursor_after_splice` (`:7763`)
- Marker rendering / extraction:
  `_render_micro_marker_content` (`:7986`), `_rolling_summary_from_marker` (`:7740`)
- Window helpers: `_protect_head_size` (`:6563`), `_align_boundary_forward` (`:6503`),
  `_find_tail_cut_by_tokens` (`:7089`)
- Persistence seam (left unbound so it no-ops): `_sync_micro_compact_to_db` (`:7854`)
- Constants: `MICRO_COMPACT_MARKER_KEY` (`:308`), `COMPRESSED_SUMMARY_METADATA_KEY`
  (`:301`), `COMPRESSED_SUMMARY_HAS_USER_TURN_KEY` (`:302`), `_DB_PERSISTED_MARKER`
  (`:309`), `SUMMARY_PREFIX` (`:251`), `HISTORICAL_TASK_HEADING` (`:248`),
  `_SUMMARY_END_MARKER` (`:525`),
  `_MICRO_COMPACT_MAX_CONSECUTIVE_FAILURES` (`:857`)

## What is patched, and what is not

The task allows patching only the auxiliary summarizer plus any time/persistence
dependency needed for deterministic pure execution.

- **Auxiliary summarizer** is the one real external dependency.
  `_micro_summarize_one` imports `call_llm` and `aux_interrupt_protection` from
  `agent.auxiliary_client` lazily, so they are patched there. `call_llm` is
  replaced by a scripted stand-in: each case carries a `script` list, one entry
  consumed per aux call, in order across all passes (a defrag pass consumes one
  too). Entry kinds: `ok` (content becomes the summary), `empty`, `length`
  (finish_reason=length), `raise`. This makes the summarizer a replayable
  contract, so the golden pins the state machine, not the model. Focused native
  tests exercise the corresponding success, empty, partial, and failure paths
  independently.
- **`aux_interrupt_protection`** is replaced with a null context manager so no
  signal handling runs in the generator.
- **`get_model_context_length`** is replaced with a fixed 40960-token window
  during construction and budget resolution, so nothing touches the network. This
  is the same window the Python tests use.
- **Persistence is not patched.** The session DB is left unbound, so
  `_sync_micro_compact_to_db` returns early. That is the exact shape the in-memory
  splice has to survive, and it is what lets the `_db_persisted` stamp case be
  meaningful.
- **Time is not patched.** Duration is only ever logged (telemetry), never
  returned or recorded, so no timestamp can enter a golden. Nothing else in the
  path depends on wall-clock or monotonic time for its result.

No timestamps, random data, absolute paths, network, or credentials enter the
golden. The em dashes that appear inside marker `content` are the verbatim
`SUMMARY_PREFIX` / end-marker product strings, required for faithful marker
rendering, not oracle prose.

## Record shape

Each case records `config`, `initial_state` (the in-memory micro state the pass
starts from), the pristine `input` message list, the `script`, and per pass the
full `output` list, the observable `state` (cursor, rolling summary, both failure
counters, pass and token-saved totals, cadence counter, flush-cursor flag), the
`marker_count`, and the aux calls that pass made. Passes are chained: each pass's
output feeds the next, mirroring how `finalize_turn` re-invokes the compressor.

Resume, defrag, and batch-marker cases seed a realistic `input` + `initial_state`
by first driving a throwaway compressor through the real code (`produce`), then
freezing the concrete result into the case. Those three fields are sufficient
for a future direct native replay harness.

## Coverage

Every required scenario maps to at least one named case:

| Requirement | Case(s) |
| --- | --- |
| disabled and short transcript no-ops | `disabled-no-op`, `short-transcript-no-op` |
| every-N-turn cadence | `cadence-every-third-turn`, `cadence-clamped-to-one` |
| exactly one exchange absorbed per due pass | `absorbs-exactly-one-exchange` |
| protected head + token-selected tail | `protected-head-and-tail-preserved` |
| cumulative supersession, every user byte, strict alternation | `cumulative-supersession-keeps-one-marker` |
| cursor relocation after tool-bearing splices | `cursor-relocates-after-tool-splice` |
| resume rehydration from a micro marker | `resume-rehydrates-from-marker` |
| failed-rehydration preservation | `failed-rehydration-preserves-marker` |
| bounded repeated-summary failure | `bounded-summarize-failure`, `summarize-length-stop-no-op`, `summarize-raise-no-op` |
| poison-exchange cursor skip | `poison-exchange-cursor-skip` |
| defrag success / failure / batch-marker non-rewrite | `defrag-rewrites-marker-in-place`, `defrag-failure-restores-summary`, `defrag-never-rewrites-batch-marker` |
| supersede never drops a batch marker | `supersede-never-drops-batch-marker` |
| marker rendering and rolling-summary extraction | `marker-rendering-and-extraction` |
| stale api_content removal on adjacent user merge | `stale-api-content-dropped-on-user-merge` |
| db-persisted stamps survive the in-place splice | `splice-preserves-db-persisted-stamps` |

Each builder self-asserts its target behaviour before anything is written, so a
drift in the source that changes an outcome fails generation rather than silently
producing a wrong golden.

## Notes on window sizing (for the Rust port)

Two behaviours bit during construction and are worth carrying over:

- The compress window can be empty even on a non-trivial transcript. With the lean
  tail budget (10000-token floor) most short transcripts fit entirely in the tail,
  so `_find_tail_cut_by_tokens` takes its "everything fits, re-walk with the raw
  budget" path and the window ends up near `len - min_tail`. If the transcript is
  too short, `cursor >= compress_end` and the pass no-ops. The
  `stale-api-content-dropped-on-user-merge` case needed trailing filler exchanges
  so pass 2 was actually due to absorb the second exchange and supersede the pass-1
  marker.
- `protect_first_n` is protected as head, so the first real user turn sits in the
  head. Supersession can still merge it with a later user turn (the merge runs over
  the whole result list, head included); that is intended behaviour.

## Verification

Generator writes and re-checks clean:

```
$ .venv/bin/python rust/tools/micro-compaction-oracle.py
Wrote rust/tools/micro-compaction-goldens.json (21 cases).

$ .venv/bin/python rust/tools/micro-compaction-oracle.py --check
OK: rust/tools/micro-compaction-goldens.json matches Python (21 cases).
```

Directly relevant Python micro-compaction and compressor tests pass:

```
$ .venv/bin/python -m pytest tests/agent/test_micro_compaction.py -q
36 passed in 6.03s

$ .venv/bin/python -m pytest tests/agent/test_context_compressor.py \
    tests/agent/test_context_compressor_summary_continuity.py \
    tests/agent/test_context_compressor_session_end_clears_state.py \
    tests/agent/test_pre_compress_checkpoint_contract.py \
    tests/agent/test_compressed_summary_metadata.py -q
196 passed in 4.91s
```
