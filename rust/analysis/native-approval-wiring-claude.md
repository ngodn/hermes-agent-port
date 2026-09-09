# Native interactive terminal approval: Rust gateway seam audit

Read-only architecture audit of the Rust gateway (`rust/crates/hermes-gateway`)
for the seams a native interactive terminal-approval checkpoint would have to
cross. Scope is the transport boundary only. The static `approvals.deny`
matching contract and its interaction with `approvals.mode: off` are owned by
the Python deny-rule lane (`native-approval-deny-contract-agy.md`) and are not
restated here. Every Rust fact was read from the working tree on the
`rust-rewrite` branch. Every Python fact cited is only the minimum needed to
locate where the two designs diverge. No production code, tests, oracles,
`PORT.md`, or `INDEX.md` were touched.

## Headline

Interactive terminal approval cannot be landed safely in the Rust gateway as it
stands today, and it should follow the static deny-rule checkpoint rather than
precede it. The reason is structural, not cosmetic: Python's blocking approval
works because the tool runs on a worker thread while the asyncio loop stays free
to receive the `/approve` reply, whereas the Rust tool loop runs the tool inside
the same async turn task that holds the per-session turn lease. Blocking a Rust
tool call to wait for a human reply pins the turn lease, and the reply turn that
would release it fails closed after the lease wait budget. There is also no seam
for a native tool to emit a prompt outward mid-turn or to receive a reply back
into the running turn. The existing destructive slash-confirm primitive
(`slash_confirm.rs`) already builds the pending-state, ownership-key,
exactly-once, timeout, and future-button machinery this checkpoint needs, but it
resolves on a fresh ingress turn, not inside a blocked one, so it does not by
itself close the transport gap.

Recommendation: keep the native terminal fenced behind
`native_local_terminal_eligible` (`main.rs:241-256`), land the static deny-rule
checkpoint next, and defer interactive approval until the tool-execution seam can
suspend a tool call without holding the turn lease. Section 6 gives the smallest
truthful interactive design for when that prerequisite is met.

---

## 1. Native tool invocation: the seam that would host the checkpoint

- The whole tool surface is `native_tools::Tool` (`native_tools.rs:39-43`):
  `fn spec(&self) -> ToolSpec` and `async fn call(&self, args: &Value) ->
  Result<Value>`. `call` receives only the decoded arguments. It has no session
  key, no route key, no event sender, no platform handle, and no cancellation
  token. Any context must be captured when the tool object is constructed, which
  is how `TerminalTool` already captures its session-scoped runtime
  (`native_terminal.rs`, constructed at `main.rs:845-852`).
- The tool loop drives `call` synchronously in line:
  `run_tool_loop_with_messages` (`native_tools.rs:546`) reaches
  `tool.call(&call.arguments).await` at `native_tools.rs:759`, mapping
  `Ok(v) -> (v, true)` and `Err(e) -> ("tool error: {e}", false)`
  (`native_tools.rs:759-762`). Around that call it emits `ToolCallChunk` before
  (`native_tools.rs:728-739`) and `ToolCallFinished` after
  (`native_tools.rs:765-772`). Those are the only two tool-lifecycle events; the
  channel carries facts outward, never a request that expects a reply.
- Consequence: for a tool to ask for approval mid-call it would have to (a)
  reach an outbound platform prompt and (b) await an inbound resolution, and the
  `Tool` trait exposes neither. This is the first hard gap, and it is a signature
  and wiring gap, not a policy gap.

## 2. Stream events: outbound only, buffered, no request channel

- `StreamEvent` (`hermes-core/src/stream.rs:25-65`) has `MessageChunk`,
  `MessageStop`, `Commentary`, `ToolCallChunk`, `ToolCallFinished`, plus
  `LongToolHint` and `GatewayNotice` (matched at `dispatch.rs:683-686`). There is
  no approval-request variant and no reverse channel. The enum is a one-way
  producer stream from the agent turn to the delivery loop.
- The push delivery loop buffers the entire turn into one string and only
  delivers at the terminal stop: `dispatch.rs:670-688` accumulates
  `MessageChunk` and `Commentary` into `reply`, treats tool chrome as
  unrendered, and breaks on `MessageStop { final_: true }`. Persist and deliver
  happen after the loop (`dispatch.rs:702-708`). So even the presentation layer
  has no notion of an interim outbound message that pauses the turn and expects
  an answer.
