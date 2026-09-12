# Native /steer: Rust persistence and ownership audit

Lane: Rust persistence and ownership (Claude). Companion executable-contract
lane: `native-steer-contract-agy.md` / `native-steer-goldens.json` (AGY author,
not edited here). Foundational stop/steer architecture map:
`native-stop-steer-map-claude.md` (same author). This report edits no
production code, no `PORT.md`, no `INDEX.md`, and no AGY-lane file.

All Rust line numbers are pinned to the working tree at read time on
`rust-rewrite` after commit `25a1d1f712`. **A concurrent lane is editing this
tree while this audit was written.** `turn_control.rs` changed under me mid-read
(the steer buffer, `SteerOutcome`, `steer()`, and `take_pending_steer()` landed
between two reads of the same file), so re-diff before acting on any line
number. Symbol names are the stable anchors.

---

## 0. What already landed vs. what this lane must still build

The cooperative-stop checkpoint (`25a1d1f712`) landed more than `/stop`. The
control substrate now already carries a steer buffer:

- `turn_control.rs`: `TurnControl` holds `pending_steer: Arc<Mutex<Option<String>>>`
  with `queue_steer` (newline-accumulate) and `take_pending_steer` (atomic take).
  `TurnControlRegistry::steer(route_key, text)` returns
  `SteerOutcome::{Idle, Empty, Queued{preview}}`: `Idle` when no active turn or
  the turn is already cancelled, `Empty` when the text is whitespace-only after
  `python_whitespace` trim, else `Queued` with a 60-char (`chars().take(60)`)
  preview plus `"..."` when the cleaned text exceeds 60 code points
  (`turn_control.rs:154-172`).
- The registry is owned by both ingress surfaces (`Dispatcher.turn_controls`
  at `dispatch.rs:40`, HTTP `state.turn_controls` at `message.rs`), shared via
  `with_turn_controls` (`dispatch.rs:134`), registered per admitted turn
  (`dispatch.rs:667`, `message.rs:447`), threaded into the turn through
  `TurnContext::with_turn_control` (`agent.rs:109`), and retired with
  `TurnControlRegistration::finish()` returning whether stop won
  (`dispatch.rs:807`, `message.rs:519`).
- The tool loop already observes the cancel half of the control: a top-of-batch
  `is_cancelled` skip (`native_tools.rs:887`), a `select!` of the cancel token
  against each tool call (`native_tools.rs:922-929`), and an interrupted
  terminal row (`native_tools.rs:955-970`).

So the ownership plumbing steer needs is present. **What is missing is every
step that consumes the steer buffer:**

1. Neither ingress path calls `turn_controls.steer(...)`. There is no `Steer`
   arm in `NativeSlashCommand` (`slash.rs:20-32`, only `Stop`, `Reset`, `Title`,
   `Resume`, `Compress`), and no busy-message steer routing. A `/steer` today
   is not classified, so it either flows to the model as literal text or, on the
   synchronous HTTP path, tries to take the turn lease and gets a `409 session
   is busy` (`message.rs:437`) because the running turn holds the lease.
2. The tool loop never calls `take_pending_steer`. The boundary drain
   (`conversation_loop.py` post-tool-batch and pre-API) is not ported.
3. There is no way to re-persist a tool row after mutating it. `append_native_tool_message`
   only INSERTs (see section 3); no update-in-place path exists.
4. Leftover-steer promotion (Python `result["pending_steer"]`) is not ported on
   either surface.

The rest of this report specifies the smallest complete build of items 1 to 4
with Python parity, and the persistence contract for item 3, which is the
delicate part.

---

## 1. Busy /steer ingress: push and synchronous HTTP

### 1.1 The Python model is a busy-input mode, not only a slash verb

Python delivers steer two ways, both landing in `AIAgent.steer(text)`
(`run_agent.py:3908-3942`), which trims, ignores empty, and newline-accumulates
under `_pending_steer_lock`:

- **Implicit**, when `busy_input_mode == "steer"` and a plain-text follow-up
  arrives during a run: `_handle_active_session_busy_message`
  (`gateway/run.py:11273`) calls `running_agent.steer(steer_text)`
  (`:11490`).
- **Explicit** `/steer <text>` busy command: `_busy_steer_command`
  (`gateway/run.py:18719`).

