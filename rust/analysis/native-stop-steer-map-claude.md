# Native /stop and /steer: Rust architecture map and implementation plan

Lane: Rust architecture (Claude). Companion oracle lane: Python contract in
`native-stop-steer-oracle-agy.md` (own author). This report is source-mapping
plus a concrete design; it edits no production code.

All Rust line numbers are pinned to the working tree at read time on the
`rust-rewrite` branch. A concurrent porting lane is actively editing this tree
(several `hermes-gateway/src/*.rs` files show as modified, and an untracked
`turn_control.rs` and the companion `native-stop-steer-oracle-agy.md` are
present), so re-diff before acting on any specific line number. Symbol names are
the stable anchors.

**Important reconciliation:** an in-progress `crates/hermes-gateway/src/turn_control.rs`
already exists (untracked). It implements the Checkpoint A skeleton this report
independently arrived at: a `TurnControlRegistry` with generation-stamped
`register(route_key) -> TurnControlRegistration`, a `Drop` that removes the entry
only when the stored generation still matches (the anti-stale / leak guard),
`register` cancelling any replaced predecessor, and `stop(route_key) -> StopOutcome::{Idle,Requested}`
firing a `CancellationToken`. `TurnControl` currently holds only the cancel
token (no steer buffer, no `resolved_session_id`/rebind, no `response_started`).
The design below is written to match those actual symbol names and to describe
the deltas still needed, rather than proposing a fresh file. That an
implementation already stops-first with no steer buffer corroborates the staging
recommendation in section 9.

---

## 1. Executive finding

`/stop` and `/steer` are **advertised but not implemented** in the Rust
gateway. They exist only as strings in the relay slash-command manifest
(`relay_command_manifest.rs:142` and `:144`). There is:

- no `NativeSlashCommand` arm for either command (`slash.rs:20-103`),
- no per-turn control handle or cancellation token reachable from a later
  message,
- no registry mapping a running turn to anything a second inbound message can
  act on,
- no interruptible wait anywhere in the provider retry/backoff or
  response-wait paths (`native_agent.rs:3290`, `:3300`, `:9393` are plain
  `tokio::time::sleep(...).await`),
- no steer buffer or tool-boundary injection point (the tool loop in
  `native_tools.rs:645` never consults out-of-band state).

Today, typing `/stop` on any platform is gate-allowed (`slash.rs:145`),
matched by no native handler, so it falls through and is delivered to the
model as the literal user text "/stop" (`dispatch.rs:350-365` then the normal
turn path). `/steer <text>` behaves the same. That is the behavior gap to
close.

The good news: the transcript-side vocabulary for steering is already ported.
`system_prompt.rs:313` emits `STEER_CHANNEL_NOTE` whenever tools are enabled,
and `compression_handoff.rs:439 extract_steer_text` already parses the
`[OUT-OF-BAND USER MESSAGE ...]` marker out of tool content. So the model is
already briefed to trust the marker and the compaction path already treats it
as a real user message. Only the live injection and the control plane are
missing.

The substrate needed also mostly exists but is unwired: `SessionRegistry`
(`session_registry.rs`) with `AgentSlot::{Idle,Pending,Running}`,
run-generation invalidation (`begin_run_generation` / `invalidate_run_generation`
/ `is_run_current`), and `ConversationState.queued_events`. None of it is
instantiated in the live dispatch path (only declared as a module in
`main.rs:160`). The `ApprovalBroker` (`tool_approval.rs`) is the working
precedent for the exact shape we need: an `Arc<_>` shared across every ingress,
keyed by `route_key`, mutated under a short `std::Mutex`, cleared on session
rotation via `cancel_route`.

---

## 2. Turn lifecycle in the Rust gateway (what a control plane must hook)

### 2.1 Ingress and task spawns

Push ingress only. Each platform adapter (`telegram.rs`, `slack.rs`,
`discord.rs`, ...) runs an inbound loop feeding one `mpsc::Sender<Message>`
(`main.rs:2019-2035`, `start_platform`). `Dispatcher::run`
(`dispatch.rs:176-183`) receives each `Message` and spawns a detached task per
message: `tokio::spawn(this.handle_turn(msg))`.

