# Rust seam: ordinary main-turn successful-body validation and fallback

Scope: the narrowest safe ownership seam for the part of the ordinary main-turn
path that runs *after* an HTTP 200 is accepted, that is malformed or structurally
missing bodies, empty assistant responses, and content-policy refusals, plus the
same-route-retry versus cross-provider-fallback decision for those cases. The
pre-body transport, overload, and server-status retry lane landed at `38748fd20f`
("Port native main provider retries") and its sibling analysis is
`main-provider-retry-seam-claude.md`. This document does not rebuild that lane and
does not re-derive its budget or backoff. It picks up exactly where
`send_main_request` returns a success `reqwest::Response`.

This is a Rust ownership and interface lane. The Python behavior contract and its
goldens belong to the parallel agy lane
(`main-provider-success-body-contract-agy.md`,
`gen_main_provider_success_body_goldens.py`,
`main-provider-success-body-goldens.json`, none landed yet, the task files are
freshly created and untracked). This document does not assert Python behavior. It
cites the live Rust checkout only, at `38748fd20f` plus the two untracked task
files, and every symbol is anchored to a line range readable in the working tree.
Where a decision needs the oracle before it is safe, section 9 says so.

All line citations are `crates/hermes-gateway/src/native_agent.rs` unless the path
says `native_tools.rs` or another file.

## 0. Verified current behavior: HTTP 200 to final text or tool calls

There are two transports and they consume a 200 body in two structurally
different places. The seam has to respect that split, so it is mapped first.

### 0.1 The dispatch boundary is pre-body for both transports

`dispatch_main_turn` (`2117` to `2164`) reads the sticky cursor
`MainFallbackState.active` (`1539`, read at `2121` to `2126`), builds the body for
the serving route through the caller's closure (`2131`), and calls
`send_main_request` (`2133`). `send_main_request` (`2166` to `2365`) returns the
first success as a raw `reqwest::Response` the instant the status line is a success
(`2219` to `2221`); it never touches the answer body. On success `dispatch_main_turn`
commits the cursor (`2135` to `2139`) and hands back a `MainDispatch { response,
provider }` (`1550`, returned at `2140` to `2143`). So at the moment dispatch
returns, zero answer bytes have been inspected and the cursor is already committed
to the serving route. Everything in this document happens after that return.

### 0.2 Streaming transport (no tools)

`run_model_turn` (`3917` to `3971`) takes the no-tools branch at `3951`. It calls
`dispatch_main_turn("", ...)` with `stream: true` in the body
(`build_request_body_from_messages`, `400`, stream flag at the `json!` block), then
feeds `dispatched.response.bytes_stream()` straight into `forward_sse`
(`3963` to `3968`), captures usage (`3969`), and returns `Ok(None)`.

`forward_sse` (`4383` to `4455`) parses SSE line by line, emits each decoded delta
as `StreamEvent::MessageChunk` the moment it is scrubbed and non-empty (`4413` to
`4417`), sends exactly one terminal `StreamEvent::MessageStop { final_: true }`
(`4453`), and returns `Ok(usage)` (`4454`). Verified consequence for a malformed or
empty successful 200: a non-SSE JSON body such as `{"choices":[]}` produces no
`data:` line, so `parse_sse_line` yields `Ignore`, no `MessageChunk` is ever sent,
`MessageStop` is sent, and the function returns `Ok(None)`. The turn completes
successfully with no visible text and no error. There is no refusal detection and
no empty-body detection on this path at all.

### 0.3 Tool-round transport (`ChatModel::step`)

`step` (`4476` to `4542`) calls `dispatch_main_turn("step", ...)` with
`stream: false` (`4484`), then fully buffers the body with `.json()` (`4507` to
`4511`). A decode failure here is a terminal `Error::Other("native agent step
decode: ...")` (`4511`). It captures usage (`4512` to `4516`), then requires
`choices[0].message` and returns a terminal `Error::Other("native agent step: no
choices[0].message")` when it is absent (`4517` to `4521`). Otherwise it repairs
tool-call names (`4525` to `4540`) and returns `parse_message_step(&message)`
(`4541`).

