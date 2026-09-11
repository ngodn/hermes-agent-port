# Review: Native Main-Provider Length-Truncation Content Guards

**Scope**: the uncommitted Rust checkpoint that ports Python's three ordinary
chat-completions `finish_reason == "length"` content guards (thinking-exhausted
detection, repetition-dominated rejection, empty reasoning-only one-shot
reasoning disable), plus the reasoning-mandatory rejection recovery and the
SQLite continuation durability that backs them.

**Files reviewed**
- `rust/crates/hermes-gateway/src/main_provider_truncation.rs` (new)
- `rust/crates/hermes-gateway/src/native_agent.rs` (streaming and buffered integration)
- `rust/crates/hermes-gateway/src/session_db.rs` (continuation persistence)
- `rust/crates/hermes-gateway/src/chat_message_projection.rs` (api_content projection)
- Supporting helpers: `visible_response.rs`, `python_value.rs`

**Python cross-checked**
- `agent/repetition_guard.py`
- `agent/conversation_loop.py` lines 2209-2215, 4195-4265, 4356-4495
- The AGY contract `rust/analysis/main-provider-truncation-guards-agy.md`

This is a correctness and security review. No production code, tests, goldens,
PORT.md, INDEX.md, or the AGY report were changed. I did not build or run the
Rust suite; findings are from source inspection and Python comparison.

---

## Summary

The core classifier (`main_provider_truncation::classify`) and the repetition
algorithm are a faithful port. Tool-call preemption, the one-shot reasoning
lifecycle, the reasoning-mandatory cache-key preservation, and the SQLite
sidecar merge are all correct and well covered by the new tests.

One confirmed behavioral divergence causes user-visible content loss in a mixed
continuation sequence (Finding 1). A second, narrower divergence routes a
stalled-but-partially-visible stream through the repetition guard, which the
Python scope explicitly excludes for dropped streams (Finding 2). The remaining
items are optional hardening or intentional port decisions worth a reviewer
sign-off.

---

## Confirmed findings

### Finding 1 (Medium, correctness / data loss): buffered ceiling reached via the empty-reasoning path discards already-accumulated visible continuation parts

**Where**: `native_agent.rs:5893-5932` (Block A, empty-reasoning ceiling at
`5923-5931`) versus `native_agent.rs:5956-6000` (Block B, visible ceiling at
`5994-6000`).

The buffered length handling is split into two sequential blocks that share one
`length_continue_retries` counter:

- Block A (`5893`) handles the content guards and the empty reasoning-only
  continuation. Its ceiling (`length_continue_retries >= 4`) returns
  `Step::PartialFinal { text: NO_VISIBLE_RESPONSE, ... }` and never looks at
  `continuation_parts`.
- Block B (`5935`) handles visible text continuation. It pushes visible text to
  `continuation_parts` (`5958`) and, at its own ceiling, returns
  `main_join_length_parts(&continuation_parts)` (`5996`), preserving the partial.

Python has a single unified block (`conversation_loop.py:4356-4495`). At the
ceiling it computes `partial_response = strip(join(truncated_response_parts))`
(`:4439`) and, if that is non-empty, returns it as `_ceiling_final` (`:4446-4453`);
only when every fragment was empty does it emit the "No visible answer" message
(`:4454-4476`). The decision keys on whether any visible text accumulated, not on
the shape of the fourth attempt.

**Failure scenario**: a reasoning model truncates with visible text on attempts
1 through 3 (each goes through Block B, pushing "chapter one", "chapter two",
"chapter three" to `continuation_parts` and advancing the shared counter to 3),
then on attempt 4 returns `finish_reason == "length"` with empty content and
reasoning delivered in a separate field. Attempt 4 enters Block A, increments
the counter to 4, fails the `< 4` test, and returns `NO_VISIBLE_RESPONSE`. The
three accumulated chapters are dropped. Python would return the stitched
"chapter one ... chapter three".

Because the buffered path never streamed the interim text to the user, this is a
genuine loss of content the user would otherwise have received. It is not caught
by the new tests, which only exercise the all-empty ceiling
(`reasoning_only_stream_ceiling_is_bounded_and_does_not_leak_next_turn`) and the
pure two-attempt recovery.

