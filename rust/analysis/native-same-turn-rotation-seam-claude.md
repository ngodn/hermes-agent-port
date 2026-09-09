# Native same-turn rotation seam (claude)

Rust-side seam analysis for enabling rotation-mode (`in_place = false`) full
compression inside the native tool loop, after a completed tool-result batch and
before the next provider request. Scope is the Rust production path only. The
authoritative Python behavior contract is owned by
`native-same-turn-rotation-contract-agy.md`; this document does not restate or
re-audit Python semantics. Read-only analysis, no source touched.

## Bottom line

The durable and in-memory machinery for a physical compression rotation already
exists and is proven by tests. It is wired only for the pre-turn path
(`automatic_compression::compress_before_turn`), which runs before the model
turn starts, while the admission layer still holds the `SessionEntry`, the turn
lease token, and the conversation-client cache handle.

The native same-turn full-compression pass
(`native_agent::full_compress_after_tool_batch`) runs deep inside the tool loop,
where none of those handles are reachable. It holds only a fixed
`session_id: &str` borrowed for the whole turn and a bare `SessionDb`. Today it
gates rotation off (`if !policy.enabled || !policy.in_place { return NotTriggered }`,
`native_agent.rs:1315`) and only ever commits in place.

The smallest deep seam that closes the gap is a single new turn-scoped object,
`ActiveTurnSession`, carried on `TurnContext`. It exposes a two-method interface
(`current()` read, `rotate(plan)` publish) and hides the whole publish-and-
propagate sequence: the atomic `SessionStore::publish_compression`, the turn-
lease rebind, the client-cache rekey, and the shared-cell handoff that lets both
the tool loop and dispatch pick up the child id for the rest of the turn. It
reuses every existing primitive and rebuilds nothing.

## The invariant that breaks

The native turn pins one session id for its entire duration. `TranscriptModel<'a>`
(`native_agent.rs:30-37`) holds `session_id: &'a str` as an immutable borrow, and
`run_native_turn` computes it once as a local `String`
(`session_id = message_session_id(msg)`, `native_agent.rs:1821`). A same-turn
rotation mints a child session id, so every reference below that assumes the id
is fixed goes stale the instant the child is published.

| # | Fixed parent-session reference | Location | Stale after rotation because |
|---|---|---|---|
| A | Tool-call / tool-result persistence | `TranscriptModel::persist_tool_loop_message` -> `append_native_tool_message(self.session_id, ...)` `native_agent.rs:121` | Post-boundary tool rows must land on the child transcript, not the closed parent |
| B | Subsequent same-turn compression / maintenance | `maintain_tool_loop_messages` -> `maintain_after_tool_batch(self.session_id, ...)` `native_agent.rs:144`, and every `session_id` read inside `full_compress_after_tool_batch` `native_agent.rs:1302-1576` | A second pass would read guard/snapshot/route for the parent |
| C | Durable transcript reload for the next provider request | `adopt_durable_tool_loop_transcript(database, session_id, ...)` `native_agent.rs:1559-1564` (and `:1410`, `:1729`) | The compacted transcript now lives under the child id |
| D | Next provider message assembly | in-memory `messages` in `run_model_turn` `native_agent.rs:1748-1810` | Request body is built from `messages`, not keyed by id, so the request itself is fine, but the reload feeding it (C) is not |
| E | Main provider usage accounting | `finalize_turn_after_persist` -> `record_main_usage(&message_session_id(msg), ...)` `native_agent.rs:2117-2124` | Post-boundary usage is attributable to the child |
| F | Memory finalization | `finalize_turn_after_persist` -> `host.turn_complete(...)` `native_agent.rs:2141`; boundary notify -> `host.session_switch(child, parent, ...)` `native_agent.rs:1997` | The memory session must switch to the child before `turn_complete` lands |
| G | Final assistant persistence | `dispatch::run_admitted_turn` -> `end_turn(turn_db, manages, &msg, &reply)` `dispatch.rs:678` (reads `message_session_id(&msg)`) | The assistant reply must be appended to the child transcript |
| H | Turn lease ownership / release | acquired `lease.acquire(&session_id, ...)` `dispatch.rs:557-564`, released on token drop at end of `run_admitted_turn` | Release and next-turn keying must follow the child; a concurrent inbound on the child id must contend on the same lock |
| I | Conversation-client cache key | `ConversationAgent` cache keyed `(home, message_session_id(msg))` `conversation_agent.rs:108-113` | The frozen client that ran the turn is checked out under the parent key; the child must reuse the same `Arc<dyn AgentClient>`, not rebuild it |
| J | Routing index / current-session swap | `SessionStore::index: Mutex<RoutingIndex>` `session_store.rs:17` | The route must atomically start naming the child; native currently writes durable rows via `SessionDb` directly and never touches the index |