`parse_message_step` (`native_tools.rs:275` to `382`) is the decoder. If
`tool_calls` is a non-empty array with at least one parsable call it returns
`Step::ToolCalls { calls, assistant_message }` (`native_tools.rs:353` to `356`),
carrying a replay-faithful `assistant_message` (raw argument strings and reasoning
sidecars preserved, `native_tools.rs:318` to `352`). Otherwise it falls to the
`Final` branch (`native_tools.rs:360` to `381`): it reads `content`, and only when
`content` is whitespace-empty it promotes a non-empty `message.refusal` string into
the visible text (`native_tools.rs:365` to `380`), returning `Step::Final(content)`.
So a refusal is not a distinct signal; it is folded into `Final` text.

`run_tool_loop_with_messages` (`native_tools.rs:587` onward) drives the rounds:

- `Step::Final(text)` (`native_tools.rs:614`): it computes
  `visible_response::answer(&text)` (`native_tools.rs:615`), which strips protocol
  and thinking scaffolding and returns `None` when nothing visible remains
  (`visible_response.rs:65` to `69`). If the answer is empty *and* no housekeeping
  answer is pending *and* it has not already retried *and* there is no inline
  thinking *and* one of the last five messages is a `tool` role, it performs one
  same-route retry: it pushes a synthetic `(empty)` assistant message and an
  `EMPTY_TOOL_RESPONSE_NUDGE` user message, both flagged
  `_empty_recovery_synthetic` so they never persist, and `continue`s
  (`native_tools.rs:616` to `632`). Otherwise it emits the visible answer (or the
  housekeeping answer, or nothing) plus one `MessageStop` and returns
  (`native_tools.rs:636` to `643`).
- `Step::ToolCalls` (`native_tools.rs:645`): name repair
  (`native_tools.rs:649` to `659`), wholly-invalid-name strike counting with a
  terminal error at three strikes (`native_tools.rs:662` to `680`), invalid-JSON
  argument retries with a truncation guard (`native_tools.rs:687` to `733`), then
  `persist_tool_loop_message(&assistant_message)` (`native_tools.rs:767`) which
  writes the durable assistant row before any tool executes, then per-call
  `ToolCallChunk` emission and execution (`native_tools.rs:769` onward).

### 0.4 What is verified as unhandled or divergent today

- Streaming malformed or empty 200 is a silent successful empty turn (section
  0.2). No retry, no fallback, no error.
- Tool-round malformed or empty 200 is a hard terminal error at `step`, either the
  `.json()` decode error (`4511`) or `no choices[0].message` (`4521`). Both convert
  through `From<Error> for MainRequestError` into `Internal` and reach the
  dispatch catch-all `Err(error) => return Err(error.into_error())` (`2161`). No
  retry, no fallback.
- Empty `choices` array specifically: on the tool path `choices.get(0)` is `None`,
  so it hits the `no choices[0].message` terminal (`4521`). On the streaming path
  it is the silent empty turn. The existing test
  `overloaded_main_route_retries_once_then_uses_fallback` (`4667`) even has its
  fallback server return `{"choices":[]}` (`4701`) and dispatch treats that 200 as
  success (`4729`), confirming a malformed 200 is accepted as success at the
  dispatch boundary.
- Refusal on the tool path is surfaced once as visible text and is neither retried
  nor failed over (`native_tools.rs:365` to `380`, and the existing test
  `refusal_only_http_response_reaches_the_user_once`, `7978` to `8029`, asserts one
  request and one `MessageChunk`). Refusal on the streaming path is whatever the
  SSE frames contain; there is no `finish_reason == "content_filter"` handling
  anywhere.
- The one existing post-body recovery, empty-after-tool
  (`native_tools.rs:616` to `632`), is a same-route retry that lives *above*
  dispatch in the tool loop. It re-enters `step`, so it re-enters
  `dispatch_main_turn` on the same sticky cursor. It does not consult any failure
  class and cannot cross to a fallback provider.

### 0.5 The pre-body classes that look post-body but are not

`MainPoolFailure::FormatError` (`910`) is not a 200-body class. It is produced only
by `main_retry_failure` for a 500 or 502 status carrying request-validation markers
(`1293` to `1313`), and it is a terminal pre-body class that activates fallback
(`920` to `933`, and the terminal `matches!` at `2271` to `2286`). Do not overload
it for the successful-body cases; it is about a non-success status whose body text
names a bad parameter, which is a different axis from a 200 whose structure is
wrong.