`handle_turn` (`dispatch.rs:~230-662`) does, in order:

1. inbound transcription enrichment,
2. tool-approval confirmation replies and reset-confirmation replies
   (`dispatch.rs:295-345`), these return before any turn,
3. slash gating + built-ins (`dispatch.rs:350-365`),
4. native lifecycle commands: Title / Resume / Compress / Reset
   (`dispatch.rs:367-517`), each returns after handling,
5. session admission (`session_admission::admit_turn`, `dispatch.rs:557`),
   which resolves the durable session, runs automatic-compression preflight,
   and hands back a transcript lease + optional durable lease,
6. acquires the per-session turn lease if admission did not
   (`dispatch.rs:616-631`),
7. spawns a **second** detached task, `run_admitted_turn`
   (`dispatch.rs:642-657`).

`run_admitted_turn` (`dispatch.rs:664-814`) then spawns a **third** task, the
agent turn itself (`agent_task`, `dispatch.rs:695-708`), which calls
`AgentClient::run_turn_with_context`. It streams `StreamEvent`s into a
`mpsc::channel::<StreamEvent>(64)`; the parent loop (`dispatch.rs:717-761`)
buffers `MessageChunk`/`Commentary` into one `reply` string and breaks on
`StreamEvent::MessageStop { final_: true }`. After the stream ends it awaits
`agent_task`, persists via `end_turn` / `finalize_turn_after_persist`, updates
session activity, applies the silence gate, and delivers.

**Task-lifetime consequence.** There are three nested spawns and **no
`JoinHandle` or `AbortHandle` is retained in any shared structure**. Once
`handle_turn` returns, nothing outside the running task tree can reach the
agent turn. A control plane must therefore register a handle at admission time,
into shared state, before the spawn, mirroring how Python claims the
`_running_agents` slot synchronously (`run.py:1400`).

### 2.2 The agent turn and its loops

`NativeAgent::run_native_turn` (`native_agent.rs:5422`) clones a per-turn
`turn_client`, resets per-turn state, then calls `run_model_turn`
(`native_agent.rs:4883`). With tools present it delegates to
`native_tools::run_tool_loop_with_messages` (`native_tools.rs:645`); with no
tools it runs a streaming retry loop inline (`native_agent.rs:4931`).

The tool loop (`native_tools.rs:671-946`) is the important one:

```
for _ in 0..max_iters {
    step = model.step(&messages, &tool_specs).await?;   // provider call (may retry/backoff inside)
    match step {
        Final(text)        => emit + MessageStop; return
        PartialFinal{..}   => emit + MessageStop; return Err
        ToolCalls{calls, assistant_message} => {
            persist(assistant_message); messages.push(assistant_message);
            for call in calls {
                emit ToolCallChunk
                content = tool.call(...).await          // <- tool executes here
                emit ToolCallFinished
                result = tool_result::build(...)
                persist(result); messages.push(result); // <- newest tool row
            }
            maintain_tool_loop_messages(...)            // same-turn compaction
        }
    }
}
```

The **safe tool boundary** for steer is exactly after the `for call in calls`
block completes and before `maintain_tool_loop_messages` and the next
`model.step` (`native_tools.rs:929-942`). At that point the newest message is a
`{"role":"tool", ...}` row that has just been persisted and pushed. Appending
the steer marker to that row's `content` keeps role adjacency intact and mutates
only the transcript tail (prompt-cache safe below that point).

Provider retry/backoff and response waits all live *inside* `model.step`:

- `wait_before_main_retry` (`native_agent.rs:~3240` caller, sleep at `:3290`),
- `wait_before_empty_response_retry` (`native_agent.rs:3294`, sleep at `:3300`),
- stale-stream 200ms sleep (`native_agent.rs:9393`),
- Z.AI long-wait notice emitted live via `try_send` (`native_agent.rs:3277`)
  then the same `:3290` sleep.

Every one is a bare `tokio::time::sleep(...).await`. None observe any cancel
signal. Cooperative interruption of a wait therefore requires threading a
cancel token down into these `wait_before_*` methods and `select!`-ing the
sleep against it.