**Suggested direction** (not applied): at the Block A ceiling, fall back to
`main_join_length_parts(&continuation_parts)` when it is non-empty, mirroring
Python's `if partial_response:` branch, and only emit `NO_VISIBLE_RESPONSE` when
no visible fragment was ever captured.

The streaming path has the analogous split (`native_agent.rs:4723-4747` empty
ceiling versus `4756-4783` visible ceiling), but there the interim visible text
was already streamed to the user as it arrived, so the practical loss is lower;
the divergence in the final assembled/durable reply still exists.

### Finding 2 (Low to Medium, divergence): a post-visible stream stall synthesizes `finish_reason == "length"` and now flows through the repetition guard, which Python excludes for dropped streams

**Where**: `native_agent.rs:5549-5550` synthesizes `outcome.finish_reason =
"length"` when a stream stalls after producing visible content;
`native_agent.rs:4710-4712` then calls `classify(Some(&outcome.raw_content),
false)` on that synthesized length.

The stall-with-no-visible case is handled earlier at `4652` (`if outcome.stalled
&& !outcome.visible`), so a stall that already produced visible text falls
through to the new Block A at `4710`.

The AGY scope (contract section 1, "Explicit Out-of-Scope Exclusions") states
that dropped streaming connections and partial stream stubs are not subject to
these content guards. In Python the repetition guard runs on a real upstream
`length` response, not on a locally synthesized stall. In Rust a stalled stream
whose partial visible text happens to be repetition-dominated (at least 400
characters with a 60-character window covering half the text) is now aborted
with `REPETITION_RESPONSE` instead of taking the dropped-stream length
continuation.

Thinking-exhausted and empty-reasoning cannot mis-fire here: both require empty
visible content, and the synthesized length only occurs when `outcome.visible ==
true`, so `visible_response::answer` returns `Some` and the disable-reasoning
flag stays false. Only the repetition branch is reachable.

**Assessment**: aborting a stalled degenerate loop is arguably reasonable, but it
diverges from the documented Python semantics for dropped streams. Worth a
reviewer decision: either gate Block A on a real provider-reported length (skip
when `outcome.stalled`) or document the intentional stricter behavior. No test
covers a repetition-dominated post-visible stall.

---

## Optional hardening (not bugs against the current tests)

### H1: reasoning-mandatory detection only inspects the top-level `reasoning` key

`native_agent.rs:2736-2740` triggers recovery only when
`body.get("reasoning").is_some_and(reasoning_is_disabled)`. The recovery itself
removes both `reasoning` and `reasoning_effort` (`2748-2751`), acknowledging both
can be present, and `apply_provider_extras_with_reasoning` can emit a disable via
`reasoning_effort: "none"` for some wire shapes. A provider that rejects a
disable expressed only as `reasoning_effort` (no top-level `reasoning` object)
would not be detected, so the 400 would propagate as a hard failure instead of
the one-shot cache-key-preserving retry. The vercel-based tests only cover the
`reasoning` object shape. Consider also checking `reasoning_effort` in the
detection predicate.

### H2: the api_content sidecar merge produces a different durable transcript shape than Python

Python appends the continuation nudge as a separate `user` message
(`conversation_loop.py:4429-4434`), yielding two consecutive user rows after the
suppressed empty assistant. The Rust port merges the nudge into the current
user message's `api_content` sidecar (`main_provider_truncation.rs:88-105` and
`session_db.rs:4051-4084`), yielding a single user row whose display `content`
stays clean and whose model-facing content becomes `question + "\n\n" + nudge`.
This is a deliberate, test-locked design (it also sidesteps the empty-assistant
HTTP 400 and consecutive-user concerns), and it correctly preserves structured
multimodal content by pushing a `{"type":"text"}` part rather than stringifying
(`session_db.rs:4058-4067`, `chat_message_projection.rs:159-167`). Flagging only
so the divergence from Python's message layout is an explicit decision rather
than an accident. Prompt-cache behavior is effectively equivalent: everything
before the mutated user message stays byte-stable, and token-prefix caching
still covers the original question tokens.

