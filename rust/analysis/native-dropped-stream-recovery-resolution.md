# Native dropped chat-completions stream recovery

Date: 2026-09-11

## Outcome

Native no-tools chat-completions streams now recover Python's text-only dropped
stream contract. Visible output is no longer accepted as complete when the SSE
body ends without a provider finish reason or usage object. A body transport
error after generation also preserves the already-delivered prefix instead of
replaying the original request or immediately entering provider fallback.

Both cases issue Python's exact network continuation prompt on the same frozen
route. The follow-up reuses the immutable system prompt, tool schema, request
policy, credentials, and provider cursor. The existing progressive output-cap
schedule and four-attempt ceiling remain the single continuation owner.

## Completion evidence and stream ends

`main_dropped_stream.rs` owns a pure classifier over three stream-end states:

1. Clean body EOF.
2. Protocol `[DONE]`.
3. Body transport error.

A provider finish reason or a non-null usage object proves a clean EOF or
`[DONE]` completion. Usage presence is sufficient even when every token count
is zero. Nous-style `lastOne` values also normalize to `stop` when they are the
boolean `true`, integer `1`, or string `"true"`, either at the top level or in
`model_extra`.

`[DONE]` alone does not prove the response complete. If visible text arrived
without finish or usage evidence, clean EOF and `[DONE]` both enter network
continuation. A body transport error takes precedence over previously parsed
finish or usage fields and becomes recoverable after any generation delta. If
the transport fails before generation starts, the error still propagates to
the existing replay-safe retry and fallback dispatcher.

## Runtime and transcript behavior

The SSE reader retains a body error long enough to parse one final buffered
provider line without a newline. This prevents a valid last delta from being
discarded merely because chunked transfer termination failed afterward.

For a visible dropped fragment, the loop:

1. Delivers the fragment once.
2. Adds an assistant fragment with transcript-only `finish_reason="length"`.
3. Adds the exact network continuation user prompt.
4. Raises the next output cap through the existing bounded schedule.
5. Requests only the missing suffix on the same route.

The fragment and nudge are committed through the existing immediate SQLite
continuation transaction before the final suffix is published. Gateway history
then stores only that suffix as the current turn reply. Reopening the session
therefore reproduces the exact alternating provider history without duplicating
the stitched answer.

A transport error after reasoning but before visible text does not create an
empty assistant message. The network nudge merges into the current user-side
provider projection, reasoning remains enabled, and strict providers never see
an invalid empty assistant turn.

At four consecutive dropped continuations, visible fragments are kept and the
turn ends as a bounded partial failure. Four empty recoveries produce the
existing delivery-only no-visible-answer result. Recoverable drops reset stale
stream health because the route demonstrably generated output. They do not
penalize credential pools or activate cross-provider fallback.

## Python contract and helper split

AGY owned the executable reference lane. Its corrected generator contains 47
cases across 11 sections and drives the live Python stream accumulator and
`AIAgent.run_conversation()` loop. Primary review rejected its first local
`_simulate_*` copies, after which AGY removed them and proved interim
suppression, four-attempt cleanup, and content-filter fallback through the real
conversation loop. Every declared golden field is asserted before output. See
[dropped-stream-contract-agy.md](dropped-stream-contract-agy.md) and the
[47-case corpus](../tools/dropped-stream-goldens.json).

Claude worked on a separate, non-overlapping future lane while the oracle and
implementation proceeded. It found one remaining run-budget dependency: only
an implicit buffered stale deadline is capped by half the current turn's
remaining wall-clock budget, with a 60-second floor. See
[main-provider-run-budget-scaling-claude.md](main-provider-run-budget-scaling-claude.md).

The earlier independent seam map remains useful for the complete Python stub
shape and the boundaries that are not structurally reachable in the current
Rust transport. See
[dropped-stream-recovery-claude.md](dropped-stream-recovery-claude.md).

## Verification

Public local-HTTP and SQLite tests prove:

- clean EOF after visible text uses exactly one network continuation and
  durable byte-exact replay
- a chunked body failure after visible text parses the final unterminated SSE
  line, preserves the prefix, and does not call fallback
- a reasoning-only transport failure suppresses the empty assistant row and
  keeps reasoning configuration unchanged
- zero-count usage and `lastOne` terminal frames complete with one request
- four repeated dropped streams stop at the exact continuation ceiling
- old clean-success fixtures now carry explicit `finish_reason="stop"`, while
  intentional drop fixtures remain incomplete

The full workspace passes 1,937 Rust tests with two ignored. The focused Python
suite passes 25 tests. The 47-case corpus regenerates byte for byte with SHA256
`8c4ad1dc221b688ed2e9481f624bbe20e9d2ee08da44707a632f37b12fa3e67d`.
Rust formatting, Python formatting, Ruff, workspace Clippy with warnings denied,
and diff hygiene pass.

## Deliberate limits and next seam

- Native tool-enabled turns use a buffered response rather than streamed tool
  deltas. Python's incomplete streamed tool-argument stub and dropped-tool-name
  prompt therefore have no reachable Rust transport yet and remain tied to a
  future streamed-tool implementation.
- Reqwest body errors do not expose the provider SDK exception taxonomy needed
  to recognize a content-policy failure hidden inside a transport exception.
  Explicit SSE content filters and refusals keep their existing handling. This
  checkpoint does not guess policy state from arbitrary socket text.
- Meaningful-event inactivity remains owned by the stale-stream policy. It is
  distinct from clean EOF and body transport failure.
- Non-chat transports retain their own recovery checkpoints.

The weighted full-port estimate is **58.95%**, reported as about **59%**, with a
judgment range of **55% to 61%**. Native agent core moves from 82% to 83%; the
other weighted areas remain unchanged. The next isolated main-provider seam is
run-budget-aware buffered stale scaling, followed by operator notices and the
remaining non-chat, OAuth, and dynamic-provider routes.