## 1. Q1. Exact files, types, functions, and control flow from HTTP 200

Two ordered layers, both already present:

1. `send_main_request` (`2166`) to the success return (`2219` to `2221`). Pre-body.
   Owns transport, overload, server, pool rotation. Returns `reqwest::Response`.
   Out of scope here except that it is the producer of the 200 this lane validates.
2. `dispatch_main_turn` (`2117`) commits the cursor (`2135`) and returns
   `MainDispatch` (`2140`). This is the last shared choke point before the two
   transports diverge, and it is where a post-body validator has to be injected if
   the two transports are to share one fallback decision.
3a. Streaming: `run_model_turn` (`3951` to `3969`) then `forward_sse` (`4383`).
    Body consumed incrementally.
3b. Tool round: `step` (`4507` buffer, `4517` structure check) then
    `parse_message_step` (`native_tools.rs:275`) then the loop's `Final` and
    `ToolCalls` handling (`native_tools.rs:614`, `:645`). Body consumed whole.

## 2. Q2. Earliest point a 200 can still be safely replayed or failed over

Safe, because no visible effect has occurred yet:

- Immediately after `send_main_request` returns the 200 and before either
  transport consumes bytes (`2219` to `2221`, then the two `dispatch_main_turn`
  callers at `3963` and `4507`). Nothing has been sent to the caller and no tool
  has run. This is the widest safe window and it is shared by both transports.
- Tool round only: after `.json()` buffers the whole body (`4507`) and after
  `parse_message_step` decodes it, but strictly before the loop persists the
  assistant row (`native_tools.rs:767`) and before any `ToolCallChunk`
  (`native_tools.rs:770`) or tool `.call` (`native_tools.rs:801`). The body is
  fully in hand, no durable transcript row exists yet, and no tool has executed, so
  a bad structure, empty content, or refusal detected here is still freely
  replayable or fail-over-able. This is the natural home for a tool-path validator.

The key asymmetry: for the tool round the safe post-body window is real and wide
(the whole body is buffered before any side effect). For streaming there is no safe
post-body window at all, because the first decoded delta is emitted before the next
byte is read (section 3).

## 3. Q3. The point after which replay must be forbidden

- Streaming: the first `StreamEvent::MessageChunk` emitted by `forward_sse`
  (`4416`). Once any text has reached the caller, a replay or fail-over
  double-emits. A truncation or stream error after that must fail the turn. The
  existing test `partial_main_stream_is_not_replayed_or_sent_to_fallback`
  (`4793` to `4872`) already pins this: one delta is delivered, the body then fails,
  the result is `Err`, the visible text is exactly `partial`, the primary was hit
  once and the fallback zero times. Because `forward_sse` starts consuming the
  moment dispatch returns, there is no seam position between "200 accepted" and
  "first delta emitted" on this path. The correct and safe design consequence:
  the streaming path gets no post-body validator. It cannot buffer a whole answer
  to validate it without defeating streaming, and any validation it could do would
  land after the replay barrier.
- Tool round: the durable persist at `native_tools.rs:767`
  (`persist_tool_loop_message`) and, after it, each `ToolCallChunk`
  (`native_tools.rs:770`) and tool `.call` (`native_tools.rs:801`). Once the
  assistant tool-call row is persisted or a tool has run, the round has a durable
  effect and must not be replayed. This is exactly why any successful-body
  validator for the tool path must sit inside `step` or between `step` returning
  and the loop persisting, never after.
- A `.json()` read failure on a 200 (`4511`): the send succeeded and the body read
  failed. No tool ran for this `step`, so it is technically replayable, but
  replaying means re-issuing a billed non-idempotent request. Today it is terminal.
  Keep it terminal until the oracle says otherwise (section 9).

## 4. Q4. Representing a recoverable successful-body failure without conflation

The design constraint is that a bad 200 must not borrow the pre-body machinery.
`MainPoolFailure` (`906`) is about transport health and credential-pool health: its
variants drive pool rotation inside `send_main_request` (`2263` to `2363`) and the
primary cooldown via `arms_primary_cooldown` (`935` to `940`). A malformed 200, an
empty answer, or a refusal says nothing about the credential and nothing about
transport, so it must not rotate a pool key and must not arm a cooldown. Folding it
into `MainPoolFailure` would do both by accident.