References that do NOT go stale and need no change:

- `cache_scope` (`native_agent.rs:1822-1828`) is derived from
  `compression_lineage_root(&session_id)`. The child's lineage root equals the
  parent's root (`parent_session_id` chain, `session_db.rs:464`, `:1381`), so the
  prompt-cache scope is stable across rotation by construction.
- Routing activity update `store.update_session(&key, ...)` (`dispatch.rs:692-694`)
  is keyed by the route `session_key`, not the session id, so it is unaffected.

## What a rotation must touch, and what already exists

The pre-turn path already composes the exact sequence a same-turn rotation needs.
The primitives are proven and can be reused verbatim:

1. Atomic publish of the child, CAS-fenced, durable split plus in-memory index
   swap under one lock: `SessionStore::publish_compression`
   (`session_store.rs:274-325`). It mints the child via
   `SessionEntry::compression_candidate`, calls
   `SessionDb::publish_gateway_compression` (`session_db.rs:2021-2135`, one
   `IMMEDIATE` transaction: byte-compares the parent snapshot, inserts the child
   `sessions` row with `parent_session_id = parent`, writes the child transcript,
   upserts `gateway_routing` to the child, closes the parent with
   `end_reason = 'compression'`), and only on durable commit does
   `index.entries.insert(key, candidate)` (`session_store.rs:315`). Returns
   `ExplicitSessionSwitch { entry, predecessor_id }` or `None` on drift / CAS loss.

2. Turn-lease rebind: `SessionTurnLeaseRegistry::rebind`
   (`turn_lease.rs:183-213`) aliases the same `Arc<AsyncMutex<()>>` under the
   child id and rewrites `token.session_id`. Returns `false` if the child id
   already holds a distinct live lease.

3. Client-cache rekey plus memory switch:
   `AgentClient::notify_compression_boundary(ctx, parent, child, false)` ->
   `ConversationAgent::finish_compression_boundary`
   (`conversation_agent.rs:162-215`) removes the entry under the parent key and
   re-inserts the same `CacheEntry` (same `Arc<ClientCell>`, same
   `Arc<dyn AgentClient>`, so the frozen prompt / tool snapshot / extension host /
   `compression_count` all survive) under the child key. On notify failure it
   retires the stale client instead of rekeying, so the next turn rebuilds
   cleanly. The inner `NativeAgentClient::notify_compression_boundary`
   (`native_agent.rs:1978-2015`) drives `host.session_switch` and the fire-and-
   forget `session:compress` hook.

4. Predecessor cleanup: `agent.retire_conversation(ctx, parent)`
   (matches `dispatch.rs:535-543`).

The reference driver is `automatic_compression::compress_before_turn`, rotation
branch `automatic_compression.rs:1127-1161`: `store.publish_compression` ->
`transcript_leases.rebind(lease, &child)` -> `notify_compression_boundary(.., false)`
-> `admitted.entry = published.entry`. Same-turn rotation needs the same four
steps, just triggered from inside the tool loop instead of before it.

The two hard facts that block calling that sequence directly from the tool loop:

- The tool loop can only reach a `SessionDb`. `SessionStore`, the lease token, and
  the `ConversationAgent` cache all live in the dispatch / admission layer above
  `NativeAgentClient`. The native in-place path even does its durable write
  straight through `SessionDb::publish_gateway_in_place_compression`
  (`native_agent.rs:1521-1533`), bypassing the store, which is safe only because
  the id does not change.
- `notify_compression_boundary` must run against the OUTER caching agent
  (`ConversationAgent`) to rekey the cache. The in-place path today calls it on
  `self` (the inner `NativeAgentClient`, `native_agent.rs:1538`), which is fine
  for an unchanged key but cannot rekey the cache for a rotation.

## Seam options compared

### Option A: push store, lease, and cache handles down into the client

Give `NativeAgentClient` / `TranscriptModel` an `Arc<SessionStore>`, a shared
lease token, the `SessionSource`, and an `Arc<dyn AgentClient>` to the outer
caching agent, and let `full_compress_after_tool_batch` run the four-step
sequence itself.

