# Main-provider response-recovery seam

Date: 2026-09-11

Author lane: independent forward-mapping for the next ordinary main-provider
response-recovery checkpoint, after request liveness and stream liveness.

## Scope

This document maps the remaining `finish_reason="length"` response-recovery
cases that the visible-text and buffered-tool continuation checkpoints
deliberately left open. The five cases in scope, from `rust/PORT.md`:

1. Thinking-only length continuation, plus its one-shot reasoning disable.
2. Repetition-dominated rejection.
3. Dropped-stream stubs (the partial-stream-stub network variant).
4. Dropped tool names (the smaller-chunks continuation prompt variant).
5. The Ollama GLM `stop`-to-`length` correction.

Out of scope and not re-analyzed here: request timeout, stream inactivity
deadline, stale-deadline machinery, non-chat transports, and OAuth routes.
Those keep their own lanes. The client-rebuild and stall-config lanes already
own the timeout question.

What is already ported (do not touch): the explicit-`length` visible-text
stream continuation, the buffered tool-path continuation, truncated tool-call
same-request retries, progressive output caps, the four-attempt ceiling, and the
durable fragment/nudge persistence. See
[native-main-provider-length-continuation-resolution.md](native-main-provider-length-continuation-resolution.md)
and
[native-main-provider-buffered-continuation-resolution.md](native-main-provider-buffered-continuation-resolution.md).

## Live file and line references

Python (`agent/conversation_loop.py` unless noted):

- Continuation prompt constants and builder: `_LENGTH_CONTINUATION_NETWORK_STUB`
  `conversation_loop.py:1312`, `_LENGTH_CONTINUATION_OUTPUT_LIMIT`
  `conversation_loop.py:1318`, `_LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX`
  `conversation_loop.py:1326`, `_get_continuation_prompt`
  `conversation_loop.py:1329-1348`.
- GLM `stop` rewrite call site: `conversation_loop.py:4035-4044`
  (`_should_treat_stop_as_truncated` then `finish_reason = "length"`).
- GLM gate helpers: `_is_ollama_glm_backend` `run_agent.py:1875-1909`,
  `_should_treat_stop_as_truncated` `run_agent.py:1911-1940`,
  `_has_natural_response_ending` `run_agent.py:1855-1873`.
- Length block entry: `conversation_loop.py:4149`.
- Truncated-response normalize: `conversation_loop.py:4170-4181`.
- Thinking-exhausted detection and abort: `conversation_loop.py:4195-4242`.
- Repetition-dominated detection and abort: `conversation_loop.py:4255-4296`.
- Content-filter-terminated stream fallback (adjacent, tied to the stub tag):
  `conversation_loop.py:4312-4354`.
- Interim append, empty-partial-stub skip, one-shot reasoning-off:
  `conversation_loop.py:4355-4396`
  (empty-stub detection `4377-4380`, reasoning-off `4381-4390`, append
  `4391-4396`).
- Prompt selection (dropped-tools / network-stub / output-limit) and continuation
  break: `conversation_loop.py:4398-4437` (`_dropped_tools` read `4402-4404`).
- Ceiling exit: `conversation_loop.py:4438-4510`.
- Partial-stream-stub construction: `_build_partial_stream_stub`
  `chat_completion_helpers.py:3608-3637`; the streaming stub carrying dropped
  tool names and the `_content_filter_terminated` stamp
  `chat_completion_helpers.py:5771-5869`.
- `PARTIAL_STREAM_STUB_ID`: `hermes_constants.py:1761`.
- Repetition guard: `is_repetition_dominated` `agent/repetition_guard.py:42-81`,
  thresholds `agent/repetition_guard.py:26-40`.

Rust (`rust/crates/hermes-gateway/src/native_agent.rs` unless noted):

- No-tools stream length block: `native_agent.rs:4366-4394`, gated by
  `if outcome.visible` at `native_agent.rs:4359`; retry counter
  `length_continue_retries` `native_agent.rs:4320`; cap growth
  `apply_main_length_continuation_cap` `native_agent.rs:407-430` applied at
  `native_agent.rs:4333`.
- Buffered tool-path length block: `native_agent.rs:5434-5504`, `Step::Final`
  arm `native_agent.rs:5455-5501`, truncated tool-call arm
  `native_agent.rs:5438-5453`.
- Non-visible fall-through: no-tools continues past the length block at
  `native_agent.rs:4424`; the empty-stream retry path is
  `native_agent.rs:4467-4490`. Tool path: `continuation_ready`
  `native_agent.rs:5506`, `empty` classification `native_agent.rs:5534`.
- Sole continuation prompt: `MAIN_LENGTH_CONTINUATION_PROMPT`
  `native_agent.rs:1003`.
