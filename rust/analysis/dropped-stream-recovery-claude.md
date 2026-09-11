# Dropped chat-completions stream recovery: Rust-port seam map

Scope: Python's recovery for a chat-completions SSE stream that ends or breaks
*without* a provider-reported `finish_reason`, and how it is surfaced to the
conversation loop through `PARTIAL_STREAM_STUB_ID`. This is the "the connection
died, but we already got some output" lane. It is deliberately kept distinct
from three neighbours:

- provider stalls (inactivity-timeout aborts), which Rust already handles;
- malformed or empty SSE (zero usable chunks), which raises instead of stubbing;
- provider-reported length (`finish_reason="length"` actually sent), which is a
  genuine output cap, not a drop.

Out of scope per the task: Ollama/GLM stop-to-length correction, thinking-budget
exhaustion, and the repetition/degenerate-loop guards. Those are referenced only
where a decision branch has to route away from them.

All line anchors verified against the working tree on 2026-09-11 (branch
`rust-rewrite`).

---

## 1. The constant and the stub shape

`PARTIAL_STREAM_STUB_ID = "partial-stream-stub"` is defined at
`hermes_constants.py:1761`. A stub is a `SimpleNamespace` masquerading as an
OpenAI chat-completions response. It always carries `finish_reason="length"` so
the conversation loop's existing continuation machinery fires, but it is tagged
with the stub id so that same machinery can tell a real output cap apart from a
network drop.

Canonical builder: `_build_partial_stream_stub` at
`agent/chat_completion_helpers.py:3608-3637`. Fields:

- `id = PARTIAL_STREAM_STUB_ID`
- `choices[0].finish_reason = FINISH_REASON_LENGTH`
- `choices[0].message`: `role`, `content` (partial visible text, may be empty),
  `tool_calls=None` (so incomplete calls can never auto-execute),
  `reasoning_content`
- `usage` (passed through, often `None`)
- `_dropped_tool_names` (list or `None`) for the dropped-tool prompt variant

---

## 2. Where Python produces a stub (three producers, one shape)