- Interface cost: large. The tool loop's dependency surface explodes from
  `(SessionDb, &str)` to five collaborators, most of which are layered above it.
  It inverts the current layering (dispatch owns store and cache; the client
  would now reach back up).
- Depth: shallow. The new surface is nearly as wide as the work it does.
- Testability: poor. Every native turn test would need to stand up a store, a
  lease registry, and a caching agent.
- Rejected.

### Option B (selected): inject one turn-scoped `ActiveTurnSession` via `TurnContext`

Introduce a single turn-scoped object that owns the current session id in a
shared cell and hides the whole publish-and-propagate sequence behind two
methods. It is constructed by dispatch's `run_admitted_turn` (which already holds
the store, the lease token, the source, and the caching agent), cloned into the
agent task, and threaded through the existing `TurnContext`.

- Interface the tool loop sees: `current()` and `rotate(plan)`. Two methods.
- Depth: deep. A small interface hides `publish_compression` plus lease rebind
  plus cache rekey plus the shared-cell handoff.
- Layering: correct. The authority stays in the layer that already owns those
  collaborators; the tool loop only reads an id and requests a rotation.
- Propagation: the shared cell is read by both the tool loop (during the turn)
  and dispatch (after the turn), so the child id reaches every stale reference in
  the table without any call site learning the mechanics.
- Selected.

### Option C: durable-first, reconcile after the turn

Let native write the child durably through `SessionDb::publish_gateway_compression`
directly, then have dispatch detect the changed id after the turn and fix up the
index, lease, and cache.

- Rejected. It opens a window where the durable child holds the live transcript
  while the in-memory index still names the parent, so cross-process readers and
  any concurrent turn diverge. The index swap must be atomic with the durable
  commit under the `SessionStore` mutex (`session_store.rs:293-315`), and native
  has no CAS generation token to fence it. Reconstructing routing state after the
  fact is exactly what `publish_compression` exists to prevent.

## Selected seam: `ActiveTurnSession`

One new type. The tool loop and dispatch share an `Arc<ActiveTurnSession>`; the
child id is published into a cell that both sides read.

### Interface (type sketch)

```rust
// new module: crate::active_turn_session

/// Durable inputs for a same-turn compression rotation. All borrowed for the
/// duration of the call; nothing is retained past `rotate`.
pub struct RotationPlan<'a> {
    /// Byte-for-byte snapshot the CAS in publish_gateway_compression checks.
    pub original_messages: &'a [crate::session_db::CompressionHistoryMessage],
    /// Child transcript rows: summary carriers plus retained tail. This is the
    /// same `replacement` native already builds via
    /// compression_handoff::plan_replacement for the in-place path.
    pub rows: &'a [crate::session_db::CompressionReplacementRow],
    /// Cross-process durable-lease holder token, if any.
    pub turn_lease_holder: Option<&'a str>,
}

/// Outcome of a committed rotation.
pub struct RotationCommit {
    pub child_session_id: String,
}

pub struct ActiveTurnSession {
    /// The current session id for the turn. Starts as the parent, becomes the
    /// child once a rotation commits. Read by the tool loop on every persistence
    /// op and by dispatch after the turn.
    current: arc_swap::ArcSwap<str>,     // or Mutex<String>
    /// Set once, on a committed rotation, so dispatch can write it back into the
    /// owned Message before end_turn / finalize.
    rotated_child: std::sync::Mutex<Option<String>>,
    /// The publish-and-propagate authority. None on paths with no store/cache
    /// (tests, stateless backends), in which case rotate is a no-op.
    authority: Option<RotationAuthority>,
}

impl ActiveTurnSession {
    /// Cheap read used at every persistence site and by dispatch post-turn.
    pub fn current(&self) -> std::sync::Arc<str> { self.current.load_full() }

    /// The child id if a rotation committed this turn, else None.
    pub fn rotated_child(&self) -> Option<String> {
        self.rotated_child.lock().unwrap().clone()
    }

    /// Atomically publish a compression child and propagate its identity across
    /// the routing index, the turn lease, and the client cache. Returns Ok(true)
    /// on a committed rotation (current() now returns the child), Ok(false) if
    /// the rotation was not published (drift, CAS loss, no authority).
    pub async fn rotate(&self, plan: &RotationPlan<'_>) -> Result<bool>;
}

/// Lives in the dispatch/admission layer. Never seen by the tool loop.
struct RotationAuthority {
    store: std::sync::Arc<crate::session_store::SessionStore>,
    source: crate::session::SessionSource,
    /// The OUTER caching agent, so notify_compression_boundary rekeys the cache.
    agent: std::sync::Arc<dyn crate::agent::AgentClient>,
    /// Shared with dispatch so rotate can rebind at publish time.
    lease: std::sync::Arc<std::sync::Mutex<Option<crate::turn_lease::TurnLeaseToken>>>,
    database: std::sync::Arc<crate::session_db::SessionDb>,
    finalizable: bool,
}
```

