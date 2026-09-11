# Review: main-provider fallback-switch and primary-restore operator notices (Rust port)

Scope: the uncommitted work that adds turn-local operator notices for ordinary
main-provider recovery. Files under review:

- `rust/crates/hermes-gateway/src/main_provider_notices.rs` (new)
- `rust/crates/hermes-gateway/src/native_agent.rs` (notice recording, drain, and
  the run-turn forwarding change)
- `rust/crates/hermes-gateway/src/dispatch.rs` (push sink delivery)
- `rust/crates/hermes-gateway/src/message.rs` (HTTP sink)

Python compared against (live source):

- `agent/chat_completion_helpers.py:2669` `_fallback_reason_text`,
  `:3152` the switch notice and its dual buffer.
- `agent/agent_runtime_helpers.py:1641` `restore_primary_runtime`,
  `:1743` and `:1957` the restore notice and its gate.
- `run_agent.py:1285` `_emit_pending_fallback_notice`,
  `:1315` `_flush_status_buffer`.

Note on typography: this document avoids literal em dash characters, including
inside quotations. Where the source uses one I have rewritten with a comma or
parentheses and said so.

## Executive summary

The port is structurally faithful and, on the common single-conversation path,
correct. Notices never touch the transcript, the durable history, or the prompt
cache scope, so presentation-only isolation holds. Poisoned-mutex handling is
consistent and no `std::sync::Mutex` is held across an `.await`. The delayed
final `MessageStop` is safe for both current sinks.

The findings below are ranked by severity. Two are genuine behavioral
divergences from Python (F1, F2). The rest are latent or robustness concerns
that are not live bugs on today's call paths, plus several suspected issues that
I checked and found to be non-issues (listed explicitly at the end).

---

## F1. Medium: the two-vector lifecycle is inert, so terminal failure and success emit the identical set, and the `recovered` flag has no observable effect

`main_provider_notices.rs:16` `record_fallback` and `:34` `record_primary_restore`
each push an identical `Notice` into BOTH `buffered` and `pending_durable`
(`:30-31`, `:47-48`). `drain` (`:54`) returns one vector and clears the other
depending on `recovered`. Because the two vectors always hold the same content,
`drain(true)` and `drain(false)` produce the same output. The `recovered`
argument computed at `native_agent.rs:5294`
(`outcome.is_ok() && !delivery_only`) therefore changes nothing observable
today, and neither does the `delivery_only` term.

Why this matters against Python. The split mirrors Python exactly:
`pending_durable` is `_pending_fallback_notice` (the durable one-shot switch
list, `chat_completion_helpers.py:3160-3166`) and `buffered` is
`_retry_status_buffer` (the full trace, seeded by `_buffer_status` at
`:3159`). In Python the two diverge because `_buffer_status` also collects the
verbose retry chatter (attempt counts, wait timers, provider error bodies, see
`conversation_loop.py:3915-3919`, `:5780-5789`). On success Python emits only
the clean one-shot list (`run_agent.py:1285` `_emit_pending_fallback_notice`);
on terminal failure it flushes the full trace and nulls the pending list
(`run_agent.py:1315-1325`), so the switch shows exactly once either way.

The Rust port records only the switch and restore lines into both vectors and
never records the transient chatter, so the terminal-failure branch shows just
the switch line(s) rather than Python's full trace. This is a fidelity gap, not
a crash: the switch still shows exactly once, and the prior sink analysis
already argued that verbose retry chatter is a live-surface luxury that does not
belong on the append-only messaging gateway. So the reduced terminal trace is
defensible. The concrete finding is narrower and worth calling out for the next
maintainer: as written, the `buffered` versus `pending_durable` distinction and
the `recovered`/`delivery_only` computation are dead code. If someone later
starts recording chatter into `buffered` only (to match Python), the existing
drain logic is already correct, but until then the design reads as if it does
something it does not.

No action strictly required for correctness. If the intent is to keep the
skeleton for a future chatter port, a one-line comment on `drain` saying the two
vectors are currently identical would prevent a maintainer from "fixing" the
seemingly redundant double push.

## F2. Medium: primary-restore notice is emitted without Python's cross-provider gate

`restore_primary_route_for_turn` (`native_agent.rs:2389`) records a
`primary_restore` notice whenever the active index was non-zero and a previous
route resolves (`:2450-2460`). It is gated only on "a fallback route was active"
(the early return at `:2396` when `state.active == 0`).

Python gates the same notice more tightly. `restore_primary_runtime` emits
`"Primary model restored ..."` only inside `if provider_fallback_active:`
(`agent_runtime_helpers.py:1957-1962`), where `provider_fallback_active` is
`_provider_fallback_active` (`:1752`), a flag set true only by
`try_activate_fallback` on a real provider fallback
(`chat_completion_helpers.py:3170`). The comment at `agent_runtime_helpers.py:1935`
explains the distinction: `_fallback_activated` is reused by temporary
`/model --once` restoration, so the restore path must not emit a recovery notice
for that case.

