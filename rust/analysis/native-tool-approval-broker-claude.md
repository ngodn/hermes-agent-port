# Native tool approval broker (Rust)

## Primary integration disposition

The helper draft was a useful isolated state-machine starting point, but this
document describes that draft rather than the final integrated surface. The
primary lane added command metadata, all four decision scopes, deny reasons,
batch replies, asynchronous prompt publication, shared gateway ownership,
pre-lease authorization, session-boundary cancellation, and shutdown cleanup.
The existing pre-lease control path means no lease-suspension mechanism was
needed. Unused exact-ID button and reconnect snapshot APIs were removed because
this checkpoint has no production consumer for them.

New module: `rust/crates/hermes-gateway/src/tool_approval.rs`. Self-contained,
rustfmt clean, no unsafe, no lint allowances, not wired or committed. 13 in-file
async tests, all green in a standalone crate. Nothing else was touched.

This is the deep in-process approval broker the prior wiring audit
(`native-approval-wiring-claude.md`) identified as the missing piece: a
route-keyed pending store with a real suspend primitive. It does not attempt to
solve the transport blockers B1 to B4 from that audit. It is the control-plane
state machine those transports will call once a suspend-without-holding-the-lease
seam exists. The primary owns wiring it to `Tool::call`, the turn task model,
and any adapter buttons.

## What it is

One session-owned type, `ApprovalBroker`, hiding the pending set, per-route FIFO
queues, the global id index, timeout reclaim, and exactly-once resolution behind
a small surface. Build one per session or per control-plane life and share it
through an `Arc`. It has no knowledge of the model transcript, the prompt, or
tool arguments. Callers pass only opaque routing metadata.

## Interface

Request side (the tool that needs a human decision):

- `request(RequestSpec) -> Result<Outcome, SubmitError>` async. Registers a
  pending request on `spec.route_key` and awaits a decision up to the per-request
  or default timeout. `Outcome` is `Decided(Decision)`, `TimedOut`, or
  `Cancelled`. `SubmitError::Overloaded` is returned without waiting when the
  route or the process is saturated.
- `RequestSpec { route_key, principal, action, timeout }` with a `new(...)`
  helper that inherits the default timeout. `principal` is the opaque owner the
  reply sender must match. `action` is an opaque display label, for example a
  tool name.

Reply side (ingress that carries a human decision):

- `resolve_text(route_key, text, allowed) -> ResolveOutcome`. Parses `text` to a
  decision and resolves the oldest pending request on the route (FIFO). Returns
  `Resolved { id, decision }`, `NoPending`, `Malformed`, or `Unauthorized`.
- `resolve_by_id(id, decision, allowed) -> ResolveOutcome`. Resolves an exact
  request by its global id, for an adapter button that already carries a
  decision.
- `allowed: impl FnOnce(&RequestInfo) -> bool` is the authorized-sender
  predicate, supplied at resolution. It receives the request metadata and gates
  the reply. A false result returns `Unauthorized` and consumes nothing.

Snapshot side (reconnect):

- `list() -> Vec<RequestInfo>`, `route_snapshot(route_key) -> Vec<RequestInfo>`,
  `get(id) -> Option<RequestInfo>`, `ack(id) -> bool`. `ack` marks a request as
  delivered so a reconnecting adapter can re-render only what has not been
  prompted yet. `RequestInfo` carries `id`, `route_key`, `principal`, `action`,
  `delivered`, and `age`, all opaque.

Lifecycle side (boundaries):

- `cancel_route(route_key) -> usize` cancels every pending request on one route,
  for a session boundary, so an approval that predates the new conversation on
  the same stable route cannot resolve into it.
- `shutdown() -> usize` cancels everything, for run shutdown.

Enums: `Decision::{AllowOnce, AllowAlways, Deny}`, `Outcome::{Decided, TimedOut,
Cancelled}`, `SubmitError::Overloaded`, `ResolveOutcome::{Resolved, NoPending,
Malformed, Unauthorized}`. Config: `BrokerConfig { default_timeout,
max_pending_per_route, max_total_pending }` with `Default`.

## Design calls worth flagging

- **Route ownership is stable, not transcript-bound.** Pending state is keyed by
  the gateway route the same way `slash_confirm.rs` keys its confirmations.
  A session reset that inherits the same route calls `cancel_route`, which is why
  a stale approval never lands in a new conversation.

- **No std mutex across an await.** Every map mutation is a short critical
  section under `std::sync::Mutex`. The only await points in `request` are the
  per-request `oneshot` receiver and the `tokio::time::sleep` timer, both taken
  after the lock is dropped. Resolution sends the decision on the oneshot after
  dropping the lock. `tokio::sync::Mutex` was deliberately not used because no
  lock needs to be held across an await.

