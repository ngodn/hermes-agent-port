# Full-compression handoff layer: source-executed oracle

Companion analysis for `rust/tools/compression-handoff-oracle.py` and its
checked-in corpus `rust/tools/compression-handoff-goldens.json` (60 cases). The
oracle drives the real Python handoff functions and records their genuine
returns. No decision is reimplemented: the one imperative decision that is not a
standalone function (summary role selection) is compiled verbatim from source
and executed against controlled inputs.

Regenerate and verify from the repo root with the project virtualenv:

```
.venv/bin/python rust/tools/compression-handoff-oracle.py
.venv/bin/python rust/tools/compression-handoff-oracle.py --check
```

## Scope

In scope (the handoff layer):

- exact summary prefix, historical heading, end marker, continuation strings,
  merge delimiters, metadata keys, and every frozen historical prefix;
- prefix recognition / stripping / re-normalization;
- context-summary and synthetic-user classification after SessionDB projection
  strips private metadata;
- summary role selection and collision behavior across user / assistant /
  tool-call template-visible combinations, including merge-into-tail and
  forced-user-leading;
- stripping / unwrapping standalone and merged old handoffs on re-compression;
- zero-real-user continuation insertion and real-user anchor preservation;
- `reference_handoff_would_drive_next_model_call` across standalone handoff,
  later real user input, tool results, pending assistant tool calls, and
  composite carriers.

Out of scope on purpose: token-tail selection and `min_tail_user_messages`
(AGY owns that independent lane), and todo snapshot insertion. The role-
selection cases feed controlled head/tail role lists straight into the real
decision block, so the handoff lane is exercised without entangling AGY's
sizing lane.

## Runtime coverage vs pure constants

Two kinds of golden live in the corpus and the Rust port should treat them
differently.

### Pure constants (`constants` section, no runtime)

These are frozen wire/recognition strings recorded verbatim. They carry no
behavior, only byte parity:

- `SUMMARY_PREFIX`, `LEGACY_SUMMARY_PREFIX`, `HISTORICAL_TASK_HEADING`
  (`agent/context_compressor.py:248-286`).
- `_SUMMARY_END_MARKER` (`agent/context_compressor.py:525-528`).
- `_MERGED_PRIOR_CONTEXT_HEADER`, `_MERGED_SUMMARY_DELIMITER`
  (`agent/context_compressor.py:536-537`).
- `COMPRESSED_SUMMARY_METADATA_KEY`, `COMPRESSED_SUMMARY_HAS_USER_TURN_KEY`
  (`agent/context_compressor.py:301-302`).
- `COMPRESSION_CONTINUATION_USER_CONTENT`,
  `_LEGACY_COMPRESSION_CONTINUATION_USER_CONTENT`,
  `MAX_ITERATIONS_SUMMARY_REQUEST` (`agent/context_compressor.py:320-337`).
- the full `_HISTORICAL_SUMMARY_PREFIXES` tuple, 5 entries, newest first
  (`agent/context_compressor.py:706-837`).

These constants contain em dashes; the oracle prose and this document do not.
Byte parity requires reproducing the em dashes exactly (U+2014).

### Runtime coverage (every other section)

Each recorded value is the return of a real function executed under a pinned
model window (`get_model_context_length` patched so construction never probes
`/models`). No network, clock, randomness, timestamps, credentials, or absolute
paths enter any golden.

- `prefix_recognition`: `ContextCompressor._starts_with_summary_prefix`,
  `_strip_summary_prefix`, `_with_summary_prefix`, `classify_summary_content`
  (`agent/context_compressor.py:5848-5919`). Covers current / legacy / all five
  historical prefixes / merged carrier / plain text / heading-quote.
- `classification`: `_is_context_summary_content`,
  `_is_synthetic_compression_user_turn`, `_is_actionable_user_turn`,
  `_transcript_has_real_user_turn` (`agent/context_compressor.py:5922-6084`,
  `5938-6002`), `is_compaction_summary_message`
  (`agent/context_compressor.py:8915-8935`), and
  `_is_real_user_message` (`agent/conversation_compression.py:2913-2933`). Rows
  are passed through `project_persisted` (drops every `_`-prefixed key) to
  reproduce the SessionDB round-trip, so content markers, not the private flag,
  are what classify them.
- `role_selection`: the inline decision block from
  `ContextCompressor.compress` (`agent/context_compressor.py:8596-8708`, offsets
  558-670 within the `compress` source that begins at file line 8038). The
  oracle compiles that block verbatim and executes it against controlled
  `compressed` / `tail_messages` / `compress_start`, recording the computed
  `summary_role`, `_merge_summary_into_tail`, `_force_user_leading`,
  `last_head_role`, `first_tail_role`, `first_tail_visible_idx`. It asserts the
  block references only names the oracle supplies, so a future source edit that
  adds a dependency fails loud rather than silently running a stale paraphrase.