Recommendation. Introduce a separate, small post-body classification distinct from
`MainPoolFailure`, for example:

```
enum SuccessBodyFailure {
    Malformed,   // undecodable body, or missing choices[0].message
    Empty,       // decoded, but no visible content and no tool calls
    Refusal,     // finish_reason == "content_filter" or a populated message.refusal
}
```

Properties that keep it unconflated:

- It never reaches `send_main_request` and never touches `rotate_after_failure`
  (`1057`) or `recovery_attempts` (`2287`). Pool health is untouched.
- It does not implement `arms_primary_cooldown`. A bad 200 from the primary must
  not park the primary on the exponential cooldown ladder that `activate_main_fallback`
  runs for rate and billing classes (`2075` to `2081`).
- It carries its own fallback-eligibility and retry-eligibility, decided per
  variant, not inherited from the transport taxonomy: `Malformed` and `Empty` are
  fail-over-first then bounded same-route retry only when the chain is exhausted;
  `Refusal` is fail-over-at-most-once and never same-route retried (this mirrors
  the existing tool-loop refusal behavior at `native_tools.rs:365` to `380` and the
  test at `7978`, which retries zero times).
- The retry budget it consumes, if any, is a distinct counter from the pre-body
  `request_failures`/`recovery_attempts` in `send_main_request` (`2175`, `2174`)
  and from the tool loop's `invalid_name_retries`/`invalid_json_retries`
  (`native_tools.rs:606`, `:607`). Conflating with any of those changes an
  unrelated budget.

## 5. Q5. How the fallback should behave for each concern

Anchored to the mechanisms that already exist so the successful-body case reuses
them rather than inventing parallel state.

- Route cursor. Reuse `MainFallbackState.active` (`1539`) and
  `activate_main_fallback` (`2063`). A successful-body fail-over should advance the
  cursor exactly like a pre-body class does, but through a path that passes a
  `failure.arms_primary_cooldown() == false` equivalent, so `2075` to `2081` does
  not fire. In practice this means either a `SuccessBodyFailure` that maps to a
  cooldown-free activation or a dedicated advance that only sets `state.active =
  next` (like `2082`) without the cooldown arm. Stickiness is then automatic: the
  next tool round reads the advanced cursor at `2121`.
- Per-route retry budget. Do not spend the pre-body budget. `main_attempt_limit`
  (`943` to `955`) and `self.main_retry.max_attempts` (`1557`, default 3 at `1564`)
  are the transport budget. A successful-body same-route retry, when it happens at
  all, is at most one attempt for the tool path, matching the existing single
  empty-after-tool retry (`native_tools.rs:611`, `:626`). Keep it a separate small
  bound so a flapping malformed primary cannot burn the transport budget.
- Prompt bytes. Unchanged and load-bearing. `dispatch_main_turn` rebuilds the body
  per route through the closure (`2131`), and within a route `send_main_request`
  re-sends the same `&Value` byte-for-byte. The existing tests already assert byte
  stability across a same-route retry and message identity across a fail-over
  (`overloaded_main_route_retries_once_then_uses_fallback` at `4730` to `4739`). A
  successful-body fail-over must preserve the same property: the fallback route
  gets its own model and extras through the closure while the primary bytes are
  untouched. The empty-after-tool retry already preserves the tool-result prefix
  and appends only synthetic non-persisted rows (`native_tools.rs:629` to `630`,
  asserted at `7964` to `7975`); a fail-over must not carry those synthetic rows
  into the fallback request.
- Request value. The closure at `3951` (streaming) and `4479` (tool) is the single
  builder. A successful-body fail-over re-invokes it for the new route, so provider
  extras, output cap, and cache keys are recomputed per route by
  `apply_provider_extras` (`2394`). No new builder.
- Usage. `capture_usage` (`2563`) runs only on a returned success. A failed 200
  that is then failed over should not double-count: the tool path currently
  captures usage from the bad body at `4512` before it errors, which means a
  malformed 200 that becomes a fail-over would capture usage twice unless the
  validator runs before `capture_usage`. Recommendation: move the successful-body
  validation ahead of `capture_usage` for the case that will fail over, so only the
  accepted body's usage is booked. This is a real ordering constraint, not a
  preference.