- The channel itself is bounded, `mpsc::channel::<StreamEvent>(64)`
  (`dispatch.rs:637`), and is drained by a different task than the one producing
  it (section 3). It is a display pipe, not a control pipe.

## 3. Pending route state, turn leases, and the interruption model

- A push turn acquires the per-session turn lease with no explicit timeout:
  `self.lease.acquire(&session_id, &msg.sender_id, generation, None)`
  (`dispatch.rs:576-587`). `None` resolves to `DEFAULT_LEASE_WAIT = 5s`
  (`turn_lease.rs:36`, applied `turn_lease.rs:129-132`) and the acquire fails
  closed on timeout (`turn_lease.rs:149-160`): the second turn is rejected, not
  run unserialized. The lease is keyed by resolved `session_id`
  (`turn_lease.rs:90-96`).
- The turn then runs on a spawned task, `agent_task`
  (`dispatch.rs:650-662`), which calls `run_turn_with_context` and streams into
  `tx`. The tool loop, and therefore `tool.call().await`, runs inside that task,
  under the held lease. The lease token is moved into the admitted-turn ownership
  (`dispatch.rs:604-609`, `AdmittedTurnOwnership.transcript_lease`) and lives for
  the whole turn.
- Interruption is coarse. The only cancellation path is aborting the ingress
  waiter, and the design explicitly detaches the admitted turn so it survives
  that abort (`dispatch.rs:594-597`, and the test at `dispatch.rs:744-794`).
  There is no per-turn cancellation token threaded into the tool loop and no
  in-turn "a newer message arrived, interrupt the tool" path. A turn runs to its
  terminal stop or its own error.
- Net effect on interactive approval: if a Rust tool call blocked to await a
  human reply, it would hold the `session_id` turn lease for the whole approval
  window. The `/approve` reply is itself a new inbound `Message` that flows
  through the same `dispatch` -> `acquire(&session_id, ...)` path
  (`dispatch.rs:576-587`) and would fail closed after 5s because the blocked
  turn still holds the lease. The reply that is supposed to release the wait
  cannot be admitted. This is the core blocker, and it is a property of the
  single-task, lease-per-session execution model.

## 4. Platform delivery and the HTTP path

- `PlatformAdapter` (`platform.rs:25-34`) is `name` / `run` / `send(&Message)`.
  There is no inline-keyboard or reply-markup send, no callback ingress, and no
  answer-callback method. Buttons for an approval prompt are as greenfield here
  as the destructive-confirm audit found them to be
  (`destructive-slash-confirm-map-claude.md`, section 4). A text-fallback
  approval prompt could be delivered through `send`, but only as an ordinary
  message, and only at a turn boundary given section 2.
- The HTTP ingress is strictly one request to one response:
  `handle` returns a single `Json<MessageResponse>` whose only field is `reply`
  (`message.rs:62-63`, `message.rs:83`, examples at `message.rs:129-131`). A
  synchronous request cannot carry an interactive round trip inside one turn; an
  HTTP client would have to poll or reconnect on a separate request, which is
  exactly the replay surface Python exposes (section 5) and which does not exist
  in Rust yet.

## 5. The Python transport boundary (only what locates the divergence)

Python's blocking gateway approval, `tools/approval.py`:

- A pending approval is an `_ApprovalEntry` holding a `threading.Event`
  (`approval.py:2820-2833`), queued per session in `_gateway_queues`
  (`approval.py:2837`). The tool worker appends its entry
  (`approval.py:4606`) and then blocks on a polling `entry.event.wait(timeout=
  min(1.0, _remaining))` loop (`approval.py:4704`, leader variant at
  `approval.py:4515`).
- The blocking tool runs on a `DaemonThreadPoolExecutor` worker thread
  (`agent/tool_executor.py:875-876`), not on the event loop. That is the whole
  reason the design is safe: the worker thread blocks on the `Event`, the asyncio
  loop stays free, and the loop keeps receiving the `/approve` reply that
  resolves it. The sync/async split is the transport boundary.