Whether this is a live divergence depends on whether every Rust main-fallback
route corresponds to what Python calls a provider fallback. The Rust gateway
has no `/model --once` concept feeding `main_fallback.state.active`, and the
configured fallback routes are alternate full routes, so in practice
`active != 0` should coincide with Python's `provider_fallback_active`. On that
basis this is most likely benign. I am flagging it as Medium rather than
dismissing it because the port drops an explicit guard that Python documents as
load-bearing, and the equivalence rests on an external assumption
(no same-identity fallback route is ever configured) that the notice code does
not itself enforce. If a same-provider or same-identity fallback route is ever
added, Rust will emit a spurious "Primary model restored" line that Python
suppresses. A `provider_name != previous.provider_name` (or model identity)
check before recording would make the port match Python's intent explicitly.

## F3. Low/Medium: the cross-call primary-restore hop through the shared client buffer relies on per-conversation turn serialization

The restore notice is recorded during compression preflight, which runs on the
memoized per-conversation client `self` (`compression_preflight` at
`native_agent.rs:5502` calls `restore_primary_route_for_turn`, which records
into `self.main_notices` at `:2451`). The subsequent `run_native_turn` takes the
whole buffer out of `self.main_notices` at the very top of the turn
(`:5166-5173`, `std::mem::take`) into a fresh per-turn Arc. The test
`next_turn_primary_restore_emits_exact_notice_before_reply` exercises exactly
this hand-off and passes.

This is correct as long as preflight and its run happen sequentially for a given
conversation and no second turn for the same conversation overlaps them. The
notices client is per conversation (created once by the `ConversationAgent`
factory via `checkout` / `get_or_try_init`, `conversation_agent.rs:744-758`), so
there is no cross-conversation leak. The remaining exposure is intra-conversation
concurrency: `std::mem::take` drains the shared buffer wholesale with no turn or
session key, so if two turns for the same conversation were ever in flight at
once, one turn's take could carry off the other turn's preflight-recorded restore
notice (misattributing it, and losing it from the intended turn). Gateway turns
for a single conversation are serialized by the transcript lease, so this is
almost certainly not reachable today. It is worth a note because the safety is an
external invariant, not a property of the notice buffer itself. Keying the buffer
by session id, or recording the restore on the per-turn client instead of the
shared client, would remove the dependency.

## F4. Low: `record_main_fallback_notice` fires before the state check and without an idempotency guard

In `activate_main_fallback` (`native_agent.rs:2524`) the notice is recorded at
`:2531`, before the state lock is taken at `:2535` and before `state.active` is
set at `:2547`. `activate_main_success_body_fallback` does the same at `:2572`.
Neither re-checks that `state.active` still equals `failed_index`, unlike
`restore_primary_route_for_turn`, which re-locks and bails if the state moved
(`:2438-2444`). `next_main_fallback_index` computes `failed_index + 1` and does
not consult `state.active`, so the notice is recorded purely from the caller's
`failed_index`.

On the normal path each route fails at most once per turn and the caller passes
the currently-serving index, so exactly one notice is recorded per switch (both
the recovered and terminal tests confirm a single notice). The concern is
robustness: if the retry machinery ever re-invokes `activate_main_fallback` with
a stale `failed_index` (or two attempts race), the notice would be recorded again
for the same logical switch and `state.active` would be rewritten
unconditionally. Recording inside the same lock section that verifies
`state.active == failed_index` before advancing would make the notice as
idempotent as the restore path. Low severity given current single-future turns.

## F5. Low: the "notice before reply" ordering is an emergent property of sink buffering, not of the event stream

At the event-stream level the notice is emitted AFTER the model chunks and
before the re-emitted final stop. `run_native_turn` forwards all chunks, drops
the final `MessageStop` (`native_agent.rs:5278-5281`), then after the join sends
the notices (`:5295-5303`) and finally re-emits the stop (`:5304-5306`). The
test `recovered_main_fallback_emits_one_switch_notice_before_the_reply` asserts
the order chunk, notice, stop, so the notice is literally after the reply text on
the wire despite the test name.

It reaches the user before the reply only because both sinks buffer the reply
and flush it on the terminal stop while delivering the notice inline the moment
it arrives: the push dispatcher accumulates chunks into `reply` and delivers the
notice via `self.deliver` at `dispatch.rs:752-753`, then breaks on the final stop
at `:726` and delivers the buffered reply at `:813`. The HTTP handler buffers
identically (`message.rs:464-484`). This is fine for both current sinks. It is
fragile for any future consumer that streams `MessageChunk` to the user live:
such a consumer would show the notice after the streamed reply, inverting the
intended order. Worth a comment near the emit loop that the ordering guarantee
depends on downstream buffering.

## F6. Low: collapsing the final `MessageStop` assumes exactly one terminal stop per turn