The Rust gateway has no busy-input-mode machinery (there is no pending-event
FIFO, no `busy_text_mode`; a second concurrent turn is simply refused by the
lease). The faithful, smallest Rust surface is therefore the **explicit
`/steer <text>` slash verb**, classified exactly like `/stop`, matching
`_busy_steer_command`. Implicit steer-mode is a larger, separate feature (it
needs the busy FIFO that is unported, my prior map's Checkpoint dependency) and
must not be smuggled in here.

### 1.2 Exact authorization ordering (both surfaces, mirror /stop)

`/stop` already establishes the ordering both surfaces must reuse verbatim, and
it is the same as Python's busy-path guard (`gateway/run.py:11279`
`_is_user_authorized` before any steer):

1. `slash::evaluate(&user_config, &msg)` first (`dispatch.rs:362`, `message.rs:163`).
   On `Denied{command}` deliver `slash::denial_text(&command)` and return; the
   running turn is never touched.
2. On `Allowed{command}`, `handle_builtin` short-circuit, then
   `slash::native_command(&command, &msg.text)`.
3. Only then match the `Steer` arm, derive the control key, and call
   `turn_controls.steer`.

Authorization must precede the registry call. A denied sender must never reach
`steer`, exactly as a denied sender never reaches `stop` today.

### 1.3 Control-key derivation (identical to /stop)

Both surfaces compute the key the same way `/stop` does, so steer and stop
address the same registry entry:

- Push: `session_store` present maps `source_from_message` through
  `store.session_key_for_source`, else `session_db::message_session_id(&msg)`
  (`dispatch.rs:379-384`).
- HTTP: `routing_key.clone().unwrap_or_else(|| session_id.clone())` is what
  `register` used at admission (`message.rs:446-447`); the intercept must
  reproduce that same fallback so a `/steer` with no routing key still finds the
  entry keyed at admission. Using `session_key_for_source` here (as `/stop`
  does at `message.rs:179-182`) is only safe if it yields the same string
  `register` was given. This is the one place the two `/stop` sites already
  differ from `register`; confirm the key equivalence when wiring steer, or key
  both off `routing_key`.

### 1.4 Argument parsing, acknowledgement, preview slicing

Parse: everything after the verb, trimmed, is the steer text. Empty is a usage
error, not a queue. Python strings to match exactly (`gateway/run.py:18719-18760`):

| Condition | Python reply | Rust source of the outcome |
|---|---|---|
| empty args | `Usage: /steer <prompt>` | classify before calling `steer` |
| queued on live turn | `⏩ Steer queued \u2014 arrives after the next tool call: '{preview}'` | `SteerOutcome::Queued{preview}` |
| empty payload rejected | `Steer rejected (empty payload).` | `SteerOutcome::Empty` |
| agent not started yet | `Agent still starting \u2014 /steer queued for the next turn.` | see §5.3 (no live-registry entry maps to `Idle` in Rust) |
| no running agent | queue fallback / next-turn | `SteerOutcome::Idle` (see §5) |

Preview slicing is already Python-faithful in `turn_control.rs:167-170`:
`steer_text[:60] + ("..." if len > 60 else "")`. The registry does the trim and
preview; the ingress handler only formats the outcome into the reply string. Do
not re-slice in the handler or the boundary drifts. The implicit steer-mode ack
`⏩ Steered into current run{status_detail}. Your message arrives after the next
tool call.` (`gateway/run.py:11636-11640`) is out of scope with steer-mode
itself; the explicit-verb ack above is the one to port.

### 1.5 FIFO buffer that does not interrupt provider work