- Persistence. No durable row may be written for a rejected body. On the tool path
  the persist is `native_tools.rs:767`, which is after `parse_message_step`
  returns, so a validator that rejects before the loop persists is naturally safe.
  On the streaming path there is no persist inside `forward_sse`; the durable
  capture is the caller's job in `run_native_turn` (`4076` to `4090`) and only
  happens for a non-empty response, so a rejected streaming body must surface as an
  `Err` from `forward_sse`, not as `Ok(None)`, or it silently persists nothing and
  reports success.
- Tool rounds. A successful-body fail-over inside a multi-round tool conversation
  must keep the already-persisted prior rounds and only replace the current round's
  request. The cursor advance is per client and sticky, so subsequent rounds
  continue on the fallback route. The empty-after-tool same-route retry already
  demonstrates the "append synthetic, keep prefix, do not persist" discipline
  (`native_tools.rs:627` to `631`); a cross-provider variant must additionally not
  leak the synthetic rows across the provider switch.

## 6. Q6. Concrete public-seam real-HTTP red tests to add first

The established pattern is axum servers bound to `127.0.0.1:0` via
`serve_main_retry` (`4555`), a `NativeAgentClient` with
`.with_main_fallback_routes(...)` and `.with_main_retry_backoff(Duration::ZERO)`
(`4712` to `4716`), and a direct call to `dispatch_main_turn` or a higher entry
like `run_turn` / `run_tool_loop_*`. Each test below is written to fail at
`38748fd20f`.

1. Streaming empty 200 does not silently succeed. Primary returns a 200 whose body
   is `{"choices":[]}` (or an SSE stream with only `[DONE]`), one live fallback.
   Drive `run_turn`. Assert the turn does not complete as a silent empty success:
   either the fallback serves and its text reaches the user, or the turn errors.
   Fails today because `forward_sse` returns `Ok(None)` with no visible text
   (section 0.2). This is the highest-value red test because the current behavior
   is a silent wrong answer.
2. Tool-round empty `choices` fails over rather than hard-erroring. Primary `step`
   returns `{"choices":[]}`, fallback returns a normal answer. Drive
   `run_tool_loop_with_content`. Assert the fallback served the round, no tool
   executed on the empty response, and the user got the fallback's answer. Fails
   today at the `no choices[0].message` terminal (`4521`).
3. Malformed 200 body (undecodable JSON) on the tool path is recoverable. Primary
   returns `200` with body `not json`, fallback returns a normal answer. Assert
   fail-over and that no durable assistant row was persisted for the malformed
   response. Fails today at the `.json()` decode terminal (`4511`).
4. Refusal on the tool path tries a fallback at most once and never same-route
   retries. Extend `refusal_only_http_response_reaches_the_user_once` (`7978`) with
   a configured fallback: primary returns `finish_reason == "content_filter"` with
   a populated `refusal`, fallback returns a normal answer. Assert at most one
   primary request, at most one fallback attempt, and no same-route retry. Pins the
   non-retryable rule for `SuccessBodyFailure::Refusal`.
