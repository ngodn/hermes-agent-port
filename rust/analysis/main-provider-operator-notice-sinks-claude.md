# Main-provider operator-notice delivery sinks (Rust)

Scope: map every current `StreamEvent::GatewayNotice` consumer in the Rust
gateway, find exactly where notices are dropped, identify which existing adapter
method can deliver them, work out how to keep them ordered against message
chunks and stops, decide whether the HTTP `/message` response can carry them
without breaking its public schema, then recommend the narrowest production
delivery seam plus 3 to 5 public tests. Notices stay presentation-only: no
transcript persistence, no prompt mutation.

All line references below were read against current source on the
`rust-rewrite` branch.

## The event and its contract

`StreamEvent` is defined at `hermes-core/src/stream.rs:25`. The relevant
variants:

- `GatewayNotice { notice_kind, text, extra }` (`stream.rs:98-104`). The doc
  comment names it "a gateway-originated control message" and says
  `notice_kind` is a stable string the adapter can switch on. `text` already
  carries the rendered display string; the producer builds it.
- `LongToolHint { tool_name, duration }` (`stream.rs:73-78`). Defined but has
  no producer anywhere in the crate, so it is dead today.

The module header (`stream.rs:13-14`) states the invariant that matters here:
these events "carry transport/presentation only. Nothing here is conversation
history." That is the load-bearing rule for notices.

## Producers today

There is exactly one `GatewayNotice` producer in the whole crate:
`native_agent.rs:5146-5151`, which emits `notice_kind: "memory_recall"` with the
external-memory recall indicator text. There is no wait notice, no
retry/fallback notice, no provider-switch notice, and no refusal notice in Rust
yet. `extra` is always empty in current code.

## Consumers (the two sinks) and where notices are dropped

The gateway has two stream-consuming sinks. Both buffer assistant text into a
single `reply` string and both explicitly drop notices.

1. Push dispatch, `dispatch.rs`. The consumer loop is at `dispatch.rs:717-758`.
   - `MessageChunk` / `Commentary` accumulate into `reply` (`:719-725`).
   - `MessageStop { final_: true }` breaks the loop (`:726`); an intermediate
     stop appends a blank line (`:727`).
   - `ApprovalRequest` is delivered inline as its own message via
     `self.deliver(...)` while the loop is still running (`:728-751`). This is
     the existing precedent for turning a non-reply event into an immediate
     outbound message.
   - The drop site: `ToolCallChunk | ToolCallFinished | LongToolHint |
     GatewayNotice => {}` at `dispatch.rs:753-756`. Comment: "Tool chrome, hints
     and notices: not rendered in this pass."
   - The buffered `reply` is delivered once, after the loop, at
     `dispatch.rs:810` via `self.deliver(&msg, reply)`. It is suppressed for an
     empty or intentional-silence reply at `dispatch.rs:806-808`.

2. HTTP `/message`, `message.rs`. The consumer loop is at `message.rs:465-484`.
   - Same `MessageChunk` / `Commentary` / `MessageStop` accumulation
     (`:467-475`).
   - `ApprovalRequest` only logs a warning; a synchronous surface cannot host an
     interactive approval (`:476-478`).
   - The drop site: `ToolCallChunk | ToolCallFinished | LongToolHint |
     GatewayNotice => {}` at `message.rs:479-482`.
   - The buffered `reply` is both persisted to the transcript (`end_turn` at
     `message.rs:494-495`) and returned to the caller as the response body
     (`Ok(Json(MessageResponse { reply }))` at `message.rs:528`).

There is no separate Telegram, Discord, or Slack notice consumer, and no shared
notice-rendering module. `discord.rs`, `telegram.rs`, and `slack.rs` never see
`StreamEvent`; they only implement `PlatformAdapter::send`. The dispatcher is
the single place platform delivery is decided. A grep for `notice_kind` /
`GatewayNotice` across `crates/` returns only the one producer and the two drop
sites above.

## The only adapter method that can deliver a notice

The adapter surface is `PlatformAdapter` (`platform.rs:25-34`). Its only outbound
method is `async fn send(&self, msg: &Message) -> Result<()>`. There is no
ephemeral-message concept, no "notice" method, and no per-message styling flag.
Each concrete adapter just posts `msg.text` (for example Telegram at
`telegram.rs:265-292` posts `{chat_id, text}`).

So on the push side, a notice can only be delivered as an ordinary outbound
`Message` carrying the notice text. The dispatcher already wraps that in
`Dispatcher::deliver` (`dispatch.rs:186-258`), which resolves the adapter, skips
dead targets, records a delivery-ledger obligation, and calls `adapter.send`.
`deliver` is safe to call more than once per turn; the `ApprovalRequest` arm
already does exactly that mid-loop.

On the HTTP side there is no adapter at all. The handler runs the turn in a
spawned task and returns a single JSON body. There is no out-of-band channel to
push a separate notice message to the caller.

## Preserving order against chunks and stops

Events arrive on one `mpsc` channel, so arrival order is already the true order.
The current sinks lose that order for text because they coalesce all chunks into
one buffered reply delivered at the very end.

For push dispatch the correct seam is to deliver a notice inline the moment its
event is received, exactly like the `ApprovalRequest` arm. For main-provider
operator notices (waits, retry or fallback status, provider switch) this
produces the natural human ordering: the notice message lands first, then the
buffered answer lands when the turn finishes. That matches how the Python side
fires a status notice before the answer streams. A notice that arrives between
two message segments is delivered in its arrival position relative to those
inline-delivered events, which is the best ordering this buffered model can
express without also switching text to streaming delivery (out of scope here).

