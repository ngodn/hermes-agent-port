# Adversarial Review: Ollama/GLM stop-to-length truncation correction (Rust checkpoint)

**Scope**: the uncommitted checkpoint that ports Python's conservative local Ollama/GLM
`finish_reason="stop"` to `"length"` correction into Rust.

**Files inspected**
- `run_agent.py` (`_should_treat_stop_as_truncated` 1911-1940, `_is_ollama_glm_backend` 1875-1909, `_has_natural_response_ending` 1855-1873, `_strip_think_blocks` 1850-1853)
- `agent/agent_runtime_helpers.py` (`strip_think_blocks`, tag name tuples 65-66)
- `rust/crates/hermes-gateway/src/ollama_glm_truncation.rs`
- `rust/crates/hermes-gateway/src/native_agent.rs` (streaming call site 4711-4734 in `run_model_turn`, buffered call site 5850-5872 in `step`, `forward_sse` 5563-5690, `observe_main_sse_line` 5499-5533, dispatch/serving-route resolution 4613-4659 and 5747-5872, three new run-turn tests 8415-8804)
- `rust/crates/hermes-gateway/src/visible_response.rs`, `rust/crates/hermes-gateway/src/python_value.rs`
- `rust/tools/gen_ollama_glm_truncation_goldens.py`, `rust/tools/ollama-glm-truncation-goldens.json`
- `rust/analysis/ollama-glm-truncation-contract-agy.md`

**Bottom line**: no correctness finding survived verification against the exact code. The
classifier is a faithful, pure port of the 9-gate Python heuristic, its parity is pinned by 113
source-executed goldens plus three full `run_turn` integration tests, the correction is scoped and
ordered so it cannot double-fire with the stall-synthesized `length` path, and serving-route
identity is read from the dispatched route so it stays correct under fallback and credential pools.
Residual risks are listed separately in section 3; none is a defect in this checkpoint.

---

## 1. What the checkpoint does and why it is correct

`should_rewrite` (`ollama_glm_truncation.rs:20-56`) reproduces `_should_treat_stop_as_truncated`
gate-for-gate, in the same short-circuit order:

| Python gate | Rust line | Verified equivalent |
| :-- | :-- | :-- |
| `finish_reason != "stop"` / `api_mode != "chat_completions"` | 21-23 | exact |
| `_is_ollama_glm_backend()` | 24-26 -> `is_local_ollama_glm` 58-69 | exact (see below) |
| history has `role=="tool"` | 27-33 | `messages.iter().any(... role == Some("tool"))` |
| assistant not None, no truthy `tool_calls` | 35-43 | `python_value::truthy` on `tool_calls` |
| `isinstance(content, str)` | 44-46 | `get("content").and_then(as_str)` -> None for null/list/dict/number |
| visible after think-strip non-empty | 47-49 | `visible_response::strip` then `trim_matches(python_whitespace)` |
| `len >= 20` and has whitespace | 49-54 | `chars().count()` and `chars().any(python_whitespace)` |
| `not _has_natural_response_ending` | 55 -> `has_natural_ending` 71-102 | exact |

Points I checked closely and found correct:

- **Backend identity ordering**. `is_local_ollama_glm` (58-69) applies the cloud exclusion
  (`ollama.com` in URL or `:cloud` in model) before the local signatures (`ollama`/`:11434`/provider
  `ollama`), matching Python's precedence at `run_agent.py:1902-1908`. Cloud wins over local, so a
  `glm-5.1:cloud` proxied through `:11434` is correctly not rewritten. Case-insensitivity via
  `to_lowercase()` matches `.lower()` for the ASCII tokens `glm`, `zai`, `ollama`, `:11434`,
  `ollama.com`, `:cloud`.
- **Natural-ending set**. The 19-character terminal set (`.!?:)"']}` plus the nine CJK marks plus
  `^`), the code-fence check, and the `ord(last) >= 0x1F300` emoji threshold are byte-identical to
  `_has_natural_response_ending`. `u32::from(last)` over the last `char` equals Python `ord(stripped[-1])`
  over the last code point. The `⚠️` golden (last scalar U+FE0F, below the threshold and not in the
  set) is correctly classified as truncated on both sides.
- **Unicode / truthiness parity**. Length is compared in code points on both sides. The whitespace
  floor uses `python_whitespace` (`python_value.rs:286`, `is_whitespace()` plus C0 separators
  U+001C-U+001F), which covers exactly what Python's `re.search(r"\s", ...)` matches, including NBSP,
  NEL, and the four information separators that Rust's `char::is_whitespace()` alone omits. The
  `tool_calls` gate uses `python_value::truthy`, so `null`, `[]`, and missing all behave like Python's
  falsy `getattr(..., None)`.
- **Think-strip parity**. `visible_response::strip` uses the same reasoning tag set
  (`think`, `thinking`, `reasoning`, `REASONING_SCRATCHPAD`, `thought`) and tool tag set
  (`tool_call`, `tool_calls`, `tool_result`, `function_call`, `function_calls`) as
  `agent_runtime_helpers._REASONING_TAG_NAMES` / `_TOOL_CALL_TAG_NAMES`, and it is already covered by
  its own goldens. The classifier strips raw content (with think blocks) exactly as Python passes raw
  `content` into `_strip_think_blocks`.