`queue_steer` newline-appends under the `pending_steer` mutex
(`turn_control.rs:56-65`), FIFO by arrival, matching Python's
`_pending_steer = existing + "\n" + cleaned`. The AGY concurrent-submission
determinism assertion targets this. The buffer is route-scoped (lives on the
per-turn `TurnControl` behind the registry's route key), never global.

Non-interruption is structural: `steer` only pushes to a mutex; it never touches
the `CancellationToken`, never acquires the turn lease, and returns immediately.
The in-flight `model.step` and any running tool are not signalled. This is the
concrete contrast with `/stop`, which cancels. The mutex sections in
`turn_control.rs` hold no `.await`, so a steer from the ingress task cannot
deadlock the turn task.

---

## 2. Injection after the current tool batch, before the next provider call

Python has two drains, both consuming the same buffer via `_drain_pending_steer`:

- **Post-tool-batch:** `apply_pending_steer_to_tool_results`
  (`agent_runtime_helpers.py:5223`), invoked after each batch
  (`tool_executor.py:1933,2871,2933`).
- **Pre-API:** the top-of-loop drain (`conversation_loop.py:2387-2424`) so a
  steer that arrived during the previous provider call lands on THIS iteration
  even if no further tool batch follows.

Both scan backward for the last `role:"tool"` message in the recent tail and
append `format_steer_marker(text)` to its `content`. For string content it is
`existing + marker`; for list/block content (Anthropic multimodal) it appends a
`{"type":"text","text": marker.lstrip()}` block (the post-batch path lstrips the
marker's leading `\n\n`; the pre-API path appends the marker unstripped,
`conversation_loop.py:2402`). If no tool row is found, the text is put back into
the buffer for a later boundary.

### 2.1 Where the Rust drain goes

The safe boundary is after the `for call in calls` loop and after the existing
cancel-terminal check, before `maintain_tool_loop_messages`
(`native_tools.rs:954` then `:971`). At that point `messages.last()` is the
just-persisted `role:"tool"` row. The pre-API drain goes at the very top of the
`for _ in 0..max_iters` body, before `model.step` (`native_tools.rs:680`).

### 2.2 String vs. structured content, matching Python exactly

`tool_result::build` produces the tool row; its `content` is what the drain
mutates. The Rust helper must reproduce the Python branch precisely:

- `content` is a JSON string: set it to `format!("{existing}{marker}")`.
- `content` is a JSON array (multimodal blocks): push
  `json!({"type":"text","text": marker_trimmed})` where the marker's leading
  whitespace is stripped for the block form, matching `marker.lstrip()`.
- Anything else: fall back to string concatenation (Python's `except` branch).

The marker text must be byte-identical to Python's `STEER_MARKER_OPEN` /
`STEER_MARKER_CLOSE` (`agent/prompt_builder.py:733-743`). Rust already knows the
close and an open prefix in `compression_handoff.rs:441-442`
(`extract_steer_text`), and the full note is loaded via `guidance("STEER_CHANNEL_NOTE")`
(`system_prompt.rs:313`). **The injection helper must reuse the exact bundled
marker open/close, not re-hardcode a paraphrase**, or `extract_steer_text` and
the model briefing drift from what is injected. The AGY golden should pin the
literal open/close bytes so a Rust unit test asserts them.

### 2.3 Backward scan window

Python's post-batch scan is bounded to the batch (`num_tool_msgs`); the pre-API
scan walks the whole list. The Rust boundary drain can target `messages.last()`
directly because the loop guarantees the newest row is the last tool result of
the batch just built. The pre-API drain should scan backward for the last
`role == "tool"` row (mirroring `conversation_loop.py:2390`), because
`maintain_tool_loop_messages` may have appended nothing but a continuation could
sit after it. If no tool row exists (first iteration, no tools yet), leave the
text in the buffer; never inject into a user or assistant row (role-alternation
and prompt-cache safety).

---

## 3. Atomic re-persistence of the already-inserted latest tool row

This is the core of the persistence lane and the one place Rust diverges
structurally from Python.

### 3.1 Why Rust has a split-history risk that Python does not

`append_native_tool_message` and `persist_tool_loop_message` **exist only in
Rust** (confirmed: no Python definition anywhere in the tree; the Python native
loop keeps `messages` as the working copy and persists through `hermes_state`,
so its in-memory steer marker is naturally included whenever the list is saved).
Rust chose eager, append-only, tail-validated persistence: each tool result is
written to SQLite the instant it is built, inside the `for call in calls` loop
(`native_tools.rs:952` -> `TranscriptModel::persist_tool_loop_message`
`native_agent.rs:135` -> `append_native_tool_message` `session_db.rs:4155`).

So by the time the steer drain runs at the batch boundary, the newest tool row
is **already durable with pre-steer content**. Mutating `messages.last()` in
memory (section 2) makes the in-memory transcript carry the marker while the
durable row does not. On a restart / durable reload
(`build_messages_from_durable`, `native_agent.rs:297`), the model would then
never see the steer that the live turn acted on. That is the "in-memory and
durable history split" the task names, and it is Rust-specific.

### 3.2 Why the existing append method cannot re-persist

`append_native_tool_message` INSERTs a new row after validating the tail phase
(`session_db.rs:4242-4273`). Re-calling it with the mutated tool row would be a
second `role:"tool"` INSERT whose tail phase is `Tools(pending)` with `pending`
already emptied by the first insert; the `("tool", Tools(pending))` arm requires
`pending.contains(tool_call_id)`, which now fails, so it returns `Ok(false)` and
`persist_tool_loop_message` turns that into `Error::Other("...rejected an
invalid tail")` (`native_agent.rs:149-153`). It is structurally a append-only
guard; it must not be bent into an update. So `persist_tool_loop_message` needs
a sibling, not a new overload of itself.

### 3.3 The invariants a correct re-persist must honor

A new `SessionDb` method (call it `amend_native_tool_tail` / `replace_native_tool_tail`)
must run inside one `transaction_with_behavior(Immediate)` and enforce, in the
same order the existing writers do:

1. **Lease / CAS check.** If `turn_lease_holder` is `Some`, resolve
   `compression_lineage_root_on(&tx, session_id)` and require the live
   `session_turn_leases` row's `holder` for that conversation to equal it, with
   `expires_at >= now` (`session_db.rs:4218-4231`, identical to every other
   native writer). A mismatch returns `Ok(false)` so the caller surfaces the
   same "rejected" error and does not silently diverge. This is the guard that a
   rotated / rebound session or a stolen lease cannot have its tail rewritten by
   a stale turn.
2. **Liveness.** `ended_at IS NULL` for the session (`session_db.rs:4232-4241`).
   A closed (compressed / rotated-away) session rejects the amend.
3. **Row identity.** Locate the exact row to update by
   `MAX(id) WHERE session_id = ? AND active = 1 AND role = 'tool'`, and verify
   that row is still the tail (no newer active row of any role sits after it) and
   that its `tool_call_id` matches the in-memory row the drain mutated. Update by
   primary-key `id`, never by content match. The identity anchor prevents
   amending the wrong row if a concurrent writer appended after the drain read.
   If the newest active row is not that tool row, return `Ok(false)` and keep the
   marker buffered (the drain fell behind a rotation or another append).
4. **What changes.** Only the `content` column of that one row (and, if the drain
   produced multimodal blocks, its serialized form via `native_persisted_content`
   the same way the INSERT encodes it, `session_db.rs:4275`). `tool_calls`,
   `tool_call_id`, `tool_name`, `role`, `timestamp`, `active` stay untouched.
   `message_count` and `tool_call_count` must NOT change (no new row).
5. **Atomicity.** `content` update and nothing else, then `commit`. On any
   rejection return before `commit` (implicit rollback on drop of `tx`).

### 3.4 FTS effects

The messages FTS5 index is external-content with sync triggers
(`session_db.rs:3736-3759`). The `messages_fts_update` trigger fires
`AFTER UPDATE ON messages` and does a delete-then-reinsert of the row's
`content`/`tool_name`/`tool_calls` into the FTS index keyed by the same rowid
(`session_db.rs:3754-3758`). So a **plain `UPDATE messages SET content = ?
WHERE id = ?` re-indexes the steered row automatically and correctly**, keeping
the same rowid. This is the decisive reason to update in place rather than
delete-and-reinsert: a DELETE+INSERT would allocate a new `id`, break the
`tool_call_id` adjacency the tail validator relies on, and (via the delete/insert
triggers) churn the FTS rowid. The existing `fts_reflects_edits_and_deletes`
test (`session_db.rs:8439`) already proves an in-place content UPDATE re-indexes;
the amend method inherits that behavior for free.

### 3.5 Ordering: mutate memory and durable together, fail closed

The drain must treat the in-memory mutation and the durable amend as one logical
step and fail closed if the durable side is refused:

- Take the steer text (`take_pending_steer`), format the marker, mutate
  `messages.last()`.
- Call the amend method. On `Ok(true)`, proceed.
- On `Ok(false)` / `Err`, the durable row could not be updated. The safest
  Python-consistent choice is to **roll the in-memory mutation back** (restore
  the pre-marker content) and re-buffer the steer so a later boundary or the
  leftover path re-attempts, rather than sending a marker to the provider that
  is not in durable history. This keeps memory and disk identical at every
  provider call. Document this as the invariant: the marker is visible to the
  model only after it is durable.

An acceptable weaker alternative, if the maintainers prefer Python's
"in-memory is source of truth" stance, is to log-and-continue on amend failure
(the live turn still acts on the steer, durable reload just misses it). This
report recommends fail-closed because Rust's whole native-persistence design is
built on durable == in-memory (every other writer returns `false` and the caller
errors); a silent split would be the one exception.

### 3.6 Prefer the deep helper, not a special-case SQL shortcut

Do not inline a bare `UPDATE messages SET content` at the call site or in
`TranscriptModel`. Add the method on `SessionDb` next to
`append_native_tool_message`, reusing `compression_lineage_root_on`,
`native_persisted_content`, and the same Immediate-transaction + lease + liveness
preamble those neighbors use. That keeps all tail invariants in one audited place
and inherits the FTS trigger behavior. The compression writers
(`publish_gateway_in_place_compression`, `session_db.rs:2147`) already do
in-place `UPDATE messages SET content` inside a leased, snapshot-checked
transaction; the amend method is a much smaller sibling of that pattern
(single row, no snapshot compare, no route rewrite).

---

## 4. Pre-API draining without duplicating a marker

The de-duplication is not a comparison; it is atomic ownership of the buffer.
`take_pending_steer` (`turn_control.rs:67-69`) does `Option::take` under the
mutex, so whichever drain runs first empties the buffer and the other sees
`None`. Python gets the same property from `_drain_pending_steer` clearing
`_pending_steer` under its lock.

Concrete sequence and why no double-marker occurs:

1. Steer arrives during `model.step`. Buffer = Some(text).
2. Batch completes, boundary drain takes the buffer (now None), appends one
   marker to the tool row, amends durably.
3. Next iteration top-of-loop pre-API drain takes the buffer, sees None, does
   nothing.

The only way a second marker lands on the same row is if a **new** steer arrives
between step 2 and step 3; that is correct, it is genuinely new text, and it
appends a second marker to the still-newest tool row plus a second amend. That
is not duplication.

The one real hazard to guard: the boundary drain and the pre-API drain must both
use `take_pending_steer`, never a peek-then-apply that leaves the buffer
populated. And the run-budget wrap-up injector (Python `_maybe_inject_run_budget_wrapup`,
`conversation_loop.py:2432`, which shares the same newest-tool-row channel and,
per the retry-notices lane, is the "marker already applied after a tool batch")
must not itself consume or race the steer buffer; it appends its own distinct
text. Keep the two injectors reading disjoint state so the steer drain and the
budget wrap-up cannot clobber each other's amend of the same row (they should
serialize: drain steer, amend, then wrap-up, amend, or combine both appends into
one amend of the row before a single durable update).

---

## 5. A steer arriving during a final text-only response

Python: if the turn ends (final assistant text, no further tool batch) with the
buffer non-empty, `_drain_pending_steer` in the turn finalizer
(`turn_finalizer.py:779-781`) returns the leftover as `result["pending_steer"]`.
Two consumers diverge by surface.

### 5.1 Push: promote to the next normal user turn

`gateway/run.py:32638-32642`: after the turn, if there is no other pending
follow-up, `pending = result["pending_steer"]` and it is delivered as the next
user turn (with a slash-command safety filter at `:32649` discarding anything
that resolves to a command). This is a genuine next-turn promotion after
ownership releases.

**Rust mapping.** The natural seam is the post-turn region where
`TurnControlRegistration::finish()` runs (`dispatch.rs:807`). Ownership releases
there. For a leftover steer to promote, the turn must surface it out of
`run_native_turn` (there is no `pending_steer` field on the Rust turn result
today; that plumbing is unbuilt), and the Dispatcher must re-enter a turn with
that text as the user content. Rust has no pending-event FIFO, so the minimal
faithful push behavior is: after `finish()`, if the turn returned a leftover
steer and there is nothing else queued, synthesize one follow-up `Message` with
that text and dispatch it as a fresh turn on the same route (respecting the same
slash-safety filter so a `/`-prefixed leftover is dropped). This is additive and
does not touch prompt bytes.

If that re-entry is judged too large for this checkpoint, the honest interim is
to drain the leftover into the turn's normal delivered reply as a trailing
notice ("your steered message arrived after I finished; resend it to continue"),
which loses nothing and executes nothing. Prefer real promotion; fall back to
the notice only if re-entry is deferred.

### 5.2 Synchronous HTTP: surface, never silently lose or execute

`api_server.py:5011-5034` puts `pending_steer` into the `run.completed` payload
and the run status; it neither replays nor drops it. The Rust synchronous HTTP
surface (`message.rs`) is different in shape: `/steer` and the running turn are
**two separate HTTP requests**. The steer request returns its `⏩ ... queued`
ack immediately; the running turn's request is blocked on its own reply and
returns a bare `MessageResponse{reply}` (`message.rs`), which has no
`pending_steer` field.

Truthful minimal behavior for Rust synchronous HTTP:

- The running turn must not execute an undeliverable steer (it never reaches a
  tool boundary in a text-only turn, so the buffer is simply never drained into
  a provider call, which is already the case).
- It must not silently lose it. The smallest honest change is to surface the
  leftover: either add an optional `pending_steer` field to `MessageResponse`
  (parity with `run.completed`), or, given the ack already told the steering
  client "arrives after the next tool call," emit a truthful terminal
  `GatewayNotice` on the running turn's stream (kind `steer_undelivered`) so the
  turn's own client learns the steer did not land. The steering client already
  has its ack; the honest signal is that the promise ("after the next tool
  call") was not kept because no tool call followed.
- Do NOT convert it into a hidden next turn on HTTP: there is no next-turn
  concept per request, so a silent promotion would execute text the client never
  re-submitted. Surfacing (return it) is the truthful option; promotion is a
  push-only affordance.

### 5.3 Agent-not-started-yet

Python `/steer` when `running_agent is _AGENT_PENDING_SENTINEL`
(`gateway/run.py:18729`) queues the text as a turn-boundary fallback and replies
`Agent still starting \u2014 /steer queued for the next turn.` In Rust the registry
entry is created at admission (`register`) before the agent task is spawned, so
`steer` returns `Queued` even before `model.step` runs; the buffered text drains
at the first tool boundary. There is no `_AGENT_PENDING_SENTINEL` gap to
reproduce, because registration is synchronous with admission. Only a genuinely
absent entry (`SteerOutcome::Idle`) needs the no-active-turn reply. Note this as
a deliberate, benign divergence from Python's sentinel wording.

---

## 6. Race, rotation, reset, and stale-generation behavior

Prompt bytes and tool schemas stay frozen throughout: steer only appends to a
tool row's `content` in memory and updates that one row's `content` column
durably. `tool_specs` is built once (`native_tools.rs:662`) and never rewritten;
the system prompt and marker text are unchanged; no user or assistant row is
inserted. Every case below preserves that.

1. **Stop vs. steer.** `steer` refuses when `active.control.is_cancelled()`
   (`turn_control.rs:159-161`), returning `Idle`. So a steer arriving after a
   stop is dropped, matching Python's hard-interrupt clearing of `_pending_steer`
   (`run_agent.py:3898-3906`). A steer already buffered when a stop fires is
   simply never drained: the loop breaks at the cancel check
   (`native_tools.rs:887`/`:955`) before the next `model.step`, and the buffered
   text dies with the turn (never persisted). Test: buffer a steer, cancel,
   assert no amend and no marker in durable history.
2. **Steer then natural completion.** Covered in section 5. The buffer survives
   to the finalizer as leftover; `finish()` reports the turn completed
   (`!cancelled`), and the leftover is surfaced (HTTP) or promoted (push).
3. **Session rotation (compression) mid-buffer.** The registry key is
   `route_key`, which does not change under compression, so the same
   `TurnControl` (and its buffer) stays attached to the live turn across an
   in-place or rotation compression. The amend's lease/CAS check resolves
   `compression_lineage_root_on`, so it targets the rotated session's live tail
   correctly. Because `publish_gateway_*_compression` clones/rewrites rows under
   the same lease, a steer amend and a compression publish serialize on the
   Immediate transaction; whichever commits second sees the other's rows. If the
   amend loses the race (its target row got `active = 0` by the in-place
   compaction), the newest-active-tool-row identity check (§3.3) returns
   `Ok(false)` and the marker re-buffers. Test: interleave a steer amend with an
   in-place compression on the same session; assert no split and no double
   marker.
4. **Reset / new session (`/new`, `/reset`).** A conversation boundary must not
   let a steer cross into the new conversation. The next admitted turn on the
   route calls `register`, which `insert`-replaces and cancels the predecessor
   `TurnControl` (`turn_control.rs:125-133`); the replaced control (with its
   buffer) is dropped, so a steer sent to the old turn cannot land in the new
   one. If a reset can occur with no immediately-following admitted turn, add an
   explicit `cancel_route` at the existing clearing site alongside
   `tool_approvals.cancel_route` (the `/stop` path already calls that at
   `dispatch.rs:389`). Test: register A, register B on the same route, assert
   A's buffer is unreachable and B is clean.
5. **Reset/resume durable side.** `append_native_tool_message` and the amend both
   reject when `ended_at IS NULL` is false, so a steer amend against a session
   that was ended by resume-into-different-session fails closed. The lease holder
   mismatch after a rebind likewise rejects. No frozen-prompt violation because a
   rejected amend rolls back the in-memory marker (§3.5).
6. **Stale generation.** The registry stamps each entry with a monotonic
   `generation` (`turn_control.rs:99-100,122-123`); `finish` and `Drop` remove
   the entry only when the stored generation still matches
   (`turn_control.rs:65-77,103-116`). So a superseded turn's drop never deletes
   the newer turn's entry, and `steer` always addresses the current generation's
   buffer. A stale turn that keeps running after being superseded finds its
   control cancelled (predecessor was cancelled on replace), so its own steer
   drain is a no-op and its amend, if attempted, fails the lease check against
   the new holder. Test: register, supersede, assert the old control is cancelled
   and cannot amend.
7. **Concurrent steers (FIFO determinism).** Two `steer` calls serialize on the
   registry mutex, then `queue_steer` serializes on the buffer mutex; arrival
   order is preserved and newline-joined. This is the AGY concurrent-submission
   assertion. Test mirrors `steer_normalizes_accumulates_and_previews_in_arrival_order`
   (`turn_control.rs:225`).

---

## 7. `persist_tool_loop_message` and the replace-tail contract

**`persist_tool_loop_message` should NOT be overloaded to re-persist.** It is
an append-only, tail-validated INSERT (section 3.2). Steer needs a distinct
`ChatModel` trait method, e.g.:

```
/// Re-persist the newest durable tool row's content after a mid-turn steer
/// marker (or run-budget note) was appended in memory. Returns Ok(()) only if
/// the durable tail still matches the in-memory row it amends. Stateless/test
/// models keep the default no-op.
fn replace_tool_loop_tail(&self, message: &Value) -> Result<()> { let _ = message; Ok(()) }
```

`TranscriptModel::replace_tool_loop_tail` (`native_agent.rs`, next to
`persist_tool_loop_message` at `:135`) resolves `session_id` and
`turn_lease_holder` the same way and calls the new
`SessionDb::amend_native_tool_tail`, converting `Ok(false)` into the same
`Error::Other("...rejected an invalid tail")` shape the sibling uses so the
drain's fail-closed path (§3.5) triggers uniformly.

### 7.1 How the test fakes must model it

The existing fake pattern is a `Mutex<Vec<Value>>` that records each
`persist_tool_loop_message` call (`PersistingModel`, `native_tools.rs:2339-2352`).
A steer-aware fake must model the amend as an in-place edit of the recorded tail,
not a new push, so tests can assert "one tool row, content includes exactly one
marker, no extra rows":

- `replace_tool_loop_tail`: find the last recorded `role:"tool"` entry in the
  `persisted` vec and replace its `content` with the incoming message's content
  (or, more faithfully, assert the incoming `tool_call_id` matches the recorded
  tail's before replacing). Return `Ok(())`.
- A negative fake (models a rejected tail) returns the error, so the drain's
  rollback path is exercised.

This keeps the fake honest about the append-only vs. amend distinction: a naive
fake that just pushes the amended message would hide a real regression where the
production amend accidentally INSERTs.

---

## 8. Minimal file-by-file plan

Already landed (do not recreate): `turn_control.rs` steer buffer + `steer()` +
`SteerOutcome` + `take_pending_steer` + preview; control registration,
threading, and `finish()` on both surfaces.

Edits to add:

- `slash.rs`: add `Steer { text: Option<String> }` to `NativeSlashCommand`
  (`:20`); classify `"steer"` in `native_command` (`:80`) taking the remainder
  as text; add classification + empty-arg tests.
- `dispatch.rs`: add the `Steer` intercept beside the `Stop` intercept
  (`:378-395`): authorize (already done upstream by `evaluate`), derive
  `control_key` identically, call `turn_controls.steer`, map the outcome to the
  exact reply strings (§1.4), deliver, and return without admission. After
  `finish()` (`:807`), add leftover-steer promotion (§5.1).
- `message.rs`: mirror the `Steer` intercept beside the HTTP `Stop` intercept
  (`:178-193`), returning the ack before lease acquisition so a busy `/steer`
  does not hit `409 session is busy`. Surface leftover steer per §5.2.
- `native_tools.rs`: in `run_tool_loop_with_messages`, add the pre-API drain at
  loop top (`:680`) and the post-batch drain after the tool loop / before
  `maintain_tool_loop_messages` (`:954`/`:971`); both use `take_pending_steer`,
  the shared marker helper (string + multimodal branches, §2.2), the in-memory
  mutation, and `model.replace_tool_loop_tail` with fail-closed rollback (§3.5).
  The control is already on `ToolTurnIdentity.control` (`:67`).
- `native_agent.rs`: add `TranscriptModel::replace_tool_loop_tail` next to
  `persist_tool_loop_message` (`:135`) delegating to the new SessionDb method.
- `session_db.rs`: add `amend_native_tool_tail` next to
  `append_native_tool_message` (`:4155`) with the §3.3 invariants; it inherits
  the FTS update trigger.
- `agent.rs` / trait: add `ChatModel::replace_tool_loop_tail` default no-op
  (§7). No change to `TurnContext` (control already threaded).
- Marker helper: a small `format_steer_marker` in the module that owns the
  bundled marker constants (reuse `compression_handoff`'s open/close), not a new
  paraphrase.

Explicitly not touched: prompt/tool-schema builders, `control_socket.rs`,
`relay_transport.rs` (`send_interrupt` stays a transport stub), any HTTP router
that is not `message.rs`, `PORT.md`, `INDEX.md`, and the AGY-lane files.

---

## 9. Focused test matrix

Registry (`turn_control.rs`, mostly present):
- FIFO accumulate + preview truncation at 60 code points (present at `:225`);
  add `Idle` on empty route, `Idle` when cancelled, `Empty` on whitespace-only.

SessionDb (`session_db.rs`) for `amend_native_tool_tail`:
- amend the newest tool row updates only `content`, leaves `message_count` /
  `tool_call_count` / `tool_call_id` / `timestamp` unchanged, and re-indexes FTS
  (assert an FTS MATCH finds the steered text and no longer finds only the
  pre-steer text), building on `fts_reflects_edits_and_deletes` (`:8439`).
- amend rejects (`Ok(false)`) when: lease holder mismatches; session `ended_at`
  set; newest active row is not the target tool row; `tool_call_id` mismatch.
- amend after an in-place compression that deactivated the target row rejects,
  no split.

Tool loop (`native_tools.rs`, fake ChatModel with `replace_tool_loop_tail`):
- steer buffered mid-batch lands one marker on the newest tool row at the
  boundary and calls the amend exactly once; byte-identical marker to the golden.
- structured (list) tool content gets a trailing text block, not string concat.
- two steers before the boundary join with `\n` into one marker, one amend.
- pre-API drain injects a steer that arrived during `model.step` on the next
  iteration even with no further tool batch; no double marker with the boundary
  drain (buffer emptied atomically).
- amend failure rolls back the in-memory marker and re-buffers (fail-closed).
- no steer means the messages vector and durable rows are byte-identical to the
  no-steer run (frozen prompt/schema).
- cancel wins over a buffered steer: loop breaks, no amend, no durable marker.

Ingress (`dispatch.rs` / `message.rs` scripted-agent harness):
- `/steer <text>` on a live turn returns `⏩ Steer queued \u2014 arrives after the
  next tool call: '<preview>'` and the marker appears in durable history.
- `/steer` empty args returns `Usage: /steer <prompt>` and does not touch the
  turn.
- unauthorized `/steer` on a gated platform returns denial and never buffers.
- busy `/steer` over synchronous HTTP returns the ack, not `409 session is busy`.
- leftover steer after a text-only turn: push promotes it to a next turn (or
  emits the interim notice); HTTP surfaces it and neither executes nor drops it.
- `/new` after a buffered steer: the steer cannot cross into the new turn.

---

## 10. Open items to confirm against the AGY oracle

- Exact `STEER_MARKER_OPEN` / `STEER_MARKER_CLOSE` bytes and the
  `format_steer_marker` `\n\n` prefix, plus the multimodal `lstrip` difference
  between the post-batch and pre-API branches. The Rust injection helper and its
  test must pin these from the golden, not paraphrase.
- The `/steer` preview boundary: Python `steer_text[:60]` on the cleaned
  (stripped) text; confirm the golden treats the count as code points so the
  Rust `chars().take(60)` matches for multibyte input (the CJK case in
  `turn_control.rs:236` already assumes code points).
- Whether the maintainers want fail-closed rollback (§3.5, recommended) or
  Python-style log-and-continue on a rejected durable amend.
- Whether leftover-steer push promotion (§5.1) is in scope for this checkpoint or
  deferred to the notice fallback, and whether `MessageResponse` gains a
  `pending_steer` field for HTTP parity (§5.2).