### 2a. Clean SSE end, no finish_reason, tool-call args incomplete
`chat_completion_helpers.py:4825-4842` (`_tool_args_dropped_no_finish`).
Trigger: `has_truncated_tool_args and finish_reason is None`. A tool call's
name arrived but its JSON arguments were never completed (or never received a
single byte: the zero-byte branch at `4768-4780`, #80498) and the stream then
ended with no terminator. Stub carries `_dropped_tool_names`. The long comment
at `4805-4824` is explicit that this must NOT be stamped `length`, because a
real output cap would retry with a bigger `max_tokens` and a mid-tool-call drop
would not benefit.

### 2b. Clean SSE end, no finish_reason, text-only, no usage
`chat_completion_helpers.py:4854-4869` (`_text_only_dropped_no_finish`).
Trigger, all four required:
`finish_reason is None and content_parts and not tool_calls_acc and usage_obj is None`.
The `usage_obj is None` clause (#91373) is the load-bearing discriminator: an
OpenAI-compliant provider that emits a final usage-only chunk with empty
`choices` proves the stream closed cleanly, so a present usage object means
"complete stop", not "drop". `lastOne=true` terminal frames are likewise a
clean stop (#90848), handled upstream before this point.

### 2c. Transport error mid-stream, after deltas were already delivered
`chat_completion_helpers.py:5771-5869`, inside
`interruptible_streaming_api_call`. Trigger: `result["error"] is not None and
deltas_were_sent["yes"]`. The worker thread raised (connection reset, read
error, provider exception) but tokens had already crossed the platform
boundary. Re-raising would let the outer retry loop re-send and double-emit, so
instead it recovers `agent._current_streamed_assistant_text` (`5778-5780`),
optionally appends a user-visible dropped-tool warning and fires it as a delta
(`5784-5804`), classifies the swallowed error for content-filter termination
(`5841-5864`), and returns a stub. The empty-content-is-allowed note at
`5816-5826` is important: the stub may carry empty content deliberately, and the
consumer must special-case that (see 3a). `_reset_stale_streak` is called
(`5868`) because chunks were demonstrably received.

### Not a stub: zero-chunk and provider-length
- Zero usable chunks (`finish_reason is None and not content_parts and not
  reasoning_parts and not tool_calls_acc`) raises `EmptyStreamError`
  (`4794-4803`). Malformed/empty SSE is a retry, never a stub.
- `effective_finish_reason = finish_reason or "stop"` at `4871-4873`: if the
  provider actually reported a reason, the drop paths never engage.

---

## 3. Where Python consumes a stub (conversation_loop.py)

Entry: everything below lives under `if finish_reason == "length":`
(`conversation_loop.py:4149`), which a stub satisfies by construction. Import at
`:109`. Retry counters initialised at `:2209` (`length_continue_retries`),
`:2218-2219` (`truncated_tool_call_retries`, `truncated_response_parts`).

### 3a. Empty-stub: skip the interim append
`4376-4395`. `_is_empty_partial_stub = (id == PARTIAL_STREAM_STUB_ID and not
_interim_content)` at `4377-4380`. When empty, the interim assistant message is
NOT appended to history (only the continuation user nudge is), avoiding a
persisted empty assistant turn that strict providers (Moonshot/Kimi) reject with
HTTP 400 and that poisons replay. Non-empty content is appended and tagged
`_length_continuation_fragment` (`4391-4396`), and pushed onto
`truncated_response_parts`. Note: the empty-stub case skips the
`_ephemeral_reasoning_off` set (`4381-4390`) that the thinking-only path uses;
that path is out of scope here.

### 3b. Continuation prompt choice
`4398-4437`. Three prompts, selected by `_get_continuation_prompt`
(`1329-1348`), constants at `1306-1348`:

- `_LENGTH_CONTINUATION_NETWORK_STUB` (`1312-1317`): stub, no dropped tools.
  "...cut off by a network error mid-stream. Continue exactly where you left
  off..."
- dropped-tools variant (`1330-1344`, prefix constant
  `_LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX` at `1326`): stub with
  `_dropped_tool_names`. Tells the model the tool args were too large, do not
  retry the same call, split into smaller calls under ~8K tokens.
- `_LENGTH_CONTINUATION_OUTPUT_LIMIT` (`1318-1321`): the non-stub (real length)
  fallback.

The chosen text becomes a `{"role":"user", "_length_continuation_nudge": True}`
message (`4429-4434`), appended, then `_retry.restart_with_length_continuation =
True; break` (`4436-4437`).

### 3c. Replay boundary and output-cap boost
`7399-7414`. On `restart_with_length_continuation`, the output-token budget is
boosted `base * 2**length_continue_retries`, floored at any requested cap,
capped at `max(32768, requested)`, then `continue` replays the whole loop with
the appended interim+nudge pair. The network-stub lane shares this boost with
the real-length lane (the boost is harmless for a network drop).

### 3d. Ceiling exit (4th attempt)
`4398` gate is `length_continue_retries < 4`. On the 4th:
`4439-4510`. Stitches `truncated_response_parts` through `_strip_think_blocks`
into `partial_response` (`4439`), resets `_ephemeral_reasoning_off` so the
one-shot override never leaks (`4445`), then strips every
`_length_continuation_fragment` / `_length_continuation_nudge` tagged message
from this turn (`4477-4493`) so unanswered nudges cannot re-truncate later
turns, re-appends the stitched `partial_response` as a single
`finish_reason="length"` assistant message (`4494-4499`), persists
(`4500-4502`), and returns `completed=False, partial=True` (`4503-4510`). If no
visible text accumulated, returns an actionable "no visible answer" message
instead (`4454-4476`).

### 3e. Tool-call stub retry lane
`4512-4580`. When the normalized interim has tool calls, `_is_stub_stall = (id
== PARTIAL_STREAM_STUB_ID)` at `4515-4516`. Retries up to 4 from the current
message state without appending the broken response (`4518-4549`), boosting
`max_tokens` (`4540-4545`) even for a stall (comment: harmless). On ceiling,
messaging and the final error diverge by `_is_stub_stall`: "Stream repeatedly
dropped mid tool-call (network); the tool was not executed" vs "Response
truncated due to output length limit" (`4551-4567`), and
`close_interrupted_tool_sequence` repairs a dangling tool tail (`4571`).

### 3f. Content-filter-tagged stub
`4320-4354`. A stub stamped `_content_filter_terminated` (from 2c) activates the
fallback chain on the first pass rather than burning continuation retries,
rolling partial content back to the last clean turn.

### 3g. Persistence and usage
The stub's `usage` is usually `None`; usage accounting downstream
(`4615-4642`) is gated on a real usage object, so a stub contributes a counted
API call but no token/cost. Persistence happens at the ceiling exit and on
clean completion via `_persist_session`; the fragment/nudge tags are projection-
stripped by SessionDB and recognised by the compressor's
`_is_synthetic_compression_user_turn` (the reason the prompts are named
constants, per the comment at `1306-1311`).

---

## 4. State / decision table (producer side)

Inputs observed at stream end. "FR" = finish_reason sent by provider.

| FR      | visible text | tool args | usage frame | end cause        | Python result                                  |
|---------|--------------|-----------|-------------|------------------|------------------------------------------------|
| none    | no           | none      | no          | clean EOF        | `EmptyStreamError` (retry, not a stub) 4794    |
| none    | yes          | none      | no          | clean EOF        | stub, network prompt 4854                       |
| none    | yes          | none      | yes         | clean EOF        | clean STOP (not a drop) 4858, #91373            |
| none    | any          | incomplete| any         | clean EOF        | stub, dropped-tools prompt 4825                 |
| none    | any          | none/ok   | any         | lastOne=true     | clean STOP (#90848)                             |
| length  | any          | any       | any         | provider cap     | real length continuation (output-limit prompt) |
| (raised)| deltas sent  | any       | any         | transport error  | stub via post-worker path 5771                  |
| (raised)| no deltas    | any       | any         | transport error  | re-raise (outer retry) 5870                     |

Consumer side, per stub, under `finish_reason=="length"`:

| stub content | dropped tools | prompt chosen          | interim append | ceiling action                    |
|--------------|---------------|------------------------|----------------|-----------------------------------|
| empty        | no            | network stub           | skipped 4377   | stitch parts / "no visible" 4454  |
| non-empty    | no            | network stub           | appended 4391  | stitch + re-append partial 4494   |
| any          | yes           | dropped-tools          | per above      | tool-lane ceiling 4551            |
| any          | content-filter| (fallback first) 4320  | rolled back    | fallback chain                    |

---

## 5. Current Rust coverage vs gaps

The relevant Rust surface is `NativeAgentClient::run_turn` streaming text path
and its `forward_sse` helper in
`rust/crates/hermes-gateway/src/native_agent.rs`. Tool turns go through
`step()` (non-streaming, `5680-5684`), so the mid-stream tool-args-drop producer
(2a) has no structural analog on the streaming path today.

### Already covered (by stale-stream / length handling)

- **Provider stall (inactivity timeout), no visible text.** `forward_sse`
  timeout arm sets `outcome.stalled` (`5562-5568`); caller runs the stale retry
  ladder + success-body fallback (`4658-4706`). Error enums
  `MainRequestError::StreamInactivity` / `MainDispatchFailure::StreamInactivity`
  (`1093-1138`), dispatch wiring (`2587-2677`, `4640-4655`). Tests:
  `preheader_stale_uses_stream_retry_batch_before_fallback` (7839),
  `stale_stream_breaker_survives_turns_and_stops_network_replay` (7776),
  `buffered_stale_breaker_stops_retries_and_survives_calls` (7918).
- **Provider stall after visible text.** Timeout arm synthesizes
  `finish_reason = "length"` when `outcome.visible` (`5565-5567`); flows into the
  visible-length continuation (`4771-4800`) which appends assistant+nudge,
  persists continuation messages, joins fragments, and enforces the 4-retry
  ceiling. Test: `postvisible_stale_stream_uses_length_continuation_without_replay`
  (7608, drives a `stream::pending()` stall).
- **Provider-reported length (real cap).** Non-stalled `finish_reason=="length"`
  path (`4716-4800`) with truncation classify, reasoning-only one-shot nudge,
  and the same ceiling. Tests: `length_limited_main_stream_continues_with_frozen_route`
  (6801), `length_limited_main_stream_stops_after_four_fragments` (7982),
  `tool_enabled_text_length_continuation_persists_alternating_history` (8036).
- **Empty / malformed SSE (zero usable output).** `finish_reason` empty and
  `!observed_generation` runs a 3-retry-then-fallback ladder (`4868-4894`),
  the `EmptyStreamError` analog. Tests: `empty_stream_without_finish_signal_is_not_a_model_empty`
  (8735), `exhausted_truly_empty_main_stream_is_delivery_only` (8688).
- **Pre-visible dropped TCP connection.** `dropped_main_connections_retry_once_then_use_fallback`
  (6669) drops the socket before any delta; handled by retry/fallback.
- **Continuation prompt body.** `MAIN_LENGTH_CONTINUATION_PROMPT` (`1003-1004`)
  is byte-identical to Python's `_LENGTH_CONTINUATION_OUTPUT_LIMIT`.

### Not covered (the actual remaining seam)

1. **Clean SSE EOF after visible text, no finish_reason, no usage frame
   (producer 2b).** `forward_sse` returns on `Ok(None)` (`5561`) leaving
   `outcome.finish_reason` empty. The caller hits the visible branch (`4764`)
   and, because `finish_reason != "length"`, falls straight through to the
   success return (`4801-4827`): the turn is reported COMPLETE. Python would
   build a network stub and continue. This is the #32086 regression the stub
   exists to prevent, reproduced in Rust. There is no `usage_obj is None`
   discriminator on this path at all, so Rust cannot currently distinguish a
   clean usage-framed stop from an abrupt drop.

2. **Transport error mid-stream after visible deltas (producer 2c).**
   `forward_sse` hard-errors on a byte-stream error via
   `chunk.map_err(...)?` (`5571`); the `?` at the call site (`4637`) propagates
   it. Any partial visible text already emitted is lost and no continuation is
   attempted. Python recovers it into a stub.

3. **Network-drop continuation prompt.** Rust has only the output-limit prompt.
   Python's `_LENGTH_CONTINUATION_NETWORK_STUB` and dropped-tools variant are not
   ported, so even when the synthesized-length stall path fires (covered case
   above), the model is told "truncated by the output length limit" rather than
   "cut off by a network error". Semantic divergence, not a crash.

4. **Dropped-tool-name plumbing.** No `_dropped_tool_names` analog; structurally
   moot while streaming is text-only, but a risk the moment streamed tool calls
   are added.

No Rust test exercises cases 1 or 2 (confirmed by enumerating the `async fn`
test list in `native_agent.rs`). `postvisible_stale_stream...` covers the
timeout variant only (`stream::pending`), never a clean EOF or a transport
error.

---

## 6. Narrowest deep Rust module interface

The decision logic is small and pure; the only non-pure requirement is that
`forward_sse` must first distinguish three stream-end causes it currently
conflates or drops: clean EOF, inactivity timeout, and transport error. Propose
a leaf module `main_dropped_stream` with no I/O.

```rust
// crate::main_dropped_stream

/// How the SSE byte stream ended, as observed by forward_sse.
pub enum StreamEnd {
    CleanEof,          // stream.next() -> Ok(None)
    InactivityTimeout, // timeout_at fired (already modeled as outcome.stalled)
    TransportError,    // stream yielded Err(_) mid-flight
}

/// Minimal view of what forward_sse saw, so the decision stays testable
/// without a live stream.
pub struct StreamSnapshot {
    pub end: StreamEnd,
    pub provider_finish_reason: Option<String>, // Some only if provider sent one
    pub has_visible_text: bool,
    pub saw_usage_frame: bool, // the #91373 discriminator
    pub tool_args_incomplete: bool,
    pub dropped_tool_names: Vec<String>,
}

/// The disposition the run_turn loop must act on.
pub enum Disposition {
    CleanStop,                       // treat as a complete turn
    EmptyRetry,                      // zero usable output: retry/fallback ladder
    ProviderLength,                  // real cap: existing length continuation
    PartialDrop { network: bool, dropped_tools: Vec<String> },
}

/// Pure classifier mirroring chat_completion_helpers.py:4794-4873 plus the
/// post-worker error path at 5771.
pub fn classify(s: &StreamSnapshot) -> Disposition;

/// Continuation prompt selector, mirroring conversation_loop.py:1329-1348.
pub fn continuation_prompt(d: &Disposition) -> &'static str | String;
```

Boundary rationale:

- `classify` is a single total function over an enum-typed snapshot. It isolates
  every "is this a drop" rule behind one seam, so the run_turn loop only matches
  on `Disposition` and never re-derives the finish_reason/usage/tool conditions
  inline. This is the deep-module move: a wide, messy input space collapsed to a
  four-variant output.
- The only change `forward_sse` needs is to stop throwing away the end-cause:
  return a `StreamEnd` (do not `?` the transport error when visible text exists;
  set `TransportError` and break, preserving `visible_content`), and record
  `saw_usage_frame` (it already parses usage, just needs a bool). Everything
  else stays.
- `PartialDrop { network: true }` reuses the existing visible-length
  continuation machinery (`4771-4800`) with a different prompt string; no new
  persistence or ceiling code. `network: false` (future streamed tool calls)
  carries `dropped_tool_names` for the chunking prompt.
- Keep the three neighbours distinct at the enum level: `InactivityTimeout`
  stays on the stale ladder (unchanged), `EmptyRetry` stays on the empty ladder
  (unchanged), `ProviderLength` stays on the real-cap path (unchanged). Only
  `CleanEof`/`TransportError` with visible text route into `PartialDrop`.

Two new prompt constants are needed next to `MAIN_LENGTH_CONTINUATION_PROMPT`
(`1003-1004`), byte-identical to Python `1312-1344`.

---

## 7. Public run-turn test cases to port

Anchored to Python tests; these are the behaviors a Rust `run_turn` must pin.

1. **Text-only clean drop, no usage, continues (not stop).** Stream emits
   "part one" then EOF with no finish_reason and no usage frame; run_turn must
   issue a continuation and stitch, not return complete. Python:
   `test_text_stream_abrupt_drop_without_usage_still_returns_stub`
   (test_partial_stream_finish_reason.py) +
   `test_partial_stream_stub_does_not_exit_loop_immediately`. This is the
   primary gap (case 5.1).

2. **Text stream with final usage-only chunk completes as stop.** Same shape but
   a trailing usage frame must force `CleanStop`, no continuation. Python:
   `test_text_stream_with_final_usage_chunk_completes_as_stop`,
   `test_last_one_usage_frame_is_a_clean_stop` (#90848). Guards against
   over-triggering the new path.

3. **Transport error after visible deltas recovers partial, continues.** Stream
   emits visible text then the socket errors; run_turn must preserve the emitted
   text and continue rather than propagating a hard error (case 5.2). Python:
   producer `chat_completion_helpers.py:5771-5869` + loop continuation.

4. **Network-drop continuation uses the network prompt.** The injected nudge
   after a drop must be the network-error wording, while a real
   `finish_reason="length"` uses the output-limit wording. Python:
   `test_partial_stream_stub_uses_network_prompt`,
   `test_no_id_falls_through_to_length_prompt`.

5. **Ceiling: 4 drop continuations then keep the stitched partial.** After 4
   failed continuations, return `partial=True` with the stitched visible text
   and strip every fragment/nudge tag from the turn. Rust already has the
   analog for real length (`length_limited_main_stream_stops_after_four_fragments`,
   7982); this asserts the drop lane reuses it. Python: `4439-4510`.

6. **Empty drop (no visible text) stays on the empty/stale ladder, not the
   partial lane.** A zero-output drop must not synthesize a partial-drop
   continuation. Python: `EmptyStreamError` at `4794-4803`; Rust analog
   `empty_stream_without_finish_signal_is_not_a_model_empty` (8735) must remain
   green.

7. **Stall-after-visible remains on the synthesized-length path.** Regression
   guard that adding the clean-EOF path does not reclassify the timeout case.
   Rust: `postvisible_stale_stream_uses_length_continuation_without_replay`
   (7608) must remain green.

---

## 8. Unresolved risks

- **Usage-frame detection fidelity (case 5.1 / test 2).** The whole clean-drop
  decision hinges on `saw_usage_frame`. Rust parses usage via
  `provider_usage::from_sse_line` (`5580-5586`, `5618-5624`) but does not record
  a "a usage frame was present" bool distinct from "usage had nonzero tokens".
  Python's discriminator is `usage_obj is None` (object presence, not token
  counts). The port must track presence, not magnitude, or a zero-token usage
  frame will be misread as a drop.

- **`forward_sse` error handling change is load-bearing and subtle.** Today the
  `?` at `5571` / `4637` means a transport error is never observable as a
  partial. Changing it to preserve visible text and return `TransportError`
  must not regress the pre-visible dropped-connection retry path
  (`dropped_main_connections_retry_once_then_use_fallback`, 6669), which relies
  on the error propagating to the retry ladder when no deltas were sent. The
  branch must be `has_visible_text` (mirrors Python `deltas_were_sent`), exactly.

- **Double-emit on continuation replay.** Python streams deltas live, then on
  continuation the model is told "do not repeat prior text"; the stitched result
  joins fragments. Rust emits `StreamEvent::MessageChunk` as it goes
  (`5598-5600`) and also joins via `main_join_length_parts`. A partial-drop
  continuation that the model partially repeats could double-surface text to the
  platform. The existing length lane already faces this; the drop lane inherits
  the same risk and should reuse `continuation_join_after` (`4776`) boundary
  handling rather than re-inventing it.

- **Persistence of the empty-drop stub.** Python deliberately skips appending an
  empty stub to history (`4377-4395`) to avoid HTTP-400 session poisoning on
  replay. Rust's continuation persistence
  (`append_native_continuation_messages`, `4802-4824`) must apply the same
  skip-empty rule, or a drop before the first visible byte will persist an empty
  assistant turn. Note this is the same empty-non-final-message hazard flagged in
  `main-provider-truncation-review-claude.md` Finding 1; the drop lane is a
  second entry point into it.

- **Content-filter-tagged drop.** Producer 2c can stamp
  `_content_filter_terminated` and the consumer activates fallback first
  (`4320-4354`). If the transport-error recovery is ported without the
  classifier call, a content-filtered mid-stream termination would be retried on
  the same primary instead of failing over. Lower priority while the Rust
  streaming path is text-only, but it travels with case 5.2.

- **Scope collision with the stall lane's repetition guard.** Per the prior
  liveness/truncation reviews, a synthesized-length stall now flows into the
  repetition guard at the loop level. A newly added clean-EOF partial-drop must
  route as a drop, not through that guard, to honor the AGY contract that
  excludes dropped streams from degenerate-loop aborts. Keep `PartialDrop`
  distinct from the synthesized-length path if the guard cannot tell them apart.