**Gate ordering / no double-fire.** In `run_model_turn`, a stream that stalls after visible text sets
`outcome.finish_reason = "length"` at `native_agent.rs:5592` and `outcome.stalled = true`. The
rewrite at 4716 requires `finish_reason == "stop"`, so it cannot re-fire on a stalled stream, and the
downstream length handler at 4742 is additionally guarded by `!outcome.stalled`. A genuine clean
`stop` leaves `stalled == false`, so after the rewrite it correctly flows into the existing
length-continuation machinery. This is the sequencing the AGY contract requires.

**Serving-route identity under fallback / credential pools.** Both call sites read identity from the
dispatched route, not the primary: streaming uses `route = main_route(dispatched.route_index)` with
`serving_base_url = dispatched.base_url` (4615-4638), buffered uses `dispatched_route` and
`dispatched.base_url` (5783-5860). If a fallback switches to a non-Ollama provider, the classifier
re-evaluates against that provider and declines the rewrite, which is the correct behavior. A
credential pool that keeps the same base_url/model/provider is unaffected.

**Bucket scoping is correct, not a divergence.** Both sites gate on `usage_bucket == UsageBucket::Main`
(4715, 5853). The only other bucket is `Auxiliary`, used for the context summarizer (3413, 3510).
Python calls `_should_treat_stop_as_truncated` from the main `conversation_loop`, not from the
summarizer, so excluding `Auxiliary` matches Python and is consistent with every other length /
truncation guard in this file (5926, 5958, 6008).

**No prompt-cache mutation, no unsafe replay, no transcript/usage corruption.** `should_rewrite`
takes only shared references and returns a `bool`; it mutates nothing. The sole side effect is
rewriting the local `finish_reason` (`outcome.finish_reason` / `value["choices"][0]["finish_reason"]`).
`request_messages` is read-only at both sites. Usage capture is independent of the rewrite. The
persisted interim fragment carries `finish_reason == "length"`, which the integration tests assert
against the durable transcript (`durable[3]`/`durable[5]`).

## 2. Do the tests prove production behavior?

Yes, more than the usual bar:

- `classifier_matches_all_source_executed_python_goldens` drives `should_rewrite` over all 113
  source-executed cases (43 positive, 70 negative) and asserts the exact per-case Python result,
  including the content-type, tool_calls, think-strip, floor, punctuation, CJK, and emoji edges.
- `local_ollama_glm_stop_after_tool_history_continues_and_persists` (8415) runs the full buffered
  `run_turn`: tool call, truncated unpunctuated `stop`, then continuation. It asserts three upstream
  calls, the `max_tokens` bump, the interim fragment and continuation nudge in the outgoing body, and
  the durable transcript with `finish_reason == "length"` and the stitched final answer. This proves
  the buffered call site end to end, not just the predicate.
- `ollama_glm_stop_correction_stays_inside_its_public_route_boundary` (8564) is a genuine negative
  suite through `run_turn`: a `:cloud` model (cloud exclusion), a naturally punctuated answer (gate 9),
  and a non-GLM/non-zai model (gate 3.1) each complete in two calls with no continuation.
- `restored_tool_history_enables_ollama_glm_stream_correction` (8650) exercises the streaming
  (SSE) call site with tool history restored from SQLite, proving gate 4 sees the restored
  `role=="tool"` message and the stream rewrite drives continuation and durable persistence.

These are not predicate-only tests; they observe emitted chunks, outgoing request bodies, and the
persisted lifecycle transcript.

## 3. Residual risks (not defects in this checkpoint)

1. **Streaming synthesizes the assistant message without `tool_calls`** (`native_agent.rs:4711-4714`).
   `forward_sse` never parses `tool_calls` (confirmed: no tool_call handling in 5596-5690), so the
   streaming path only ever carries text. If some provider ever streamed visible content plus
   tool_calls while reporting `finish_reason="stop"`, Python's gate 5 would block on
   `assistant_message.tool_calls`, whereas the Rust synthetic message has no such field. This is
   unreachable today because the streaming path structurally ignores tool_calls; it becomes relevant
   only if streaming ever learns to carry them. Low, theoretical.

2. **No integration test for within-turn tool -> streaming-stop.** Test 1 and 2 cover the buffered
   path; test 3 covers streaming but with tool history restored from the DB rather than produced
   earlier in the same turn. The within-turn streaming variant relies on the same `request_messages`
   accumulation and is covered transitively, but a dedicated test would close the gap. Test coverage,
   not correctness.

3. **`api_mode` is hardcoded to `"chat_completions"`** at both call sites. Correct for this native
   chat-completions gateway, but it means the gate-2 guard is a constant rather than a read of the
   live wire mode. If the native loop ever speaks another wire format, the guard would not protect it.
   Informational.

4. **`to_lowercase()` vs `.lower()` on non-ASCII model/provider strings** could differ under full
   Unicode case folding (for example a dotted/dotless I), but every token the checks look for is
   ASCII, so no realistic model name changes the outcome. Negligible.

5. **Whitespace parity depends on the shared `python_whitespace` approximation.** I verified it
   matches Python `re.\s` for the cases that matter here (ASCII whitespace, NBSP, NEL, U+001C-U+001F),
   and it is the same helper used throughout the visible-text port, but it is an approximation of the
   full Python whitespace class rather than a table copy. Low.

## Notes

Review only. No Rust, Python, JSON, PORT.md, INDEX.md, or existing analysis files were edited. This
document is the only file written.