### H3: non-string buffered content is treated as reasoning-only rather than skipped

In the buffered path, `classify(message["content"].as_str(), ...)`
(`native_agent.rs:5896-5901`) passes `None` when `content` is a JSON array or
other non-string. With no tool calls, `classify` then returns
`Continue { disable_reasoning_once: true }` (`main_provider_truncation.rs:64`),
arming a reasoning-off continuation. Python's `is_repetition_dominated` guards
with `isinstance(text, str)` and its thinking regex would raise on a list, so the
shapes differ. Ordinary chat-completions responses use string or null content,
so this is an edge case, but a defensive note is warranted.

### H4: `text.to_lowercase()` allocates the entire error body on every non-2xx

`native_agent.rs:2737` lowercases the full response body to substring-match
"reasoning is mandatory". For large error payloads this is a minor allocation on
an already-error path. A case-insensitive `find` or an ASCII-only check would
avoid the copy. Purely cosmetic.

---

## Verified correct (Python parity holds)

- **Repetition algorithm** (`main_provider_truncation.rs:107-152`): the
  `MIN_FRAGMENT_LENGTH = 400`, `REPEAT_WINDOW = 60`, `MIN_REPEAT_COUNT = 5`, and
  `DOMINANCE_RATIO = 0.5` constants match `repetition_guard.py:28-40`. The line
  dominance test `count * len * 2 >= n` is algebraically identical to Python's
  `count * len(line) >= n * 0.5` for both even and odd `n`. The window `needed =
  max(5, n.div_ceil(120))` equals `max(_MIN_REPEAT_COUNT, ceil(n * 0.5 / 60))`.
  Length is measured in Unicode scalar values on both sides (`chars().count()`
  versus Python `len`), and windows are sliced by scalar (`chars[start..]` versus
  `text[i:i+window]`), so no byte-versus-codepoint mismatch.
- **Unicode line and whitespace parity**: the split predicate
  (`main_provider_truncation.rs:115-128`) matches Python `str.splitlines()`
  boundaries exactly (it includes `\x1c \x1d \x1e` and `   ` but not
  `\x1f`), and `python_whitespace` (`python_value.rs:286-288`) reproduces
  CPython `str.strip` by adding the four information separators `\x1c-\x1f` on top
  of Unicode White_Space. `\r\n` splits into an empty segment that is filtered by
  the `is_empty` check, matching Python's single-break behavior for counting.
- **Thinking-tag regex** (`main_provider_truncation.rs:11-14`): case-insensitive
  match of `think|thinking|reasoning|REASONING_SCRATCHPAD` matches
  `conversation_loop.py:4196`, and correctly does not match `<thought>` (AGY 8.3).
  `answer(content).is_none()` reproduces `not _has_content_after_think_block`
  (strip then trim then non-empty).
- **Tool-call preemption** (buffered): `classify` receives the real
  `has_tool_calls` (`native_agent.rs:5898-5900`), and every guard branch is
  gated on `!has_tool_calls`, so a truncated response carrying tool calls falls
  through to the tool-truncation lane at `5935-5955`, matching Python.
- **Streaming has_tool_calls = false is safe**: `MainStreamOutcome`
  (`native_agent.rs:5444-5455`) has no tool-call field; the streaming run_turn
  path never parses tool calls (they are handled in the buffered `ChatModel::step`
  path). Passing `false` is consistent with how that path treats all responses.
- **One-shot reasoning lifecycle and next-turn isolation**: the disable flag is a
  turn-local `disable_reasoning_once` consumed per request via `std::mem::take`
  (`native_agent.rs:4577`, `4585-4589`, `5701`, `5712-5716`), so it cannot leak
  into a later turn. This is structurally safer than Python's shared
  `agent._ephemeral_reasoning_off` and its explicit reset at
  `conversation_loop.py:2215`. The persistent `reasoning_disable_rejected`
  (`Arc<AtomicBool>`, `native_agent.rs:1895-1897`) is intentionally shared across
  route clones and turns, matching Python's `_reasoning_disable_rejected`, and is
  covered by `reasoning_only_stream_ceiling_is_bounded_and_does_not_leak_next_turn`.