- Outbound request: `register_gateway_notify(session_key, cb)`
  (`approval.py:2840-2850`) registers a per-session callback that bridges
  sync to async and schedules the actual prompt send on the loop. Inbound
  resolution: `resolve_gateway_approval(session_key, choice, ...)`
  (`approval.py:2865-2906`) is called by the gateway's `/approve` or `/deny`
  handler, sets `entry.result` and calls `entry.event.set()`, unblocking the
  worker. FIFO by default, `resolve_all` for `/approve all`, and a `request_id`
  targeted form.
- Concurrency, replay, disconnect, and batch-deadline handling all already
  exist: a per-session FIFO queue supports several concurrently blocked workers
  (parallel subagents); `list_gateway_approvals` (`approval.py:2909`),
  `get_pending_gateway_approval` (`approval.py:2934-2943`), and
  `ack_gateway_approval` (`approval.py:2914-2921`) let a reconnecting client
  restore a prompt whose notification was sent while it was detached;
  `unregister_gateway_notify` (`approval.py:2852-2863`) sets every pending
  event on run end or interrupt so no worker hangs forever; and the human-wait
  accounting (`_HumanWaitState`, `approval.py:2599-2609`) subtracts verified
  human-wait time from the concurrent tool-batch deadline so a slow answer does
  not time out a batch.
- `has_blocking_approval(session_key)` (`approval.py:2923-2926`) is the
  precedence predicate that lets a live tool approval take `/approve` before the
  slash-confirm intercept sees it.

The single divergence that matters: Python suspends a tool on a thread while an
async loop keeps serving ingress. Rust runs the tool inside the async turn task
under a per-session lease, so the same suspend starves the reply. Everything else
(pending state, ownership key, exactly-once, timeout, replay) Python has and Rust
can copy; the suspend-without-holding-the-lease primitive is the one thing Rust
does not have.

## 6. Smallest truthful interactive approval checkpoint (for when unblocked)

This is the design to build once the prerequisite in section 7 is met. It reuses
`slash_confirm.rs` wholesale rather than inventing a second pending-state module.

- Ownership key. Key pending approvals by the same stable route key
  `SlashConfirmations` already uses (`slash_confirm.rs:86-103`,
  `store.session_key_for_source`), not the rotating transcript id, so a
  compression rotation cannot let an old reply target a successor conversation
  (the existing `clear` at `slash_confirm.rs:108-110` already does this for
  resets).
- Pending state. A per-route queue of pending approvals, monotonic
  `confirm_id` per entry (`slash_confirm.rs:55, 92`), 300s default timeout
  (`slash_confirm.rs:18`), superseded-on-reregister, and removed before the
  approved action runs. The one required change from the reset store is FIFO
  multiplicity: a single tool loop can request several approvals, and parallel
  delegated children can each block (Python's per-session queue,
  `approval.py:2837`), so this is a `Vec` per route, not a single slot.
- Event/request/response path. Request: a new outbound surface, either a
  `StreamEvent::ApprovalRequest { route, confirm_id, command, description }`
  drained specially by the delivery loop, or a direct adapter send scheduled off
  the tool. Either way it must reach `PlatformAdapter::send`
  (`platform.rs:25-34`) as text now, with the button seam deferred exactly as
  `resolve_by_id` (`slash_confirm.rs:144-166`) already reserves it. Response: the
  reply arrives as a normal inbound `Message`, is intercepted before slash
  dispatch (the intercept point already exists at `message.rs:107-133` and its
  push twin), and resolves through the store. The intercept must gate on the
  same tool-approval-precedence input the reset path already carries,
  `tool_approval_live` (`slash_confirm.rs:119, 127`; hardcoded `false` today at
  `message.rs:126`), so a live tool approval wins `/approve` over a pending
  reset.
- Concurrency and replay. Exactly-once is the remove-before-run fence the reset
  store already proves under contention (`slash_confirm.rs:136-140`, test
  `concurrent_replies_approve_exactly_once` at `slash_confirm.rs:449-469`). Replay
  and reconnect need the read-only snapshot accessors Python exposes
  (`list_gateway_approvals`, `get_pending_gateway_approval`,
  `ack_gateway_approval`); none exists in Rust and they must be added if any
  adapter can detach and reconnect.