- Reasoning primitives already present: `main_message_has_reasoning`
  `native_agent.rs:1730`, `main_inline_reasoning_text` `native_agent.rs:1759`,
  think-tag match `native_agent.rs:1740`.
- Visible-text primitives: `visible_response::answer`
  `visible_response.rs:65`, `visible_response::strip` `visible_response.rs:58`.
- Continuation persistence: `persist_continuation_messages`
  `native_agent.rs:156`; `turn_has_continuation` `native_agent.rs:1901` with
  `mark_turn_continuation` `native_agent.rs:2905` and `clear_turn_continuation`
  `native_agent.rs:2910`.

Absent in Rust (confirmed by crate-wide search): any partial-stream-stub
sentinel, any thinking-exhausted length error, any ephemeral reasoning-off flag,
any repetition guard, any dropped-tool-names nudge, any Ollama or GLM gate, and
two of the three continuation prompt variants. Only the output-limit prompt
exists.

## Behavior and ownership table

| Case | Python runtime owner and order | Budget | Visible-replay barrier | Durable transcript | Usage | Fallback | Provider or model gate | Rust today |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Thinking-only length continuation | Length block, after both guards, at the interim-append step (`4381-4390`). Empty non-stub content sets `_ephemeral_reasoning_off = True`, appends only the nudge, continues. | Shares the four-attempt `length_continue_retries` budget. | No visible fragment appended; only the user nudge. | Nudge row only; no assistant fragment for the empty attempt. | Not captured at the aborting/looping sites; main accounting at `4612` is never reached inside the block. | None; stays on frozen route with reasoning off for one call. | None; content-only detection. | Skipped. The `if outcome.visible` gate (`4359`) and `visible_response::answer` gate (`5507`) drop an empty-visible `length` response into empty-response handling (`4424`, `4467`, `5534`) instead of continuing. No reasoning-off. |
| Repetition rejection | Length block, after thinking-exhausted, before interim append (`4255-4296`). Aborts the whole turn. | Terminal; no retry consumed. | Partial content discarded, not stitched. | Prior clean turns only; the degenerate fragment is dropped. Any fragments appended on earlier passes remain unless separately stripped. | Not captured. | None. | None; `is_repetition_dominated` over the think-stripped visible text. | Absent. No guard; a repetition-dominated `length` fragment is continued and stitched like any other. |
| Dropped-stream stub (network) | Transport builds `PARTIAL_STREAM_STUB_ID` + `finish_reason=length` after deltas were sent (`chat_completion_helpers.py:5771-5869`, `3608-3637`); loop reads the stub id and, when empty, skips the interim append and sends the network-stub prompt (`4377-4380`, `4414-4419`, `4426`). | Same four-attempt length budget. | Empty stub never appended; only the nudge. | Nudge row only for the empty stub. | Stub carries no usage; nothing captured. | If `_content_filter_terminated`, escalate to fallback before retry (`4312-4354`). | None for the plain stub; the content-filter branch is provider-classified. | Absent. Rust has no stub concept. A drop after visible output fails immediately; a drop before any output is `EmptyStreamError` retried three times then fallback (`4467-4490`). Neither is Python's stub continuation. |
| Dropped tool names | Same stub path with `_dropped_tool_names` populated (`chat_completion_helpers.py:5784-5862`); loop selects the smaller-chunks prompt with up to three names (`4402-4413`, `_get_continuation_prompt` `1330-1344`). | Same four-attempt length budget. | Empty stub not appended; only the nudge. | Nudge row only. | None. | None. | None; names come from the dropped tool-call deltas. | Absent. No dropped-tool nudge variant. |
| Ollama GLM stop-to-length | Pre-block gate at normalize (`4035-4044`), before the content-filter and length branches. Rewrites `stop` to `length` so the response enters the length block. | Feeds the existing length budget once rewritten. | None added; only reclassifies `finish_reason`. | None directly; downstream behavior follows the length block. | None directly. | None directly. | `_is_ollama_glm_backend`: GLM model or `zai` provider, Ollama base URL or `:11434`, excluding `ollama.com` and `:cloud`; requires a `tool` message in history, a non-tool-call assistant, visible text of at least 20 chars with whitespace, and no natural ending. | Absent. Rust acts only on a literal provider-supplied `length` (`4366`, `5435`). No stop rewrite, no Ollama or GLM gate. |

## Ordering inside the length block

Python runs a fixed sequence that the Rust port must preserve, because later
steps assume earlier ones already fired:

1. Normalize `stop` to `length` for the Ollama GLM case (before the block).
2. Enter the length block on `finish_reason == "length"`.
3. Thinking-exhausted guard, abort if reasoning-only with think tags.
4. Repetition-dominated guard, abort if the think-stripped fragment is a
   degenerate repeat.
5. Content-filter-terminated escalation to fallback, when the stub is stamped.
6. Interim append: skip empty stubs, set one-shot reasoning-off for empty
   non-stub content, otherwise append the fragment.