`TurnContext` gains one optional borrow, matching its existing shape
(`agent.rs:37-47`):

```rust
pub struct TurnContext<'a> {
    // ...existing fields...
    pub active_session: Option<&'a ActiveTurnSession>,
}
impl<'a> TurnContext<'a> {
    pub fn with_active_session(mut self, active: &'a ActiveTurnSession) -> Self {
        self.active_session = Some(active); self
    }
}
```

### How it threads through the turn

- `TranscriptModel<'a>` drops `session_id: &'a str` in favor of
  `active: &'a ActiveTurnSession`. `persist_tool_loop_message` and
  `maintain_tool_loop_messages` read `self.active.current()` instead of
  `self.session_id`. Reference B (repeated compression passes) and reference A
  (tool persistence) now follow the child automatically.
- `full_compress_after_tool_batch` takes `&ActiveTurnSession` instead of
  `session_id: &str`. All its reads use `active.current()`. The in-place gate at
  `native_agent.rs:1315` becomes a branch:
  - `policy.in_place`: unchanged path (`publish_gateway_in_place_compression` on
    the same id, `notify_compression_boundary(self, id, id, true)`, adopt).
  - rotation: build the same `replacement` rows, then
    `active.rotate(&RotationPlan { original_messages: &snapshot.messages, rows: &replacement, turn_lease_holder })`.
    On `Ok(true)`, `adopt_durable_tool_loop_transcript(database, &active.current(), ...)`
    (now the child) and set `awaiting_usage`. On `Ok(false)`, refund the attempt
    exactly like the existing `!published` branch (`native_agent.rs:1534-1537`).
- Dispatch (`run_admitted_turn`) constructs the `Arc<ActiveTurnSession>` after it
  has the resolved id, the lease token, the source, and `self.agent`. It moves
  the token into the shared `Mutex`, clones the `Arc` into the agent task, and
  keeps a clone for itself. The agent task builds its `TurnContext` with
  `.with_active_session(&active)` (inside the spawned block, mirroring how
  `agent_db` is moved in today, `dispatch.rs:625-636`).
- After `agent_task.await` and before `end_turn`
  (`dispatch.rs:678`), dispatch does:

  ```rust
  if let Some(child) = active.rotated_child() {
      msg.resolved_session_id = Some(child);
  }
  ```

  `msg` is an owned local in `run_admitted_turn`, so this one write makes every
  downstream `message_session_id(&msg)` reader follow the child with no other
  change: `end_turn` (reference G), `record_main_usage` inside
  `finalize_turn_after_persist` (reference E), and the memory `turn_complete`
  path (reference F). The lease (reference H) was already rebound inside
  `rotate`, so its drop at the end of `run_admitted_turn` releases the child id.
  The index swap (reference J) happened inside `publish_compression`. The cache
  rekey (reference I) happened inside `notify_compression_boundary`.

### `rotate` implementation and ordering constraints

The order must match the proven pre-turn sequence
(`automatic_compression.rs:1127-1161`). Inside `rotate`, with `authority` present:

1. Re-fetch the current `SessionEntry` for CAS:
   `entry = store.current_entry_for_source(&source)` (`session_store.rs:69`).
   This yields a fresh object-generation token; native has none of its own.
2. `published = store.publish_compression(&source, &entry, plan.original_messages, plan.rows, plan.turn_lease_holder)?`.
   This is the single atomic step: durable split plus in-memory index swap under
   the store mutex, CAS-fenced on `entry`. If `None`, return `Ok(false)` (drift
   or CAS loss); the caller refunds the attempt and stays on the parent.