- **Exactly once by remove-then-send.** Whichever caller removes the `Pending`
  from its queue under the lock owns the outcome and holds the only responder
  sender. Concurrent resolvers race for that removal; the losers see the entry
  gone and report `NoPending`. The reply-versus-timeout race is handled with a
  `select!` over the receiver and the timer: on timer fire the waiter calls
  `claim_timeout`, and only if it actually removes the entry does it report
  `TimedOut`. If a resolver already removed it, the decision is in flight, so the
  waiter awaits the receiver instead and reports `Decided`. This is why a reply
  that lands the same instant a request expires can never be both resolved for
  the sender and timed out for the waiter. The dedicated race test loops this 50
  times.

- **Cancellation via sender drop.** `cancel_route` and `shutdown` remove entries
  and drop their responder senders. Each waiter's receiver then errors and maps
  to `Cancelled`. No explicit signal value is needed, and there is no way for a
  cancelled request to also carry a decision.

- **Non-consuming unauthorized and malformed.** `resolve_text` checks in order:
  is anything pending (`NoPending`), does the text parse (`Malformed`), is the
  sender authorized (`Unauthorized`), and only then removes and resolves. The
  first three leave the prompt live so an unrelated message or a message from
  someone who may not approve falls through to normal handling. This mirrors the
  precedence discipline in `slash_confirm.rs::resolve_text`.

- **Bounded and fail closed.** `max_pending_per_route` (default 16) and
  `max_total_pending` (default 1024) cap growth. Overload returns
  `SubmitError::Overloaded` immediately rather than queueing, so the caller
  denies the action instead of blocking a tool behind an unbounded backlog. The
  overload test proves both the per-route and the global cap.

- **Globally unique opaque ids.** A process-global `AtomicU64` sequence
  guarantees uniqueness across every broker instance in the process, including a
  broker built after a session reset, so an id is safe to route on. A random
  per-broker salt is mixed in so ids are not a guessable running counter. The
  salt is derived from a randomly seeded `RandomState` hasher, so no rng
  dependency is added. Ids render as `apr_<16 hex salt><12 hex sequence>`.

## Authorization model

The predicate is supplied at resolution, not stored, so the broker never needs
to know the authorization policy. The requester puts an opaque `principal` on the
`RequestSpec`; the ingress path captures the reply sender's identity in the
closure and compares, for example `|info| info.principal == reply_sender`. The
predicate runs while the internal lock is held, matching `slash_confirm.rs`, so
it must be cheap and must not call back into the broker (documented on the
methods). This keeps policy in the caller and state in the broker.

## Assumptions

- One text reply on a route resolves the oldest pending request on that route.
  Multiple pending requests per route are supported and are strictly FIFO; a
  button path (`resolve_by_id`) can still target any one of them out of order.
- The caller treats both `TimedOut` and `Cancelled` as denials. The broker does
  not itself run any action; it only reports the outcome.
- `AllowAlways` is surfaced but not persisted here. Persisting an opt-out (the
  `slash_confirm.rs` config write) is the caller's job, deliberately kept out of
  this control-plane primitive.
- No adapter traits were invented. The interface is concrete methods a real
  ingress path calls directly. The prompt text, the button payload format, and
  the transport are all outside this module.

## Tests

13 in-file async tests (`tokio::test`), covering: FIFO ordering of multiple
pending per route, oldest-first text resolution, exact-id resolution out of
order, timeout, stale-id after resolution, non-consuming unauthorized and
malformed replies, route-scoped cancellation and global shutdown waking every
waiter, snapshot list/get/ack across a reconnect, fail-closed overload at both
the per-route and global caps, an eight-thread concurrent-resolve race proving
exactly-once, a 50-iteration reply-versus-timeout race, and decision parsing.
The concurrency race test runs on a multi-thread runtime.

## Known gaps

- **Test command** `cargo test -p hermes-gateway tool_approval` only resolves
  once the primary adds `mod tool_approval;` to `main.rs`. Verified standalone
  until then (13 passed).
- **dead_code warnings** will appear when first wired into the binary, the same
  reason sibling modules carry `#![allow(dead_code)]`. I did not add that allow
  because the task forbids broad lint allowances; the primary owns the lint
  decision at wiring time.
- **Suspend integration is out of scope.** This module is the pending-state and
  suspend-primitive half. Making `Tool::call` await a broker request without
  pinning the turn lease, and giving ingress a callback path, are the transport
  changes tracked in `native-approval-wiring-claude.md`, not done here.