- **Reasoning-mandatory recovery preserves the cache key**: on a 400 the route
  swaps the atomic once (`native_agent.rs:2741-2745`), strips reasoning fields,
  and re-applies extras; subsequent requests resend the user's own configuration
  untouched, or omit reasoning entirely if the user's own config was a disable
  (`native_agent.rs:2985-3016`, `3050-3055`). The `swap(true)` guard bounds the
  recovery to exactly one attempt. Matches AGY 6.3 steps D and B, verified by
  `reasoning_mandatory_rejection_retries_with_original_cache_key` and
  `rejected_configured_reasoning_disable_is_omitted_on_later_requests`.
- **SQLite continuation atomicity and lease**: `append_native_continuation_messages`
  validates pair structure, rejects a leading nudge with empty content, allows
  only the first message to carry the reasoning-only flag, checks the turn phase
  under the transaction, and performs the sidecar merge or the post-tool user
  insert inside `tx` (`session_db.rs:4051-4113`). A stale lease holder returns
  `false` with no write, confirmed by
  `reasoning_only_nudge_sidecar_merge_is_atomic_and_structured`. The completed
  tool-tail case appends a fresh user row and bumps `message_count` by one
  (`session_db.rs:4075-4081`, `4139-4147`), confirmed by
  `reasoning_only_nudge_after_completed_tool_group_starts_user_continuation`.
- **Delivery-only diagnostics**: all three terminal responses call
  `mark_turn_reply_delivery_only` before returning (`native_agent.rs:4714`,
  `4739`, `5903`, `5924`), and the tests assert
  `!assistant_reply_is_durable(...)`, so guard messages are shown but not
  persisted.
- **Usage accounting**: the abort guards and the intermediate empty-reasoning
  continuations do not call `capture_usage`; only completed visible steps do
  (`native_agent.rs:4757`, `4785`, `5960`, `6012`, `6040`). This matches the AGY
  matrix (section 7): usage captured only on completed continuation. The reasoning
  tokens burned on empty attempts go unaccounted in both Python and Rust, so this
  is preserved parity rather than a new gap.

---

## Test-gap notes

1. **No mixed visible-then-empty ceiling test** (Finding 1). Add a buffered case:
   three visible length truncations followed by an empty reasoning-only length on
   the fourth, asserting the final text is the stitched visible prefix, not
   `NO_VISIBLE_RESPONSE`. This test would fail today and pin the divergence.
2. **No repetition-dominated post-visible stall test** (Finding 2). A streaming
   case that emits at least 400 characters of a repeating 60-character pattern and
   then stalls would document whether the repetition abort or the dropped-stream
   continuation is intended.
3. **No `reasoning_effort`-only mandatory rejection test** (H1). Current coverage
   only exercises the top-level `reasoning` object disable shape.
4. **No non-string buffered content test** (H3). A response with an array
   `content` and no tool calls would document the reasoning-only fallback.
5. **Repetition guard edge coverage**: the golden set covers line-path,
   window-path, dominance-under-50-percent, and below-400 fail-open. A window-path
   case whose repeat count lands exactly on the float-versus-integer `needed`
   boundary (for example `n` a multiple of 120) would harden the `div_ceil`
   equivalence claim, though inspection shows the two formulas agree.

## Suspected issues that did NOT hold

- **"Streaming path ignores tool-call preemption."** Not a bug: the streaming
  outcome cannot represent tool calls (`MainStreamOutcome` has no such field), and
  tool calls are handled in the buffered path. Passing `has_tool_calls = false`
  is correct for that path.
- **"Merging the nudge into the user message breaks prompt caching."** Does not
  hold: prefix caching still covers the system prompt, prior history, and the
  original question tokens; only the appended nudge tokens are new, the same net
  effect as Python's separate user message.
- **"Thinking-exhausted or empty-reasoning can mis-fire on a post-visible
  stall."** Does not hold: both require empty visible content, which contradicts
  the `outcome.visible == true` precondition for the synthesized length. Only the
  repetition branch is reachable (Finding 2).