One caution carried over from the Python analysis: deliver notices as they are
classified, not eagerly on every attempt, or you reintroduce the retry-spam the
Python buffer lifecycle was built to suppress. The Rust side has no such buffer
yet, so the discipline lives in the producer: only emit a `GatewayNotice` at the
points Python actually surfaced one.

## Can the HTTP `/message` response represent notices?

The public response schema is `MessageResponse { reply: String }`
(`message.rs:61-64`), serialized as `{"reply": "..."}`.

Two ways a notice could reach an HTTP caller, and why neither is the narrow
seam:

- Folding the notice text into `reply`. This is wrong on two counts. First,
  `reply` is persisted to the transcript at `message.rs:494-495`, so folding a
  notice in would write it into durable history, which violates the
  presentation-only rule. Second, it muddies the returned assistant answer.
  Reject this.
- Adding an optional `notices` field to `MessageResponse`. This is a schema
  change. It can be made additive and non-breaking (a `Vec<String>` with
  `#[serde(skip_serializing_if = "Vec::is_empty")]` so existing responses are
  byte-identical and existing clients that ignore unknown keys are unaffected),
  but it is still new public surface and it only helps callers that opt in to
  reading it.

So the honest answer: the current `{reply}` schema cannot carry notices without
either contaminating the persisted reply (unacceptable) or adding a new field.
Because the HTTP surface is synchronous and append-only, dropping notices there
is a defensible parity choice, not a gap. The Python messaging gateway ran with
`notice_clear_callback = None` on the same kind of surface. Recommendation:
leave HTTP dropping notices for the narrow seam, keep a separate notices buffer
out of `reply` if HTTP ever needs them, and add the optional serialize-only
`notices` field only if a concrete HTTP consumer asks for it.

## Recommended narrow production seam

Deliver operator notices at the push-dispatch sink only.

1. Add a tiny pure helper, `operator_notices::render_notice(notice_kind, text,
   extra) -> Option<String>`. It returns the display string to send, or `None`
   to suppress a kind that should not surface on messaging platforms. This gives
   the gateway the "should I surface this here?" decision Python owned, keeps it
   testable in isolation, and keeps `native_agent.rs` transport-only. For the
   current `text`-carrying notices it is close to identity; the `Option` is the
   suppression hook.
2. In `dispatch.rs`, split `GatewayNotice` out of the drop arm at
   `dispatch.rs:753-756` into its own arm that calls `render_notice` and, on
   `Some(text)`, `self.deliver(&msg, text).await` immediately, mirroring the
   `ApprovalRequest` arm at `:728-751`. Leave `ToolCallChunk`,
   `ToolCallFinished`, and `LongToolHint` in the drop arm.
3. Leave `message.rs:479-482` dropping notices, per the schema analysis above.

This touches one match arm plus one pure module. It reuses `deliver` (dead-target
skip and ledger obligation come for free), it never writes to the transcript
(`end_turn` still sees only `reply`), and it never mutates the prompt. Notices
delivered this way are independent outbound messages, so the empty-reply
suppression at `dispatch.rs:806-808` does not swallow them: a turn that emits
only a notice and no answer still delivers the notice.

Explicitly out of scope and not recommended: any write to history from a notice,
any change to what the model receives, streaming text delivery, and a notice
buffer with drop-on-success or flush-on-failure lifecycle (that belongs to the
producer design, not the sink).

## Public tests (dispatch-level, using the existing stub harness)

The dispatch test module already has `StubAdapter` recording every outbound
`Message` (`dispatch.rs:1217-1234`) and a stub `AgentClient` whose `run_turn`
pushes events (`dispatch.rs` test agents, and the pattern at
`dispatch.rs:822-842`). Each test below uses an agent stub that emits a
`GatewayNotice` and asserts against `StubAdapter.sent`.

1. Notice is delivered and ordered before the answer. Agent emits
   `GatewayNotice { notice_kind: "wait", text: "..." }`, then a `MessageChunk`,
   then a terminal `MessageStop`. Assert `sent` has two messages: the notice
   text first, then the buffered reply.
2. A notice-only turn still delivers. Agent emits one `GatewayNotice` and a
   terminal `MessageStop` with no message text. Assert the notice is delivered
   even though `reply` is empty, proving the empty-reply suppression at
   `dispatch.rs:806-808` does not swallow notices.
3. Notices never reach the transcript. Drive a turn that emits a notice plus a
   real answer against a stub or in-memory db, then assert the persisted
   assistant turn (what `end_turn` receives) contains only the answer text and
   not the notice. Locks the presentation-only rule at the sink.
4. Notice ordering relative to an intermediate stop. Agent emits
   `MessageChunk`, `MessageStop { final_: false }`, `GatewayNotice`,
   `MessageChunk`, `MessageStop { final_: true }`. Assert the notice is
   delivered in its arrival position relative to the inline-delivered events, and
   the two text segments still coalesce into the final buffered reply.
5. HTTP reply stays clean (message.rs test module). Drive an HTTP turn whose
   agent emits a `GatewayNotice`, and assert `MessageResponse.reply` does not
   contain the notice text and the persisted transcript does not either. This
   pins the invariant that the synchronous surface never leaks a notice into the
   returned or stored reply, and it stays valid whether or not an optional
   `notices` field is added later.

## Notes on faithfulness

The word "notice" appears in several other gateway files (`session_stall.rs`,
`config_schema.rs`, and others), but those are unrelated uses; none consume
`StreamEvent::GatewayNotice`. The two sinks and the one `memory_recall` producer
are the complete current surface.
