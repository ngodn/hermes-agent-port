# Tool-Result Pruning Port Analysis and Verification

Port specification and verification notes for the deterministic, no-LLM
pre-compression tool-result pruning module in the Rust rewrite of Hermes
Gateway (`hermes-gateway`).

The Rust module is `rust/crates/hermes-gateway/src/tool_result_prune.rs`. It is
a pure function library: it never mutates its input, never touches SQLite or
ingress, and returns a fresh candidate list plus explicit change and reclaim
metadata. The commit decision (prompt-cache no-op contract, reclaim/rearm
gates, persistence) stays with the caller, exactly as the task scoped it.

## 1. Authoritative Python Source Locations

All line numbers are against the tree at port time.

- `agent/context_compressor.py:4146-4447`: `ContextCompressor._prune_old_tool_results`,
  the multi-pass prune. This module ports its count-based deterministic subset.
- `agent/context_compressor.py:4519-4600+`: `ContextCompressor.prune_tool_results_only`,
  the cost-oriented caller that runs `_prune_old_tool_results` with
  `protect_tail_tokens=None` (COUNT-based tail protection only). Its docstring
  is the authoritative description of which passes run in the deterministic path.
- `agent/context_compressor.py:2100-2125`: `_summarize_tool_result` (guard wrapper).
- `agent/context_compressor.py:2128-2304`: `_summarize_tool_result_unguarded`
  (per-tool summary branches and the generic fallback).
- `agent/context_compressor.py:2085-2097`: `_str_arg` (Python-`str()` coercion of
  model-supplied argument values).
- `agent/context_compressor.py:1823-1866`: `_truncate_tool_call_args_json`
  (JSON-valid string-leaf shrink).
- `agent/context_compressor.py:1773-1803`: `_retire_stale_tool_result_images`.
- `agent/context_compressor.py:1744-1770`: `_strip_images_from_tool_msg`.
- `agent/context_compressor.py:1708-1741`: `_strip_image_parts_from_parts`,
  `_tool_content_has_images`, and image-part predicates.
- `agent/context_compressor.py:1295-1333`: `_collect_protected_skill_names`.
- `agent/context_compressor.py:1265-1292`: `_skill_view_call_sites`.
- `agent/context_compressor.py:895-912`: `_is_clarify_non_response_sentinel`
  and `agent/context_compressor.py:887-892`: `_CLARIFY_NON_RESPONSE_PREFIXES`.
- `agent/context_compressor.py:933-943`: `_skill_pruned_marker`.
- `agent/context_compressor.py:874-930`: the constants
  (`_PRUNED_TOOL_PLACEHOLDER`, `_PRUNE_MIN_CHARS`, `_SKILL_VIEW_PRUNE_MIN_CHARS`,
  `SKILL_PRUNED_MARKER_PREFIX`) and `agent/context_compressor.py:1262`,
  `1374`, `1381` for `_SKILL_PRUNE_RECENT_WINDOW`,
  `_PRESSURE_KEEP_RECENT_MESSAGES`, `_MAX_KEEP_TOOL_IMAGES`.
- `agent/turn_context.py:189-198`: `drop_stale_api_content` (dropping the wire
  sidecar when content is rewritten by an image strip).

## 2. Message Model

The module works with `crate::session_db::CompressionHistoryMessage` unchanged
(`session_db.rs` was not edited):

- `message.content: String` is the clean transcript content. Structured
  (multimodal) content is stored JSON-sentinel-encoded; `HistoryMessage::model_content()`
  decodes it back to a `serde_json::Value` (string, array of parts, or the
  `{_multimodal: true, ...}` envelope object). `encode_message_content()`
  re-encodes on write. Both are the existing `session_db` round-trip helpers.
- `message.api_content: Option<String>` is the exact model-wire copy. Per the
  Python image-strip path it is dropped only when an image payload is rewritten
  (`drop_stale_api_content`). Summarization and dedup leave it intact, matching
  Python (which rewrites `{**msg, "content": ...}` and preserves other keys).