7. Prompt selection: dropped-tools, then network-stub, then output-limit.
8. Ceiling exit after the fourth attempt.

The guards at steps 3 and 4 run for both the plain response and the stub, so a
stub whose recovered text is repetition-dominated still aborts rather than
continues.

## Which cases form one checkpoint

Recommended split into three lanes:

Lane A, one coherent checkpoint: thinking-only handling and repetition
rejection. Both are pure content guards that sit at the same insertion point at
the top of the length block, both operate on think-stripped visible text, both
abort or divert with an actionable message, and both are provider-neutral and
transport-neutral. The one-shot reasoning-off belongs here too, because it is
the sibling branch of the same reasoning-only detection: thinking-exhausted
(think tags, no visible text after them) aborts, while reasoning-only-without-
tags continues once with reasoning disabled. Grouping them keeps the single
"what to do with a truncated response before we nudge" decision in one place and
lets one deep module own it. This lane changes current Rust behavior: today an
empty-visible `length` response is treated as an empty response, so Lane A must
move that classification into the length block.

Lane B, separate: dropped-stream stubs and dropped tool names. These share a
prerequisite that neither Lane A nor the shipped continuation has, a
transport-layer partial-stream-stub. Rust must first synthesize a stub outcome
when the SSE stream drops after deltas (carrying recovered text, an optional
dropped-tool-name list, and a network-drop marker), then teach the loop to
select the network-stub and dropped-tools prompt variants and to skip appending
an empty stub. That is a transport change plus two prompt constants plus prompt
selection. The `_content_filter_terminated` fallback escalation rides on the
same stub tag, so it is naturally adjacent, though it is a distinct behavior
that can be deferred within the lane.

Lane C, separate: the Ollama GLM stop-to-length correction. It is a narrow
pre-block gate with its own detection surface, base-URL and model parsing, the
`ollama.com` and `:cloud` exclusions, a required tool message in history, and
the natural-ending heuristic. It is false-positive sensitive and deserves an
isolated corpus so a bad gate cannot manufacture truncations. It only fires on
the tool-loop path. Keep it out of Lane A so the content guards do not inherit
provider gating.

Do not fold all five into one checkpoint. The content guards are safe and
loop-local; the stub work reaches into the transport; the GLM gate needs
provider identity. One checkpoint would mix three risk profiles.

## Cache and alternation implications

- Guards that abort (thinking-exhausted, repetition) add nothing to the prompt,
  so they have no prompt-cache effect. They return an error turn with no new
  fragment or nudge rows, so the durable prefix and any prefix cache stay intact.
- The one-shot reasoning-off continuation appends only a deterministic user
  nudge, so the cache prefix stays stable across the turn. But because no
  assistant fragment is appended for the empty attempt, the on-wire sequence can
  become `... user (original) -> user (nudge)`, two consecutive user messages.
  Python tolerates this through its pre-call user-merge and empty-heal passes.
  The Rust port must confirm its own projection either merges consecutive user
  turns or that strict providers on this route accept the pair. This is the main
  alternation risk in Lane A.
- The stub prompts (network-stub, dropped-tools) are new deterministic suffixes.
  The network-stub constant is fully static and cache-stable. The dropped-tools
  prompt interpolates up to three tool names, so its suffix varies with the tool
  set; the variation is bounded and only appears on the drop path, so the cache
  impact is minor but nonzero.
- The GLM rewrite changes only the in-memory `finish_reason`. It never alters
  request bytes, so it has no cache or alternation effect by itself. Its only
  downstream effect is to route the same body into the existing length
  continuation, whose alternation is already handled by the shipped lane.
- Route freezing still holds across all cases. None of these paths may change the
  system prompt, tool schema, or sticky route. Continuation stays semantic, not
  prompt regeneration.

## Minimal deep-module interface recommendation

Keep the two turn loops shallow by hiding the truncation decision behind one
narrow classifier, and keep the GLM gate as a separate pre-classifier so
provider identity never leaks into the content decision.

Lane A and B classifier, one deep module over the normalized truncated response:

```text
enum TruncationDisposition {
    AbortThinkingExhausted,          // reasoning-only with think tags
    AbortRepetition,                 // think-stripped text is repetition-dominated
    Continue {
        continuation_prompt: &'static str or owned String, // output-limit | network-stub | dropped-tools(names)
        append_interim: bool,        // false for an empty stub
        disable_reasoning_once: bool,// true for empty non-stub reasoning-only
    },
    CeilingExit,                     // fourth attempt reached
}

fn classify_length_truncation(
    visible_content: Option<&str>,
    reasoning_present: bool,          // main_message_has_reasoning
    has_think_tags: bool,             // native_agent.rs:1740 primitive
    has_tool_calls: bool,
    is_partial_stream_stub: bool,
    dropped_tool_names: &[String],
    attempt: usize,                   // length_continue_retries
) -> TruncationDisposition
```