3. Publish the child id to readers:
   `self.current.store(published.entry.session_id)`. This happens after the
   durable+index commit, so no reader ever observes an uncommitted child. It is
   safe to do before steps 4 and 5 because `rotate` is awaited synchronously
   inside `maintain_tool_loop_messages`; no concurrent persistence runs on this
   turn between provider steps.
4. Rebind the lease before notifying, under the shared mutex:
   `lease.lock().as_mut().map(|tok| leases.rebind(tok, &child))`. If it returns
   `false`, log and proceed (see failure modes). Doing this at publish time,
   not post-turn, closes the window in which a concurrent inbound on the child id
   could acquire an unaliased lease and run against the tail of this turn
   (reference H).
5. `agent.notify_compression_boundary(ctx, &parent, &child, false).await` on the
   OUTER caching agent. This rekeys the cache (reference I) and switches the
   memory session (reference F, `host.session_switch(child, parent, ...)`).
6. `agent.retire_conversation(ctx, &parent)` for predecessor cleanup.
7. Record `rotated_child = Some(child)`, return `Ok(true)`.

Ordering invariants, stated plainly:

- Durable commit and index swap are one atomic unit (inside `publish_compression`).
  Nothing else may run between them; that is why native cannot do the durable
  write on its own `SessionDb`.
- The shared-cell update (step 3) must not precede the commit (step 2).
- The lease rebind (step 4) must precede any release and must precede the point
  where a concurrent turn could observe the child in the index. Publishing at
  step 2 makes the child visible to `refresh_current_entry_from_database`
  readers, so step 4 should follow step 2 as tightly as possible.
- `notify_compression_boundary` (step 5) must run against the caching agent, not
  the inner native client, or the cache is never rekeyed.
- The dispatch write-back to `msg.resolved_session_id` must happen after the
  agent task joins and before `end_turn`.

### Failure modes

- `publish_compression` returns `None` (snapshot drift, CAS loss, route moved):
  `rotate` returns `Ok(false)`. Caller refunds the compression attempt
  (`refund_compression_attempt`, `native_agent.rs:1294-1300`) and continues on
  the parent id. No stale state; identical to the existing in-place
  `!published` path.
- `publish_compression` returns `Err` (SQLite fault): fail open. Log, return
  `Ok(false)`, keep the parent. The native path already prefers fail-open for
  same-turn maintenance (empty-summary and summary-failure both return
  `Attempted` without aborting the turn, `native_agent.rs:1478-1502`).
- `rebind` returns `false` (child id already holds a distinct live lease): the
  lease stays aliased to the parent. The turn still completes and the child
  transcript is correct, but the durable turn-lease release keys the parent. This
  is the one place the lease and the index can diverge; it matches the existing
  pre-turn behavior, which only logs (`automatic_compression.rs:1142-1146`). Log
  it and note it as a known tolerated divergence.
- `notify_compression_boundary` returns `Err`: `finish_compression_boundary`
  retires the stale client instead of rekeying
  (`conversation_agent.rs:186-194`). Correctness holds; the next turn rebuilds
  the frozen client under the child key at the cost of one prompt rebuild and one
  cache miss. The fire-and-forget `session:compress` hook is unaffected (it is
  spawned, `native_agent.rs:2007-2012`).
- Agent task panics after the cell is updated but before dispatch write-back: the
  durable child exists and the index names it, so the next inbound resolves the
  child correctly. This turn's assistant reply is lost because `end_turn` never
  ran, which is the same failure surface as any mid-turn panic today.
- No `authority` (tests, stateless backends, no store): `rotate` returns
  `Ok(false)` immediately, `current()` never changes. This preserves today's
  behavior for every path that does not carry a `SessionStore`.

## Implementation sequence

1. Add the `active_turn_session` module with `ActiveTurnSession`, `RotationPlan`,
   `RotationCommit`, and the private `RotationAuthority`. Start with a no-authority
   constructor so `current()` works and `rotate` is a no-op.
2. Add `active_session: Option<&'a ActiveTurnSession>` and `with_active_session`
   to `TurnContext` (`agent.rs:37-85`).
3. Swap `TranscriptModel.session_id: &'a str` for `active: &'a ActiveTurnSession`
   and route `persist_tool_loop_message` / `maintain_tool_loop_messages` through
   `active.current()` (`native_agent.rs:34, 121, 144`). Thread `active` from
   `run_model_turn` and `run_native_turn` off `context.active_session`, falling
   back to a single-id `ActiveTurnSession::fixed(session_id)` when absent so the
   no-tools branch and existing callers are unchanged.