- `strip_unwrap`: `_strip_context_summary_handoff_message`
  (`agent/context_compressor.py:6247-6364`). Standalone (dropped -> None),
  merged string carrier (unwrapped to prior content, marker cleared),
  force-user-leading carrier (remainder after end marker), list-content merged
  carrier, and a plain non-summary row (returned as a distinct copy).
- `continuation`: `_ensure_compressed_has_user_turn` /
  `_insert_real_user_anchor` (`agent/conversation_compression.py:3158-3257`).
  Covers `placeholder_appended`, `already_present`, `inserted` at the summary
  boundary, and `inserted` appended after a trailing summary.
- `reference_handoff`: `reference_handoff_would_drive_next_model_call`
  (`agent/context_compressor.py:9173-9232`) across all required carriers.

## Relevant Python tests (context, not executed by the oracle)

- `tests/agent/test_summary_prefix_semantics.py` byte-pins every historical
  prefix and the current prefix.
- `tests/agent/test_context_compressor.py` and the compaction/summary-continuity
  suites cover role selection, merge-into-tail, strip/unwrap, and the reference
  handoff drive.

## Parity traps for the Rust port

1. The private `_compressed_summary` marker never survives SessionDB projection.
   On the far side of persistence a handoff is recognized only by its content
   prefix (or, for merged carriers, the prefix after `_MERGED_SUMMARY_DELIMITER`).
   The Rust port must keep the content recognizers as the authoritative fallback,
   not rely on the metadata flag alone.

2. Template-visible role, not literal list neighbor. Role selection alternates
   the summary against `_template_visible_role`, which returns `None` for
   `role="tool"` and for assistant messages carrying `tool_calls`. A head that
   literally ends `[user, assistant(tool_calls), tool]` has template-visible last
   role `user`, so the summary is `assistant`. Selecting against the literal last
   role would emit `user` behind that head and poison Mistral-strict backends
   with a Jinja alternation 500.

3. The collision flip-to-opposite branch is effectively unreachable and must not
   be "fixed." `last_head_role` is only ever `user`, `assistant`, `system`, or
   `None` (tool is never template-visible). On a collision the flipped role
   always equals `last_head_role` (or `_force_user_leading`/`None` blocks it), so
   the code falls through to merge-into-tail every time. The oracle exercises the
   merge outcome directly; do not port the flip as a live path or add a
   speculative test that expects it to fire.

4. Two distinct composite layouts, opposite extraction rules.
   `_MERGED_SUMMARY_DELIMITER` (ordinary merge-into-tail) keeps the live content
   *before* the delimiter; force-user-leading keeps the live content *after*
   `_SUMMARY_END_MARKER`. `_strip_context_summary_handoff_message` and
   `_handoff_only_content` are exact inverses of each other over these two
   layouts. Getting the split side wrong silently drops or leaks live user text.

5. `_force_user_leading` fires on three independent conditions: `compress_start
   == 0`, `last_head_role == "system"`, or the zero-nonempty-user-turn guard
   (no `role="user"` message with non-empty *text* survives head or tail). The
   last one counts text, not role: an image-only user row is `role="user"` but
   text-empty and does not satisfy the guard. Once forced, the flip logic is
   suppressed so the summary stays `user`.

6. `reference_handoff_would_drive_next_model_call` treats a completed merged
   *assistant* carrier (`finish_reason == "stop"`, no `tool_calls`, classified
   `merged`) as a driving handoff, but a merged assistant carrier with pending
   `tool_calls` as a live exchange (does not drive). User-role carriers that
   still embed a live ask never drive. A trailing synthetic continuation row does
   not count as a real user turn, so the handoff still drives. Port each of these
   branches exactly; the corpus pins all of them.

7. The real-user anchor is inserted at the summary boundary (before the first
   assistant not already preceded by a user), not blindly appended. It is only
   appended after a trailing user row when that row is a compaction summary
   (never merged into it, so the summary prefix stays at the message start). A
   trailing non-summary user-role scaffolding row is merged into instead. The
   `placeholder_appended` fallback uses the exact
   `COMPRESSION_CONTINUATION_USER_CONTENT` string.

8. `_strip_summary_prefix` strips the merge wrapper (everything up to and
   including `_MERGED_SUMMARY_DELIMITER`), then the current/legacy/any historical
   prefix, then truncates at `_SUMMARY_END_MARKER` even when the marker is not
   the final content (force-user-leading bodies keep a live ask after it). All
   three steps must run in that order or a stale directive or the live ask leaks
   into the next summarizer prompt.