- `tool_call_id: Option<String>` links a `tool` row to the assistant call.
- `tool_calls: Option<String>` is the assistant call array as JSON text
  (`[{id, type, function:{name, arguments}}, ...]`), where `arguments` is itself
  a JSON-encoded string, matching the OpenAI wire shape.
- `tool_name: Option<String>` exists on the row but is deliberately NOT consulted
  for summaries: Python derives the tool name only from the assistant
  `tool_calls` index keyed by `tool_call_id`, so the port mirrors that and falls
  back to `("unknown", "")` on an index miss.

## 3. Public API

```rust
pub struct PruneOutcome {
    pub messages: Vec<CompressionHistoryMessage>, // fresh candidate; == input when !changed
    pub changed: bool,          // any field differs from input
    pub pruned_count: usize,    // Python `pruned`: dedup + Pass-2 demotions + image retirements
    pub reclaimed_chars: usize, // char reduction across clean content + tool_calls
}

pub fn prune_old_tool_results(
    messages: &[CompressionHistoryMessage],
    protect_tail_count: usize,
    min_prune_chars: usize, // use PRUNE_MIN_CHARS for the default floor
) -> PruneOutcome;

pub fn summarize_tool_result(tool_name: &str, tool_args: &str, tool_content: &str) -> String;
```

`pruned_count` matches Python's `pruned` accumulator exactly: it counts dedup
back-references, Pass-2 demotions (summaries and image strips reached before the
boundary), and Pass-3.5 image retirements. It does NOT count Pass-3 argument
truncation, because Python's Pass 3 never touches `pruned`. `changed` and
`reclaimed_chars` still reflect Pass-3 rewrites so a caller can gate on real
savings.

## 4. Exact Implemented Behavior

Passes run in Python order on a single working clone.

### Boundary (count-based only)
`prune_boundary = len - protect_tail_count` (saturating at 0). Indices
`[0, prune_boundary)` are prunable by Pass 2 and Pass 3; the last
`protect_tail_count` messages are the protected tail. This is the
`prune_tool_results_only` contract (`protect_tail_tokens=None`).

### Pass 1: duplicate tool-output dedup (tail-agnostic, lossless)
Walk newest-first over `tool`-role rows with plain-string content of at least
`PRUNE_MIN_CHARS` (200) chars. The first (newest) occurrence of a given content
is kept; older exact duplicates anywhere in the list, including inside the
protected tail, are replaced with
`[Duplicate tool output - same content as a more recent call]`. Because the
newest full copy always survives, no unique content is ever lost.

Divergence: Python keys on `md5(content).hexdigest()[:12]` (48 bits). The port
keys on the exact content string. This is observationally identical for real
transcripts and strictly safer: no hash collision can ever back-reference two
genuinely different outputs.

### Pass 2: summarize old tool results
For each prunable `tool` row, `_demote_tool_result_at` is ported with the exact
guard order:
1. Image-bearing content (part list with an image, or a `_multimodal` envelope)
   goes through the image strip (same policy as Pass 3.5) and drops `api_content`.
2. Non-string, non-image content is left untouched.
3. Empty content, the generic placeholder, a `[Duplicate tool output` back-ref,
   an already-summarized `[... chars)` line shorter than 400 chars, and a
   `[screenshot removed` line are all skipped.
4. Content at or below `min_prune_chars` is skipped.
5. `skill_view` bodies for protected skills are skipped (ghost-skill defense).
6. Otherwise the content is replaced with `summarize_tool_result(...)`.