### 2.3 Keys and identities

- **Route / session key** (`store.session_key_for_source`,
  `session_store.rs:51`): the operational key. `ApprovalBroker` and
  `SlashConfirmations` are keyed by this. `dispatch.rs:593-596` clears both via
  `slash_confirmations.clear(route_key)` and
  `tool_approvals.cancel_route(route_key)` when a session is rotated. This is
  the correct key for the control registry.
- **Resolved session_id** (`resolved.entry.session_id`): the durable transcript
  identity; the turn lease is keyed here (`turn_lease.rs`, keyed by
  `message_session_id` or the resolved id). Session rotation (compression)
  rebinds the lease from old id to new id (`turn_lease.rs:183 rebind`).
- **message_session_id** (`session_db.rs:377`): platform+channel derived,
  used as the lease key when there is no resolved session.

The control registry must key on **route_key** (so every ingress path computes
the same key the same way, and rotation clearing is already wired for that key),
and additionally stamp each entry with the resolved session_id plus a
per-turn token, so a `/stop` can refuse to act across a rotation (see §4).

### 2.4 Transcript leases and SQLite persistence

- Transcript lease (`turn_lease.rs`): a `tokio` `OwnedMutexGuard` held for the
  load/run/flush region, serializing two routing keys that map to one
  session_id. Held inside `TurnSession` (`dispatch.rs:632-635`) or as the
  admitted lease.
- Durable lease (`durable_turn_lease.rs`): cross-process obligation, carries a
  `holder` string used for diagnostics (`dispatch.rs:678-680`).
- SQLite persistence: `session_db::begin_turn` records the inbound user row for
  stateless backends (`dispatch.rs:685`); tool-loop messages are persisted
  incrementally through `model.persist_tool_loop_message`
  (`native_tools.rs:858,927`); `end_turn` records the assistant reply
  (`dispatch.rs:783`); `finalize_turn_after_persist` runs post-persist hooks.

Steer mutates a tool row that was already persisted via
`persist_tool_loop_message`. The mutated row must be re-persisted (an update to
the same row content), not appended as a new row, or replay history will diverge
from the in-memory `messages` vector.

### 2.5 StreamEvent delivery

`StreamEvent` (`stream.rs:25`) variants: `MessageChunk`, `MessageStop{final_}`,
`Commentary`, `ToolCallChunk`, `ToolCallFinished`, `LongToolHint`,
`ApprovalRequest`, `GatewayNotice{notice_kind,text,extra}`. The dispatcher only
renders text, Commentary, ApprovalRequest, and non-empty GatewayNotice
(`dispatch.rs:717-760`). A stop acknowledgment and a steer acknowledgment fit
cleanly as `GatewayNotice` with a dedicated `notice_kind` (e.g. `stopped`,
`steer_ack`), which already flows to delivery without new plumbing.

---

## 3. Advertised surfaces that can issue these commands

| Surface | Ported to Rust? | Same process as the running turn? | Can issue /stop or /steer today? |
|---|---|---|---|
| Platform push slash ingress (Telegram/Slack/Discord/...) | Yes (adapters + `Dispatcher`) | Yes: adapter loop and turn tasks share the one tokio runtime | Advertised, but no handler; text falls through to the model |
| Relay slash manifest advertisement | Manifest only (`relay_command_manifest.rs`) | n/a | Advertises `stop`/`steer` to connectors, but the `RelayAdapter` is not ported, so nothing consumes an inbound relay command yet |
| Synchronous HTTP api_server `POST /v1/runs/{run_id}/stop` | **No** (only `api_server_run_idempotency.rs`, the store, is ported; no HTTP router exists) | Would be same process if ported | No live surface |
| Local control socket (`control_socket.rs`) | Yes, but only `identify`/`status` verbs | Yes | No stop/steer verb |

**Conclusion:** the only realistic near-term issuer is the platform push slash
ingress, and it shares the tokio runtime with the running turn. That is the
single surface the first checkpoint must serve. HTTP and relay ingress are not
live and should not drive the design beyond leaving the control registry
callable from a future HTTP/relay handler.

