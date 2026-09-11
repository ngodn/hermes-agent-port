# Native main-provider fallback notices

## Outcome

Native chat-completions conversations now surface the durable provider changes
that Python reports to operators. A successful switch emits the exact
model/provider fallback sentence once. If the entire fallback chain fails, the
same switch is still emitted once before the turn error. When a later admitted
turn restores the primary route, it emits Python's exact primary-restored
sentence once.

The notices are presentation events, not model content. Push dispatch delivers
each nonempty `GatewayNotice` through the selected platform adapter before the
buffered assistant reply. The synchronous HTTP `/message` contract continues
to return only `MessageResponse { reply }`, so it intentionally drops notices
instead of mixing them into the reply that is also persisted.

## Runtime integration

`main_provider_notices.rs` owns the small turn-local lifecycle. Every fallback
switch is retained in both the terminal trace and the recovery-only durable
list. Recovery drops the terminal copy and emits the durable copy. Terminal
failure drops the duplicate durable copy and emits the trace copy. This keeps
multiple switches ordered and makes every drain idempotent.

`NativeAgentClient` records switches only at the two existing route activation
seams, covering pre-body failures and successful-body refusal or invalid
response fallback. Reason labels are mapped to Python's live
`_fallback_reason_text` values. Primary restoration records the prior serving
route after the reset gate commits.

Automatic compression can restore the primary during preflight, before the
ordinary model turn receives an event sender. The conversation client retains
that pending notice, and `run_native_turn` atomically transfers it into a fresh
turn-local buffer before doing its own restore check. The final stream stop is
held until model completion so recovered notices can be sent before push
dispatch releases the already-buffered answer. Message chunks continue to
flow into the sink during generation.

No notice changes the system prompt, tool schema, provider request, fallback
route snapshot, durable assistant reply, or SQLite transcript. Surface send
failures remain best effort and cannot undo provider recovery.

## Behavioral proof

The 37-case AGY corpus executes the live Python `AIAgent` buffer methods and
`try_activate_fallback`. It covers exact fallback text and reason labels,
ordered multi-switch recovery, terminal duplicate suppression, callback
failure, and idempotent buffer cleanup. The primary lane independently checked
the generated output, fixed its formatting, and bound Rust tests to the exact
corpus fields used by production.

Public Rust tests exercise:

- primary HTTP failure followed by a successful frozen fallback
- exhaustion of both primary and fallback with exactly one switch notice
- a second turn whose compression preflight restores the primary before a
  successful provider request
- push ordering, notice-only delivery, and SQLite exclusion
- HTTP reply and SQLite exclusion without changing the public response schema
- exact Python reason-label and multi-switch text parity

The implementation was developed test-first. The initial push test delivered
only the assistant answer, and the initial native fallback test emitted no
notice. Both passed after the production event and sink paths were connected.
The focused Python retry-buffer and primary-restore suites also exercise the
live reference lifecycle.

AGY owned the source-executed Python contract lane. Claude first mapped the two
Rust delivery sinks independently, then reviewed the completed implementation
for state ownership, ordering, and prompt/transcript isolation. The helper
lanes did not edit the same production files.

Claude's [review](main-provider-fallback-notice-review-claude.md) found no live
cache, transcript, mutex, delivery-only, or sink regression. It questioned
whether restoration needed a provider-name guard,
but live Python shows `_provider_fallback_active` marks provenance from
`try_activate_fallback`, including same-provider routes. Rust's nonzero active
index has the same provenance and is never used by temporary model overrides,
so adding a provider-name comparison would diverge from Python. The review also
confirmed that the two notice vectors are intentionally identical until the
separate transient retry-trace work populates only the terminal buffer.

## Scope boundary

This checkpoint ports durable fallback-switch and primary-restoration notices
for ordinary native chat-completions routes. It does not yet port Python's full
transient retry, countdown, pre-switch, or terminal diagnostic trace. The push
dispatcher now also delivers the existing external-memory recall notice because
all nonempty `GatewayNotice` events share the same presentation contract.

HTTP stays reply-only. Adding a separate notices array later would be an
explicit public API change; folding notice text into `reply` would violate the
transcript and prompt-cache invariants.