- Timeout and disconnect. A bounded per-approval wait, and a run-end and
  interrupt hook that resolves every pending entry for the route as denied so no
  suspended tool hangs, mirroring `unregister_gateway_notify`
  (`approval.py:2852-2863`). On disconnect the pending entry must survive for
  the reconnect snapshot rather than resolve, so timeout and disconnect are
  distinct outcomes.
- Prompt-cache impact. None if built correctly, and a real hazard if not. The
  approval prompt must not enter the model transcript or the frozen tool prefix:
  it is control traffic delivered to the human, not a conversation turn, exactly
  as the reset confirmation adds no turn and rebuilds no prompt
  (`destructive-slash-confirm-resolution.md`, "Cache and concurrency
  disposition"). The `TerminalTool` schema stays the immutable constant it is
  today so the cached prefix is byte-stable across sessions; approval is a
  runtime gate inside `call`, never a schema field. If an approval outcome were
  ever folded into the tool result in a way that varied the prefix, the
  prompt-cache prefix would destabilize, so keep the approval exchange entirely
  outside the persisted message list.

## 7. Blockers versus optional hardening

Blockers, each of which independently prevents a safe interactive checkpoint now:

- B1. No suspend-without-holding-the-lease primitive. A blocked
  `tool.call().await` (`native_tools.rs:759`) holds the `session_id` turn lease
  (`dispatch.rs:576-587`, `turn_lease.rs:90-96`), and the `/approve` reply turn
  fails closed after the 5s lease wait (`turn_lease.rs:36, 149-160`). Until the
  tool loop can suspend a call and release or bypass the lease for control
  replies on the same session, interactive approval deadlocks its own resolution.
- B2. `Tool::call` has no context to request or await approval
  (`native_tools.rs:39-43`). It sees only args. It cannot emit a prompt outward
  or receive a reply. The trait needs an approval handle before any tool can ask.
- B3. No outbound approval-request or inbound-callback surface. `StreamEvent`
  is one-way and buffered (`stream.rs:25-65`, `dispatch.rs:670-688`) and
  `PlatformAdapter` is `send`-only (`platform.rs:25-34`). There is no way to
  deliver an interim prompt that pauses a turn and no callback ingress for a
  button reply.
- B4. HTTP ingress is one request to one response (`message.rs:62-63, 83`), so
  an interactive round trip inside a single HTTP turn is impossible without a
  poll or reconnect surface that does not exist.

Optional hardening, valuable but not gating (and only relevant after the
blockers are resolved):

- H1. Replay and reconnect snapshot accessors
  (`list`/`get_pending`/`ack` parity with `approval.py:2909-2943`) so a detached
  client can restore a prompt. Not gating for a text-only, single-adapter first
  slice, but required before any reconnectable transport ships.
- H2. Human-wait accounting parity (`approval.py:2599-2609`) so a slow human
  answer does not time out a concurrent tool batch. The Rust batch-deadline path
  is not yet a concern because delegated concurrency is limited, so this can wait
  until parallel children with a shared deadline exist.
- H3. Adapter buttons. The `resolve_by_id` seam (`slash_confirm.rs:144-166`) is
  already the shared primitive both text and button replies would call, so button
  work is pure adapter plumbing layered on later.

## 8. Explicit decision

Interactive approval should follow, not precede, a static deny-rule checkpoint.
The static path fits the current architecture with no new suspend primitive: the
native terminal is already fenced to advertise only when
`approvals.mode == "off"` and `approvals.deny` is empty on Unix with a local
backend (`native_local_terminal_eligible`, `main.rs:241-256`, gate applied at
`main.rs:832-852`). A deny-rule slice tightens that same synchronous, in-`call`
gate: it classifies a command and refuses it before `ExecBackend` runs, with no
human round trip and therefore no turn-lease suspension. That is a policy change
inside a single tool call, which the model already supports.

Interactive approval is a different shape. It requires suspending a tool call
across an inbound human reply, which the single-task, lease-per-session turn
model (section 3) cannot do safely today. Landing it first would either deadlock
on the turn lease (B1) or force a premature redesign of the tool-execution task
model under time pressure. Doing the deny-rule checkpoint first keeps the native
terminal honest for the config it already serves, and leaves the harder
execution-model change to be done deliberately, with the `slash_confirm.rs`
pending-state machinery ready to reuse when it is.