### 3.1 Relay `send_interrupt` is a different transport concern

`relay_transport.rs:151 send_interrupt(session_key, reason)` is the
gateway-to-connector **outbound** wire leg. In Python
(`gateway/relay/ws_transport.py:761`) it sends `{"type":"interrupt",
"session_key", "reason"}` down the socket so the connector can route a /stop to
whichever gateway instance owns that session_key in a multi-instance relay
deployment. The **inbound** counterpart is `RelayAdapter.on_interrupt`
(`gateway/relay/adapter.py:1467`), which calls `interrupt_session_activity` to
set the local per-session interrupt event.

In Rust, `send_interrupt` has no callers outside the trait stub and its test
(`relay_transport.rs:262,302`), and `RelayAdapter` is not ported. So relay
interruption is:

- a routing/transport concern (which instance owns the session), distinct from
  the in-process turn cancellation this checkpoint builds,
- out of scope here beyond the requirement that the control registry expose a
  plain `interrupt(route_key)` entry point that a future ported `RelayAdapter`
  can call, exactly as a future HTTP handler would.

Do not conflate the two. `send_interrupt` never touches a `CancellationToken`
or a steer buffer; it is a socket message.

---

## 4. Proposed state machine

### 4.1 The control handle

The per-turn control object, created at admission and shared into the turn.
Existing today (`turn_control.rs`): only `cancelled: CancellationToken`. The
deltas this design adds:

```
struct TurnControl {
    cancelled: CancellationToken,       // hard stop (already present)
    steer: Mutex<Vec<String>>,          // ADD: pending out-of-band user messages, FIFO
    resolved_session_id: Mutex<String>, // ADD: updated on rotation (rebind)
    response_started: AtomicBool,       // ADD: true once any visible chunk emitted
}
```

The `generation` (turn token) already lives on the registry's `ActiveTurn`
rather than on `TurnControl`, which is fine; anti-stale identity is enforced at
the registry, not on the shared handle. `response_started` mirrors Python's
api_calls/output gate: it lets a `/stop` distinguish "interrupted before any
visible output" (safe to advise re-send) from "interrupted after visible
output" (do not replay; silence is intentional).

### 4.2 The registry

Already implemented (`turn_control.rs`):

- `register(route_key) -> TurnControlRegistration`: allocates a monotonic
  `generation`, inserts, and **cancels any replaced predecessor** on the same
  route. The returned `TurnControlRegistration` is the RAII guard; its `Drop`
  removes the entry **only if** the stored `generation` still matches (so a
  superseding turn's entry is never deleted by an older turn's drop). This is
  the detached-turn-leak and stale-crossing guard, and it is already correct.
- `stop(route_key) -> StopOutcome::{Idle,Requested}`: look up; if present,
  `cancel()` and return `Requested`; else `Idle`.

Deltas to add for steer and rotation:

- `steer(route_key, text) -> SteerOutcome`: look up; if present, push text under
  the `steer` mutex and return `Queued{preview}`; else `Idle`.
- `rebind_session(route_key, new_session_id)`: on compression rotation, update
  the stored `resolved_session_id` so the same control follows the rotated
  session. (Optional in the current skeleton, since the entry is keyed by
  `route_key` which does not change under compression; needed only if a future
  consumer keys off the session id.)
- Conversation-boundary clearing (`/new`, `/resume`, auto-reset): the current
  register-replace-cancel plus token-matched Drop already prevents a stale
  control from acting on a new turn, because the next admitted turn replaces and
  cancels the old entry. An explicit `cancel_route(route_key)` is only needed if
  a boundary can occur with no immediately-following admitted turn on that route;
  add it there for parity with `slash_confirmations.clear` /
  `tool_approvals.cancel_route` at `dispatch.rs:593-596`.