5. `finish_reason == "content_filter"` with non-empty `content` is not discarded.
   Primary returns a 200 whose message has both visible `content` and
   `finish_reason == "content_filter"`. Assert the visible content still reaches
   the user and is not swallowed as a refusal (guards against a validator that
   treats the flag alone as fatal, matching the existing "never discard a usable
   answer" comment at `native_tools.rs:362` to `364`).
6. Streaming truncation after a delta still does not replay. Keep
   `partial_main_stream_is_not_replayed_or_sent_to_fallback` (`4793`) green. Any
   successful-body work must not regress it; add an assertion that the fallback is
   never contacted even when a fallback is configured, which it already is.
7. Successful-body fail-over does not double-count usage. Primary returns a
   malformed 200 carrying a `usage` block, fallback returns a normal answer with
   its own `usage`. Assert only the fallback usage is booked. Pins the ordering
   constraint in Q5.
8. Successful-body fail-over does not arm the primary cooldown. Primary returns a
   malformed 200, fallback serves, then read `client.main_fallback.state`
   (the existing tests reach into this at `5137` onward) and assert
   `cooldown_until` is `None` and `rate_limit_backoff_count` did not advance. Pins
   the "not conflated with pool health" rule of Q4.

## 7. Q7. Regressions and interactions with the pre-body retry checkpoint

The pre-body lane at `38748fd20f` changed the exact code this lane extends, so the
interactions are direct.

- Terminal `matches!` list. `send_main_request` now returns `Transport`,
  `Overloaded`, and `ServerError` as terminal before pool rotation (`2271` to
  `2286`), and the later credential-recovery `match` at `2323` to `2333` marks them
  `unreachable!()`. Any successful-body class must not be added to `MainPoolFailure`
  and routed through `send_main_request`, or it risks reintroducing an
  `unreachable!()` reachability bug of the kind the retry review already caught. Keep
  it a separate type (Q4).
- Shared `dispatch_main_turn` return. Both transports depend on the fact that
  dispatch returns a raw `Response` before body consumption (`2140`). A validator
  injected at the dispatch boundary must remain optional per transport (tool only),
  or it changes the streaming path's zero-buffer contract and the partial-stream
  test (`4793`) breaks.
- `capture_usage` ordering. The tool path books usage at `4512` before the
  structure check at `4517`. The retry lane did not move this. A successful-body
  validator that fails over after `4512` double-books (Q5, test 7). This is the
  single most likely silent regression.
- Cursor and cooldown. `activate_main_fallback` (`2063`) arms the exponential
  cooldown only when `failed_index == 0 && failure.arms_primary_cooldown()`
  (`2075`). If a successful-body fail-over is wired through a `MainPoolFailure`
  value whose `arms_primary_cooldown` is true, a malformed primary would park on
  cooldown and oscillate the prompt cache tenant every turn. The retry review
  already flagged cache oscillation as the main risk on the primary-cooldown gate;
  a successful-body class inheriting it would amplify exactly that. Keep it
  cooldown-free (Q4).
- Empty-after-tool retry versus a new same-route successful-body retry. The tool
  loop already spends one same-route retry for empty-after-tool
  (`native_tools.rs:611` to `632`). A new successful-body same-route retry must not
  stack with it on the same round, or a single empty body could trigger two silent
  re-requests and diverge from the single-retry the existing test asserts
  (`7963`, `assert_eq!(requests.len(), 3)`: one tool call, one empty, one recovery).
- `main_retry.backoff_base` reuse. Same-route successful-body retries, if any,
  should route through the injectable `with_main_retry_backoff`
  (`1845`) / `wait_before_main_retry` (`2367`) so tests can zero the delay, exactly
  as the pre-body tests do (`4716`). A hard `tokio::time::sleep` outside that hook
  would make the new red tests hang.

## 8. Deferred scope

- Non-chat transports. Anthropic messages, Responses/codex, and Bedrock converse
  have different refusal and finish-reason shapes. This lane is chat-completions
  only, matching the single wire shape the native client sends and the "no broad
  abstraction without a second real adapter" rule the retry seam already invoked.
- Streaming refusal and mid-stream `finish_reason == "content_filter"`. Because the
  streaming path has no safe post-body window (section 3), detecting a refusal that
  only appears in the final SSE frame cannot fail over without violating the replay
  barrier. Whether a refusal that appears before the first delta should fail over is
  a real but narrower question and depends on the oracle's stance on pre-delta SSE
  inspection.
- Context-overflow and other request-shape repairs on a non-success status remain
  the pre-body `error_classifier` catalog and stay out of this lane.

## 9. Prerequisites and what makes this checkpoint unsafe

The agy successful-body contract is not landed. Do not implement until it pins, for
the chat-completions shape only: whether malformed and empty 200 are fail-over-first
or same-route-retry-first, and the exact same-route bound; the precise refusal
detection surface (`finish_reason == "content_filter"` alone, `message.refusal`
alone, or both, and their precedence when visible content is also present); whether
a `.json()` read failure on a 200 is replayable; whether an empty visible response
with non-empty reasoning is empty for this purpose (the current tool loop treats
inline thinking as a reason to *not* retry, `native_tools.rs:619`); and whether
empty content paired with valid tool calls is ever an "empty" case (today it is
`ToolCalls`, never `Final`, so it never reaches the empty branch). Also required
before implementation: keep the successful-body classifier a distinct type from
`MainPoolFailure`; route any same-route delay through the existing injectable
backoff so the red tests can run; and place the validation before `capture_usage`
for the tool path.