The classifier is pure and fully unit-testable against the Python corpus. The
loop keeps only the mechanical work: append or skip, push the nudge, set the
one-shot reasoning-off, or return the abort or ceiling outcome. The existing
`visible_response::answer`, `main_message_has_reasoning`, and think-tag helpers
already supply the inputs, so no new stripping logic is needed for Lane A. A new
`repetition_guard` module ports `is_repetition_dominated` with the same
thresholds (`MIN_FRAGMENT_LENGTH 400`, `_REPEAT_WINDOW 60`, `_MIN_REPEAT_COUNT
5`, `_DOMINANCE_RATIO 0.5`) and the same line-fast-path then window-scan shape.

Lane C gate, a separate pre-classifier the loop calls before the length block:

```text
fn should_treat_stop_as_truncated(
    finish_reason: &str,
    api_mode: ApiMode,
    provider: &str,
    model: &str,
    base_url: &str,
    messages: &[Value],
    assistant_message: &Value,
) -> bool
```

It returns whether to rewrite `stop` to `length`. It owns the Ollama and GLM
detection, the `ollama.com` and `:cloud` exclusions, the tool-in-history
requirement, and the natural-ending heuristic. Keeping it separate means Lane A
can ship without any provider gating and Lane C can ship without touching the
content guards.

Lane B also needs a transport seam: the SSE forwarder must emit a partial-stream
outcome when the stream drops after visible deltas, carrying recovered text, the
dropped-tool-name list, and a network-drop marker, so `is_partial_stream_stub`
and `dropped_tool_names` reach the classifier. That marker is the single new
transport-visible field.

## Recommended public-seam tests

Lane A:

- Reasoning-only `length` with think tags and no visible text returns the
  thinking-exhausted actionable message as a failed, delivery-only turn, persists
  no fragment, and does not capture usage.
- Empty non-stub `length` with reasoning present but no think tags appends only
  the nudge, disables reasoning for exactly the next request, and continues on
  the frozen route.
- A repetition-dominated visible fragment aborts with the repetition message,
  discards the partial, and leaves the durable transcript at the last clean turn.
- A short or mixed fragment that is not repetition-dominated still continues,
  proving the guard is conservative.
- The one-shot reasoning-off does not leak into the next turn when the fourth
  truncation reaches the ceiling without another continuation call.

Lane B:

- A stream that drops after visible deltas produces a continuation using the
  network-stub prompt, not the output-limit prompt, and stays on the frozen
  route.
- An empty stub appends only the nudge and never writes an empty assistant row.
- A stream that drops mid tool-call surfaces the dropped-tools prompt with at
  most three names and never executes the incomplete tool call.
- A content-filter-terminated stub escalates to the configured fallback before
  spending continuation attempts, and gives up with the same message when no
  fallback exists.

Lane C:

- A GLM-on-Ollama response with `finish_reason=stop`, a tool message in history,
  a non-tool-call assistant, and visible text without a natural ending is
  rewritten to `length` and enters continuation.
- The same response on `ollama.com` or a `:cloud` model is left as `stop`.
- A non-GLM local endpoint, a response with a natural ending, or a response with
  no tool message in history is left as `stop`, guarding against false positives.

All tests exercise the public turn entry the shipped continuation tests already
use (`native_agent.rs:5750-6877`), driving a mock provider and asserting on
delivered text, request-body history, durable rows, and usage.

## Explicit deferrals

- Request timeout, stream inactivity deadline, and stale-deadline handling stay
  in the client-rebuild and stall-config lanes. Not analyzed here.
- The Codex and Responses incomplete-turn continuation (`_CODEX_INCOMPLETE_NUDGE`,
  `_CODEX_ACK_CONTINUATION_NUDGE`) and Anthropic and Bedrock length paths are
  separate transport lanes and are not part of these three checkpoints.
- The empty-after-tool nudge (`_EMPTY_TOOL_RESPONSE_NUDGE`) and the dropped
  tool-call recovery nudge (`_DROPPED_TOOLCALL_NUDGE_CONTENT`) are their own
  successful-body recovery cases, already tracked elsewhere, and are not
  reopened here.
- The content-filter-terminated fallback escalation is documented as adjacent to
  Lane B because it rides the same stub tag, but it can be deferred within Lane B
  if the stub is landed first without it.
- Usage accounting for the aborting cases follows the existing convention that
  rejected and truncated responses do not enter native usage accounting. The one
  open decision is whether an empty reasoning-only continuation attempt should
  capture usage, since the model still consumed its whole output budget on
  reasoning; resolve that against the shipped continuation accumulator rather
  than in this seam.
</content>
</invoke>