`summarize_tool_result` ports every branch from the Python unguarded builder:
`terminal`, `read_file`, `write_file`, `search_files`, `patch`, the `browser_*`
family, `web_search`, `web_extract` (including unwrapping the `{url|href}` dict
shape), `delegate_task`, `execute_code`, `skill_view` (with the ghost-skill
`[SKILL_PRUNED: ...]` marker above 5000 chars), `skills_list` / `skill_manage`,
`vision_analyze`, `memory`, `todo_list`, `clarify`, `text_to_speech`,
`cronjob_manage`, `process_manage`, and the generic fallback. Char lengths use
Unicode scalar counts (`len()` parity, not bytes) and `{n:,}` thousands grouping
is reproduced. The `terminal` exit-code and `search_files` total-count fields are
extracted with a manual JSON-int scanner (`extract_json_int`) since the crate has
no `regex` dependency; the scanner reproduces `"field"\s*:\s*(-?\d+)` including
searching past a first non-numeric occurrence.

`clarify` ports the answer-shaped detection, the non-response sentinel guard
(`The user did not provide a response`, `[user did not respond`,
`[clarify prompt could not be delivered`, `[oneshot mode:`), and the
199-char / `...[truncated]` cap. The serialized answer uses `serde_json`
(equivalent to `ensure_ascii=False`); list answers use Python's `", "` item
separator.

### Pass 3: truncate oversized tool_call arguments
For prunable assistant messages, each tool call whose `function.arguments` JSON
string exceeds 500 chars is shrunk with `truncate_tool_call_args_json`: parse the
arguments, recursively truncate string leaves longer than 200 chars to
`head + "...[truncated]"`, leave non-string leaves intact, and re-serialize. A
non-JSON arguments string is returned unchanged. The whole `tool_calls` array is
then re-serialized back into the text column, so downstream JSON stays valid.

### Pass 3.5: retire stale tool-result images (tail-agnostic, lossy)
Walk newest-first, keep the newest `MAX_KEEP_TOOL_IMAGES` (3) image-bearing
`tool` rows verbatim, and retire the rest. Part-list content has image parts
swapped for `[screenshot removed to save context]` text parts; the `_multimodal`
envelope collapses to `[screenshot removed] <text_summary truncated to 200>`.
Every rewrite drops `api_content`.

## 5. Omitted Python Passes (explicit non-parity)

These are intentionally NOT ported and are called out so nothing pretends parity:

- Token-budget boundary mode (`protect_tail_tokens`) and the entire
  `_estimate_msg_budget_tokens` walk it drives (stale-thinking charging,
  `_ALWAYS_REPLAYED_BUDGET_KEYS` / `_NEWEST_TURN_ONLY_BUDGET_KEYS`,
  `reasoning_details` text accounting, `estimate_tokens_rough` with its CJK
  density rules). This is a large estimator surface that cannot be proven without
  porting the whole token-accounting layer, and the deterministic cost path
  (`prune_tool_results_only`) never uses it. The Rust module offers only the
  count-based boundary.
- Pass 4, the protected-tail pressure demotion
  (`agent/context_compressor.py:4372-4445`). It runs only when
  `protect_tail_tokens` is set, so it is out of scope for the count-based path.
  Its `_PRESSURE_KEEP_RECENT_MESSAGES` last-resort demotion is not implemented.
- The commit layer in `prune_tool_results_only`
  (`agent/context_compressor.py:4519+`): `proactive_prune_tokens` gate,
  `_proactive_prune_rearm_tokens` hysteresis, `proactive_prune_min_reclaim_tokens`,
  `_billed_basis_over_threshold`, `_warn_reclamation_no_op`, and the
  `archive_and_compact` capability gate. These are ingress/SQLite concerns the
  task explicitly excluded; `PruneOutcome.changed` and `reclaimed_chars` give a
  caller everything needed to reimplement the gate.
- `_prune_stale_reasoning_replay` (`agent/context_compressor.py:450-517`): a
  separate function, not part of `_prune_old_tool_results`.

## 6. Minor Documented Divergences

- Dedup keys on exact content, not `md5[:12]` (safer; see Pass 1).
- `api_content` is preserved on summarize and dedup (Python parity) and dropped
  only on image strip. On the Rust store `api_content` carries the real wire
  bytes, so a summarized row still ships its full original output on replay until
  a caller chooses to drop the sidecar. This mirrors Python exactly; a caller
  wanting maximal reclaim can drop `api_content` on any rewritten row itself.