The registry is a single `TurnControlRegistry` (it is `Clone` over an inner
`Arc<Mutex<..>>`) that should be owned by `Dispatcher` like `slash_confirmations`
and `tool_approvals`, and shared across every ingress that constructs a
`Dispatcher` for the same transcripts (`with_turn_leases` sharing pattern,
`dispatch.rs:119-129`). It is not yet wired into `Dispatcher`; that wiring is
part of Checkpoint A.

### 4.3 Lifecycle (states)

```
                register(route_key)                cancel()/drain at boundary
   [no entry] ───────────────────────▶ [Running] ──────────────────────▶ [Draining]
       ▲                                  │  ▲                                 │
       │        guard Drop (token match)  │  │ steer(text): push buffer        │
       └──────────────────────────────────┘  │ (stays Running)                 │
                                              └── rebind_session on rotation ───┘
```

- **Running**: entry present, turn task alive. `/steer` pushes to the buffer;
  the tool loop drains it at the next boundary. `/stop` cancels the token.
- **Draining**: `cancel` fired; the tool loop and waits observe it, break, emit
  a terminal `GatewayNotice`, persist what exists, and the guard drops.
- The `turn_token` check on drop is what prevents an old turn from unregistering
  a newer turn that reused the route_key after a fast stop-then-new sequence.

### 4.4 Where each check fires

Hard stop (`cancel`):

