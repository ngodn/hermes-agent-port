# Native micro-compaction source map

This is the corrected, condensed result of the AGY source-mapping lane. The
original helper report was checked against the current Python and Rust trees.

## Python behavior

Configuration is declared in `hermes_cli/config_defaults.py` and parsed in
`agent/agent_init.py`.

- `compression.micro_compact` defaults to `false` and uses
  `utils.is_truthy_value` coercion.
- `compression.micro_compact_every_n_turns` defaults to `1`. Booleans and
  fractional numbers are rejected, string integers are accepted, and the
  result is clamped to at least `1`.
- `compression.micro_compact_defrag_threshold_tokens` defaults to `2000` and
  uses the same integer rules.
- `compression.checkpoint_required` disables micro-compaction. The init path
  and post-turn finalizer both enforce the gate.

The production order in `agent/turn_finalizer.py` is:

1. Finish and normalize the assistant response.
2. Run one optional micro-compaction pass.
3. Persist the resulting transcript.
4. Run output, external-memory, and context-engine hooks.
5. Allow the next provider request.

The Rust gateway already persists the assistant response before calling
`AgentClient::finalize_turn_after_persist`, while its durable turn lease is
still held. That finalizer is therefore the corresponding safe runtime seam.

## State machine

The implementation in `agent/context_compressor.py` has these invariants:

- A due pass absorbs at most one complete assistant/tool exchange.
- User messages are never summarized or removed. Supersession may merge two
  adjacent plain-text user messages with `\n\n`, preserving their bytes and
  clearing stale `api_content`.
- The protected head and token-selected tail use the same boundaries as batch
  compression. Tool groups are never split.
- A cumulative pass replaces the older contained micro marker. It never drops
  a batch marker unless its body was first rehydrated as the rolling base.
- The cursor is recomputed from the spliced transcript, immediately after the
  surviving marker.
- Three consecutive failures on one exchange advance past that poison
  exchange without deleting it.
- Defragmentation has priority over absorbing another exchange and consumes
  the pass's single auxiliary call. It rewrites only the contained marker.
- Full batch compression resets stale rolling state. A resumed process
  reconstructs it from the newest durable summary marker.

## Auxiliary request

`_micro_summarize_one` uses the compression auxiliary task with:

- one tool-free request per due pass;
- temperature `0.1`, subject to provider-specific fixed or omitted
  temperature rules;
- at most `min(1500, max_summary_tokens)` output tokens;
- the existing rolling summary plus one serialized exchange;
- strict secret redaction before request and persistence;
- inline reasoning removal;
- rejection of empty output and `finish_reason=length`.

The exchange serializer labels assistant and tool rows, retains tool names and
bounded arguments, converts images to short labels, replaces `MEDIA:` delivery
directives, strips assistant reasoning blocks, and bounds large content using
head and tail sampling.

## SQLite publication

Python `SessionDB.archive_and_compact` publishes the entire replacement inside
one transaction. The native publisher must preserve the same outcome with
stronger stale-write guards:

- no network call or mutex wait inside the write transaction;
- require the unexpired conversation turn lease and exact active snapshot;
- reject closed or missing sessions and invalid role/tool sequences;
- archive absorbed assistant/tool rows as `active=0, compacted=1`, so search
  can still find them;
- archive carried-forward originals as `active=0, compacted=0`, so their fresh
  active clones do not duplicate recall results;
- clone retained wide rows in SQL, changing only content, `api_content`, and
  `_compressed_summary` when the candidate explicitly rewrites them;
- insert one new assistant summary marker for an absorption pass, or rewrite
  the contained marker for a defrag pass;
- reconcile active message and tool-call counters in the same transaction;
- commit before advancing in-memory rolling state.

## Native seams verified

- Policy and coercion: `rust/crates/hermes-gateway/src/automatic_compression.rs`
- Pure state machine: `rust/crates/hermes-gateway/src/micro_compaction.rs`
- Guarded publication: `rust/crates/hermes-gateway/src/session_db.rs`
- Auxiliary request and post-turn ordering:
  `rust/crates/hermes-gateway/src/native_agent.rs`
- Durable invocation forwarding:
  `rust/crates/hermes-gateway/src/conversation_agent.rs`

The helper's initial suggestion to add a `config_types.rs` field and route the
work through `session_store.rs` was not applicable to this Rust tree. The live
configuration is already frozen into `AutomaticCompressionPolicy`, and the
post-turn client holds the exact `SessionDb` plus lease identity.

## Python tests to retain as compatibility checks

- `tests/agent/test_micro_compaction.py`
- `tests/agent/test_compressor_truncated_summary_guard.py`
- `tests/agent/test_pre_compress_checkpoint_contract.py`
- `tests/agent/test_compressed_summary_metadata.py`
- `tests/hermes_state/test_86366_tail_rewind_semantics.py`
- `tests/run_agent/test_in_place_persist_marker_98450.py`