- The generic-fallback summary and its "first two args" iterate `serde_json`
  object keys, which are sorted (no `preserve_order` feature) rather than
  insertion-ordered as in Python dicts. This affects only unknown tools; every
  named tool reads specific keys, so ordering is irrelevant there.
- `json_value_to_str` reproduces Python `str()` for the value kinds that appear
  in tool arguments (`True`/`False`/`None`, numbers, raw strings). Nested
  dict/list values fall back to compact JSON rather than Python `repr`; this only
  reaches the generic fallback of unknown tools.
- Lone UTF-16 surrogate `backslashreplace` in the `clarify` serializer is a
  Python-only concern (Rust `String` cannot hold lone surrogates) and is a no-op
  here.

## 7. Safety Invariants

- Input is never mutated: the function clones into a working `Vec` and the input
  slice is untouched (asserted by a test).
- Complete tool-call groups are preserved: no message is added or removed, only
  content and `tool_calls` fields are rewritten in place, so every assistant
  call keeps its matching `tool` result row and `tool_call_id`.
- Dedup is lossless: the newest full copy of any content always survives.
- Idempotence: a second run over a pruned list is a no-op (the "already
  summarized" and size guards absorb prior output), asserted by a test.
- Never panics: `summarize_tool_result` degrades to the generic shape on any
  malformed argument JSON, mirroring the Python guard wrapper.
- Char-length parity: all size gates count Unicode scalar values, not bytes.

## 8. Test Inventory (inline `#[cfg(test)]`)

- `no_op_input_returns_unchanged`: nothing prunable, `changed == false`.
- `empty_input_is_noop`: empty list.
- `protected_tail_is_untouched`: count boundary suppresses Pass 2; tail-agnostic
  dedup still fires.
- `duplicate_outputs_keep_newest_full_copy`: dedup keeps the newest copy.
- `dedup_below_floor_is_skipped`: sub-200-char duplicates are left alone.
- `large_old_tool_result_is_summarized`: terminal summary + reclaim accounting.
- `tool_call_identity_drives_summary`: summaries resolve through `tool_call_id`
  into the correct read_file / search_files shapes.
- `structured_image_content_is_retired`: part-list image retirement keeps newest 3.
- `multimodal_envelope_is_collapsed`: envelope collapse and `api_content` drop.
- `tool_call_args_truncated_outside_tail`: Pass 3 shrink stays valid JSON.
- `skill_view_body_protected_when_recently_loaded`: ghost-skill protection.
- `skill_view_demoted_with_marker_when_not_protected`: marker emitted past the
  recent window.
- `idempotent_second_pass_is_noop`: idempotence.
- `input_is_never_mutated`: input immutability.
- `clarify_answer_is_quoted` / `clarify_sentinel_is_generic`: clarify branch.
- `comma_formatting_matches_python` / `extract_json_int_handles_negative_and_missing`:
  formatting and scanner helpers.

## 9. Integration Guidance

- This module is not yet wired into the crate module tree. The main agent should
  add `mod tool_result_prune;` to `rust/crates/hermes-gateway/src/main.rs`
  (kept out of this lane, which owns only the new file). It compiles standalone
  against `session_db` and `serde_json`, both already crate dependencies; no new
  crates are needed (the regex-free `extract_json_int` scanner is why).
- A caller reproducing `prune_tool_results_only` should: call
  `prune_old_tool_results(msgs, protect_last_n, PRUNE_MIN_CHARS_or_config)`, then
  gate the commit on `changed` plus its own reclaim threshold using
  `reclaimed_chars` (converted to a token estimate) before persisting. Treat
  `changed == false` as the no-op "return the input object" case.
- The recent-tail protection here is by message COUNT. The token-budget path and
  its pressure demotion must be ported separately if the full-compression trigger
  ever needs the deterministic prune (see section 5).
- If maximal reclaim matters on the Rust store, a caller may additionally clear
  `api_content` on any row whose `content` this module rewrote; that is a
  deliberate step beyond Python parity (section 6).