4. Branch `full_compress_after_tool_batch` on `policy.in_place`: keep the in-place
   commit, add the rotation arm that calls `active.rotate(&plan)` and adopts from
   `active.current()`. Reuse the `replacement` rows already computed at
   `native_agent.rs:1513-1520`.
5. Implement `RotationAuthority` and `ActiveTurnSession::rotate` with the step
   sequence above, reusing `publish_compression`, `rebind`,
   `notify_compression_boundary`, and `retire_conversation`.
6. In `dispatch::run_admitted_turn`, construct the `Arc<ActiveTurnSession>` with
   the authority, move the lease token into the shared `Mutex`, clone the `Arc`
   into the agent task, add `.with_active_session(&active)`, and after the join
   write `msg.resolved_session_id = active.rotated_child()` before `end_turn`
   (`dispatch.rs:557-691`).
7. Remove the hard `!policy.in_place` short-circuit at `native_agent.rs:1315`;
   gate rotation on `authority.is_some()` at the `rotate` call so a policy with
   `in_place = false` but no store still fails safe.

## Test sequence

Unit, on `ActiveTurnSession` in isolation with a fake authority:

1. `current()` returns the parent until `rotate` commits, then the child.
2. `rotate` with no authority returns `Ok(false)` and leaves `current()` fixed.
3. `rotate` on a `publish_compression` `None` returns `Ok(false)`, `current()`
   unchanged, `rotated_child()` is `None`.
4. `rotate` on commit: `current()` and `rotated_child()` both become the child;
   assert the fake authority observed publish -> rebind -> notify(false) ->
   retire in that order.
5. `rebind` false path: `rotate` still returns `Ok(true)` and logs; token id
   stays parent (assert the divergence is the expected one).
6. `notify_compression_boundary` `Err` path: `rotate` returns `Ok(true)`; assert
   the retire-instead-of-rekey outcome via the fake caching agent.

Integration, native tool loop plus a real `SessionDb` and `SessionStore`
(extend the existing dispatch rotation harness at `dispatch.rs:1357-1574`, which
already asserts a parent -> child swap in `current_entry_for_source`):

7. A tool-batch turn that crosses the rotation threshold commits the child:
   post-turn `store.current_entry_for_source(&source)` names the child, the child
   transcript holds the summary plus retained tail, and the parent is closed with
   `end_reason = 'compression'`.
8. Post-boundary tool rows and the final assistant reply land on the child, not
   the parent (query both transcripts).
9. Main provider usage recorded after the boundary attaches to the child
   (`record_main_usage`, reference E).
10. The frozen client is reused, not rebuilt: assert the cache entry moved keys
    and the `compression_count` `AtomicU32` carried over (reuse the pattern in
    `conversation_agent.rs:1353-1388`).
11. The turn lease releases against the child: after the turn, a fresh inbound on
    the child id acquires without contending on the parent.
12. `in_place = true` regression: the existing in-place path is byte-identical,
    no index swap, `rotated_child()` is `None`.

Golden parity: drive the rotation decision and adoption against the Python oracle
corpus named in `native-same-turn-rotation-contract-agy.md`
(`adopt_legacy_rotation_clears_baseline` is the rotation adoption oracle;
`same-turn-full-compression-goldens.json` covers the trigger and compress state
machine). The seam does not change the decision, so those goldens should stay
green; the new coverage is the identity propagation, which the integration tests
above own.

## Open questions and parity notes

- `awaiting_usage` and attempt-refund semantics on a committed rotation should
  mirror the in-place commit (`native_agent.rs:1565-1567`); confirm against the
  contract whether the child inherits the parent's attempt counter or resets it.
  The Rust attempt state lives on `SameTurnCompressionState` inside
  `TranscriptModel`, which is per-turn, so it naturally carries across the
  boundary within the same turn.
- The contract owns whether post-rotation usage attaches to the parent or the
  child; this design attaches it to the child (write-back before `finalize`),
  which matches the pre-turn rotation. Verify that is the required parity.
- `retire_conversation(parent)` is included for symmetry with the pre-turn path
  (`dispatch.rs:543`). Confirm it is desired mid-turn, since the parent client
  was just rekeyed to the child; retiring the parent key should be a no-op after
  a successful rekey but must not evict the live child entry.