The forward loop suppresses every `MessageStop { final_: true }` and re-emits a
single one after the join (`native_agent.rs:5278-5306`). If `run_model_turn`
ever emits more than one final stop, the port now collapses them to one
(previously each was forwarded). Any event emitted after a final stop is now
forwarded before the re-emitted stop, a minor reordering. In practice a turn
emits one terminal stop and memory-recall notices are emitted at turn start
(`:5214`), not trailing, so I could not construct a reachable regression. Calling
it out only so the single-terminal-stop assumption is on record. On the terminal
failure path `final_stop` stays false and no stop is synthesized, which matches
the pre-change behavior and is confirmed by
`terminal_main_fallback_failure_flushes_one_switch_notice` (events are the notice
alone, no stop).

## F7. Low: Rust notice reasons are coarser than Python's failover reasons

`MainPoolFailure::notice_reason` (`native_agent.rs:996`) and
`MainSuccessBodyFailure::notice_reason` (`:1018`) cover about eleven labels. The
labels they do produce match Python `_fallback_reason_text`
(`chat_completion_helpers.py:2669`) exactly for the overlapping buckets, and the
golden test `fallback_notice_reason_labels_match_the_python_corpus` pins that.
But Python's `FailoverReason` enum carries roughly two dozen labels (for example
"TLS certificate verification failed", "context window exceeded",
"model not found", "provider policy blocked the request",
"encrypted reasoning state rejected"). Those Python reasons collapse into the
coarser Rust `MainPoolFailure` buckets (Transport, ServerError, Unrelated, and
so on), so the Rust operator notice will read a less specific reason than Python
for those failures. This is a pre-existing classifier-granularity difference, not
something this diff introduced, and it is not a correctness bug for the notice
code itself. Noted for completeness since the task asks about reason fidelity.

---

## Suspected issues that are NOT real

- Double emission of a switch or restore notice. `drain` moves one vector out
  with `std::mem::take` and clears the other (`main_provider_notices.rs:54-62`),
  so each recorded notice is emitted at most once, and the per-turn buffer is a
  fresh Arc each turn (`native_agent.rs:5173`). The preflight restore plus the
  in-run restore cannot both fire: the in-run `restore_primary_route_for_turn`
  sees `state.active == 0` (already restored by preflight, shared state via the
  cloned Arc) and returns at `:2396` without recording. Verified: no duplication.

- Transcript or history pollution. Notices are delivered only as `GatewayNotice`
  stream events. The push sink delivers them via `self.deliver` and never appends
  them to `reply` (`dispatch.rs:752-753`), and only `reply` is persisted at
  `end_turn` (`:782-783`). The HTTP sink drops notices entirely
  (`message.rs:479-482`) and returns only the buffered `reply`. Both delivery
  tests confirm history contains only the user turn and the model answer. Not an
  issue.

- Prompt-cache contamination. Notices are never added to the durable message
  list or to `cache_scope`; `cache_scope` is derived from the compression lineage
  or session id (`native_agent.rs:5192-5198`) independent of notices. Not an
  issue.

- Poisoned-mutex panics. Every `main_notices` and `main_fallback.state` lock in
  the diff uses `.unwrap_or_else(|error| error.into_inner())` (for example
  `:2451`, `:2539`, `:5167`, `:5292`), so a poisoned lock recovers rather than
  cascades. Not an issue.

- Mutex held across `.await`, risking a stall. The take (`:5166-5172`) and the
  drain (`:5290-5294`) both release the `std::sync::Mutex` guard before the
  awaited sends: the take is scoped in a block, and the drain returns an owned
  `Vec` so the guard temporary is dropped at the statement's semicolon before the
  `for` loop awaits. Not an issue.

- Delivery-only replies swallowing the notice. Because both vectors are identical
  (see F1), the `recovered = outcome.is_ok() && !delivery_only` selection at
  `:5294` cannot drop a notice: the delivery-only or error branch returns
  `buffered`, which holds the same switch and restore lines. Notices are always
  emitted when present regardless of success, failure, or delivery-only status,
  which is the correct "show the switch once" behavior. Not an issue.

- Notice-only turn being suppressed by the empty-reply gate. The notice is a
  separate outbound message delivered inline (`dispatch.rs:752-753`), so it
  survives the empty-reply return at `:809`. `notice_only_turn_is_still_delivered`
  confirms it. Not an issue.

- Concurrent turns across conversations leaking notices. The notices buffer lives
  on the per-conversation `NativeAgentClient` created once by the
  `ConversationAgent` factory (`conversation_agent.rs:744-758`), so distinct
  conversations have distinct buffers. The only residual concurrency concern is
  intra-conversation, covered by F3. Not an issue at the cross-conversation level.

## Net assessment

Ship-ready for the current two sinks and the serialized per-conversation turn
model. F2 is the one I would resolve before merge, since it drops a guard Python
documents as intentional and the equivalence depends on configuration staying
cross-provider. F1 and F3 through F7 are latent or informational and can be
addressed with comments or small guards without reworking the design.