1. Top of the tool loop iteration (`native_tools.rs:671`), before `model.step`
   (mirrors Python's `_interrupt_requested` check at
   `conversation_loop.py:2303`. Break, do not start another provider call.
2. Inside `wait_before_main_retry` / `wait_before_empty_response_retry` /
   stale-stream sleep: replace `sleep(d).await` with
   `select! { _ = sleep(d) => {}, _ = cancel.cancelled() => return Interrupted }`.
   This is the cooperative interruption of provider retry/backoff and response
   waits the task calls out.
3. Around the SSE forward (`forward_sse`, `native_agent.rs:4998`): pass the
   token so a long response wait aborts the stream read on cancel.

Steer (drain buffer):

1. **Tool boundary** (`native_tools.rs:929`, after the `for call in calls`
   loop): if the buffer is non-empty, pop all, append
   `format_steer_marker(joined)` to the newest tool row's `content`, and
   re-persist that row (`persist_tool_loop_message` with the updated content).
   This is the primary path and matches
   `conversation_loop.py:2387-2412`.
2. **Pre-API drain** (top of loop, before `model.step`): if a steer arrived
   during the previous provider call and the newest message is a tool row,
   append there too (mirrors `conversation_loop.py:2375-2429`). If there is no
   tool row yet (first iteration), leave it pending for the next boundary; never
   inject into a user row.

### 4.5 Slash classification and early intercept

Extend `NativeSlashCommand` (`slash.rs:20`):

```
Stop,
Steer { text: Option<String> },
```

and `native_command` (`slash.rs:80`) to classify `"stop"` and `"steer"`.

Handle them in `handle_turn` **before session admission** (before
`dispatch.rs:539` where `admit_turn`/lease acquisition begins), right after the
built-in/native-command classification block. This is the early intercept: it
must not take the turn lease, because the lease is held by the very turn being
stopped or steered. It computes `route_key = store.session_key_for_source(source)`
(same as the approval path, `dispatch.rs:296-297`), authorizes with
`slash::can_run_command(user_config, &msg, "stop"/"steer")`, then calls
`registry.stop(route_key)` or `registry.steer(route_key, text)` and delivers the
ack. It returns without spawning a turn.

Fallback for the `Idle` (no active turn) outcome, mirroring Python:

- `/stop` while `Idle`: deliver "No active agent to stop." (Python
  `gateway.stop.no_active`). Do not spawn a turn.
- `/steer` while `Idle`: this is the Python fallback that enqueues the
  text as a next-turn message. Given `queued_events` is not yet wired into live
  dispatch, the minimal non-speculative behavior is to deliver a
  "nothing running, send it as a normal message" notice rather than inventing a
  queue. Wiring `/queue` is a separate checkpoint; do not couple steer to it.

### 4.6 Threading the handle into the turn

`AgentClient::run_turn_with_context` takes a `TurnContext`
(`dispatch.rs:697-702`). Add `with_turn_control(Option<Arc<TurnControl>>)` to
`TurnContext` (the same builder pattern already used for `turn_lease_holder`,
`turn_session`, `route_key`). `run_native_turn` reads it, threads the
`CancellationToken` into `run_model_turn` and the `wait_before_*` helpers, and
threads the steer buffer into `run_tool_loop_with_messages` (extend
`ToolTurnIdentity` or add a parameter). No global; the handle is explicitly
passed, so a test can drive it directly.

---

## 5. Hazards and how the design avoids each

- **Process-global session answers.** Everything is keyed by `route_key` in a
  per-`Dispatcher` `Arc<TurnControlRegistry>`; no `static`/global map. A stop on
  one route cannot touch another. Precedent: `ApprovalBroker`.
- **Stale controls crossing session rotation.** The registry entry carries a
  `turn_token`; the RAII guard removes on drop only when the token still
  matches. `cancel_route(route_key)` runs on `/new`/`/resume` at the existing
  clearing site (`dispatch.rs:593-596`). Compression rotation calls
  `rebind_session` so the control follows the live turn instead of pointing at a
  retired session_id.
- **Duplicate user-role messages.** Steer is appended to a `role:"tool"` row,
  never inserted as a `role:"user"` row. The marker text is the exact
  `STEER_MARKER_OPEN/CLOSE` shape the system prompt and compaction already know.
  No synthetic user turn is created (Python's explicit non-goal at
  `slash_commands.py:4333` "no interrupt", and the marker's own "not a new
  delivery when replayed" clause).
- **Request replay after visible output.** `/stop` sets `cancel` and the loop
  breaks; it does **not** re-enqueue the original user message. `response_started`
  gates the ack wording: if visible output already streamed, the stop is silent
  work-drain (no "send it again" advice); only a zero-output interruption advises
  re-send (mirrors `run.py:4616-4628`). The user row was already persisted by
  `begin_turn` at turn start; stopping does not duplicate it.
- **Prompt / tool-schema mutation.** Steer only appends to the newest tool row's
  `content`. It never edits the system prompt, the tool specs
  (`tool_specs` is built once at `native_tools.rs:654` and never rewritten), or
  earlier history. The prompt-cache prefix up to the newest tool row is
  unchanged.
- **Detached-turn leaks.** The registry entry is created before the turn spawn
  and removed by an RAII guard tied to the turn task's scope
  (`run_admitted_turn`), so a panicking or early-returning turn still
  unregisters. The guard lives in the same task that owns the turn, not in a
  fire-and-forget spawn.
- **Deadlocks.** The steer buffer and registry use short `std::Mutex` sections
  that never span an `.await` (same rule `ApprovalBroker` follows,
  `tool_approval.rs:18`). The `CancellationToken` is lock-free. The stop path
  never acquires the turn lease, so it cannot deadlock against the running turn
  that holds it.

---

## 6. Race cases (and the required behavior)

1. **Stop vs natural completion.** The turn emits `MessageStop{final_:true}`
   and the guard drops at almost the same instant a `/stop` arrives.
   `stop` returns `Idle` if the entry is already gone; deliver
   "no active agent." If the entry is still present, `cancel` fires but the loop
   has already returned; the cancel is a no-op observed by nobody. Both orders
   are safe because cancel is idempotent and the loop only checks it at
   boundaries. Test: fire cancel after the loop returned; assert no panic and no
   extra delivery.
2. **Stop before first provider call (pending turn).** Admission registered the
   entry but the agent task has not reached `model.step`. `cancel` is observed
   at the top-of-loop check on the first iteration; the turn breaks with zero
   api calls and `response_started=false`, so the ack advises re-send. Mirrors
   Python's `_AGENT_PENDING_SENTINEL` stop (`run.py:19494`).
3. **Steer arriving during a provider call.** Buffer push under mutex; the
   in-flight `model.step` does not see it. The pre-API drain on the next
   iteration or the tool-boundary drain injects it. Test: push steer while a
   fake model is mid-`step`; assert the marker lands on the newest tool row on
   the next boundary, exactly once.
4. **Two steers in one provider window.** Both pushed; drained together and
   joined with `\n` into one marker (matching Python's append-with-newline at
   `conversation_loop.py:2419`). Test: push two, assert one marker containing
   both, in order.
5. **Steer then stop.** Stop wins: the loop breaks at the next boundary check
   before the steer would be consumed by a `model.step`. The un-drained steer is
   discarded with the turn (it was never persisted). Test: push steer, then
   cancel; assert the loop exits without a further provider call and the steer
   marker is not persisted.
6. **Stop during a retry/backoff wait.** `select!` resolves on
   `cancel.cancelled()`; the wait returns `Interrupted` and the loop breaks.
   Test: a fake route that forces one backoff; cancel during the sleep; assert
   the sleep is cut short (well under the backoff duration) and the turn ends
   interrupted.
7. **Concurrent duplicate `/stop`.** Two `/stop` messages race. Both call `stop`; the first returns `Requested`, the second sees the entry still
   present (cancel is idempotent) or already gone and returns `Idle`.
   Neither spawns a turn. Test: two stops on one route; assert exactly one
   `Requested`, no double-delivery of a stopped notice from the turn side.
8. **Wrong user.** `can_run_command(..., "stop")` denies a non-authorized sender
   on a gated platform (`slash.rs:123`); deliver `denial_text("stop")`. The
   running turn is untouched. Test: gated config, non-admin `/stop`; assert
   denial and that `cancel` was never fired.
9. **Rotation mid-steer.** A compression boundary rebinds the session while a
   steer is buffered. `rebind_session` keeps the same `TurnControl`, so the
   buffered steer still drains into the (rotated) live turn's newest tool row.
   A retire (`/new`) instead calls `cancel_route`, dropping the entry, so a
   steer sent to the old conversation cannot land in the new one. Test both.
10. **Fast stop then new turn reusing route_key.** Old turn's guard must not
    delete the new turn's entry. Token-matched drop covers this. Test: register
    A, register B on the same route (B supersedes), drop A's guard, assert B's
    entry survives.

---

## 7. Minimal file-by-file plan

Already present (extend, do not recreate):

- `crates/hermes-gateway/src/turn_control.rs`: has `TurnControlRegistry`,
  `TurnControl` (cancel token only), `TurnControlRegistration` (RAII guard with
  token-matched Drop), `StopOutcome`, `register`, `stop`. Add the `steer` buffer
  + `steer()`/`SteerOutcome`, `response_started`, and optional
  `rebind_session`/`cancel_route`, plus unit tests for those additions. The
  existing lock sections hold no `.await`; keep that invariant.

Edited files (small, surgical):

- `slash.rs`: add `Stop` and `Steer{text}` to `NativeSlashCommand`; classify
  in `native_command`. Add tests for classification and arg extraction.
- `dispatch.rs`: own `Arc<TurnControlRegistry>` (constructor + a
  `with_turn_control` sharing setter like `with_turn_leases`); add the early
  intercept for Stop/Steer before admission; register the control at admission
  and hold the guard in `run_admitted_turn`'s scope; set `response_started`
  from the first `MessageChunk` in the delivery loop; call `cancel_route` at the
  existing rotation-clearing site and `rebind_session` where compression rotates
  the session; thread `with_turn_control` into the `TurnContext`.
- `agent.rs`: add `TurnContext::with_turn_control(Option<Arc<TurnControl>>)` and
  the field (builder parity with the existing `with_*` methods).
- `native_agent.rs`: read the control from context in `run_native_turn`; add a
  top-of-loop cancel check and thread the token into `run_model_turn`; make
  `wait_before_main_retry`, `wait_before_empty_response_retry`, and the
  stale-stream sleep `select!` on the token; pass the token to `forward_sse`.
- `native_tools.rs`: extend `run_tool_loop_with_messages` (and
  `ToolTurnIdentity`) to accept the control; add the top-of-loop cancel break,
  the pre-API steer drain, and the tool-boundary steer drain + re-persist of the
  mutated tool row.
- `relay_command_manifest.rs`: unchanged (already advertises correctly).

Note: several of these files already show as modified in the working tree from
the concurrent implementation lane; reconcile against their current state before
adding the deltas above rather than assuming the line numbers here.

Explicitly **not** touched: `control_socket.rs` (no verb yet),
`api_server_run_idempotency.rs` and any HTTP router (no live surface),
`relay_transport.rs` (`send_interrupt` stays a transport stub), `PORT.md`,
`INDEX.md`.

---

## 8. Focused Rust tests

Registry unit tests (`turn_control.rs`): stop-then-drop token safety and
superseding-token drop keeping the newer entry are the existing concerns (some
may already be covered by the in-tree tests, verify before duplicating). Add:
steer buffering and FIFO join; `Idle` outcome on an empty route; steer preview
truncation; and, if added, `rebind_session` keeps the same handle and
`cancel_route` scoping.

Tool-loop tests (`native_tools.rs`, with a fake `ChatModel`): steer injected at
the boundary lands on the newest tool row exactly once and is re-persisted;
two steers join into one marker; cancel at top-of-loop breaks before the next
`step`; cancel wins over a pending steer (marker not persisted); no steer means
no transcript change (byte-identical messages vector).

Agent-wait tests (`native_agent.rs`): cancel during `wait_before_main_retry`
returns promptly (assert elapsed well under the backoff); a turn that starts
already-cancelled emits no provider call.

Dispatch integration tests (`dispatch.rs`, existing test harness style with a
scripted agent, cf. `dispatch.rs:817+`): `/stop` on a live turn cancels it and
delivers a stopped notice without spawning a new turn; `/stop` with no active
turn delivers "no active"; a non-authorized `/stop` on a gated platform is
denied and never cancels; `/steer` on a live turn delivers the queued ack and
the marker appears in persisted history; `/new` after a buffered steer drops the
control so the steer cannot cross into the new conversation.

---

## 9. One checkpoint or two?

**Recommendation: staged, two checkpoints, sharing one foundation.**

Rationale grounded in concrete consumers, not speculation:

- The only live issuer is platform push slash ingress; both commands are
  advertised there, so there is real value in both. But they carry very
  different risk.
- **Checkpoint A: control substrate + `/stop`.** This is the safety-critical
  half and the smaller change: `turn_control.rs`, the slash classification, the
  early intercept, the admission-time registration and RAII guard, and the
  cancel checks (top-of-loop plus the three `wait_before_*`/SSE `select!`
  seams). It touches no persisted history, so its fidelity surface is small.
  Ship this first; it delivers a working `/stop` and lays the exact registry and
  `TurnContext` threading steer will reuse.
- **Checkpoint B: `/steer`.** This adds the steer buffer drain and, critically,
  **mutation and re-persistence of a tool row in live history**. That is a
  transcript-fidelity change (re-persist semantics, replay parity, compaction
  interaction via the already-ported `extract_steer_text`) that deserves its own
  review against the oracle golden, independent of the stop cancellation
  mechanics. It is strictly additive on top of A's registry.

Splitting this way keeps each checkpoint's blast radius auditable, gets the
safety command (`/stop`) in first, and avoids blocking a clean cancellation
mechanism behind the harder-to-verify transcript injection. If the reviewing
maintainer prefers a single landing because both are advertised together, the
same file plan applies as one change; the staging is about review risk, not a
technical dependency in the other direction (B depends on A, never the reverse).

---

## 10. Open items to confirm against the oracle lane

- Exact ack strings and the interrupted-terminal wording (Python
  `gateway.stop.*` keys and the `⚡`/`⏩` prefixes at `slash_commands.py:1461-1516`
  and `run.py:18747`). Match the oracle report's observed strings verbatim.
- Whether `/steer` with no running turn must enqueue for the next turn
  (Python fallback) or may decline. This report scopes it to decline, because
  the live-dispatch queue is unported; confirm the oracle agrees this is an
  acceptable interim before `/queue` lands.
- The precise `response_started`-vs-api_calls gate for the re-send advisory
  (`run.py:4616-4649`), so the Rust `AtomicBool` gate matches the Python
  condition exactly.
