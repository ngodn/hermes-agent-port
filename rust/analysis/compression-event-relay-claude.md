# `session:compress` event / plugin-hook / relay audit at a completed full-compression boundary

Scope: the **generic `session:compress` event**, its plugin-hook dispatch, and any relay/status
notification tied to a completed full-compression boundary. This is deliberately *not* the
context-engine adoption audit nor the memory-manager `on_session_switch` audit; those are the
sibling task (`compression-context-engine-boundary-agy.md`) and are covered here only as ordering
neighbours. No production code or tests were modified. All line numbers are current as of this
commit (`be861c43da`, branch `rust-rewrite`).

Legend: **[VERIFIED]** read directly from source · **[INFERRED]** design conclusion from the
evidence · **[DORMANT]** present but unreachable/unused in the shipped path · **[OPEN]** needs
confirmation.

---

## 1. Authoritative emission site (Python)

**[VERIFIED]** The generic event is emitted from exactly one place in the generic agent loop:
`agent/conversation_compression.py:5573-5583`.

```python
# 5569  # Emit session:compress event so hooks (e.g. MemPalace sync) can ingest
# 5570  # the completed old session before its details are lost. In in-place mode
# 5571  # there is no old id (same session); ``in_place=True`` tells hooks the
# 5572  # transcript was compacted on the same id rather than rotated.
5573  if getattr(agent, "event_callback", None):
5574      try:
5575          agent.event_callback("session:compress", {
5576              "platform": agent.platform or "",
5577              "session_id": agent.session_id,
5578              "old_session_id": _old_sid or "",
5579              "in_place": in_place,
5580              "compression_count": agent.context_compressor.compression_count,
5581          })
5582      except Exception as e:
5583          logger.debug("event_callback error on session:compress: %s", e)
```

There is a **second, separate emitter** for the Codex app-server runtime at
`agent/codex_runtime.py:317-336`. It is a different runtime lane (not the generic loop) and emits a
superset payload (`runtime="codex_app_server"`, `thread_id`, `turn_id`, always
`old_session_id=""`, `in_place=False`). It is out of the generic scope but noted so a Rust port does
not accidentally treat the two shapes as one. **[VERIFIED]**

### 1.1 Exact payload, rotation vs in-place

**[VERIFIED]** The dict has exactly five keys in the generic path:

| key | rotation value | in-place value | source |
|---|---|---|---|
| `platform` | `agent.platform or ""` | same | `run_agent.py` sets `self.platform`; empty string when unset |
| `session_id` | the **new/child** id (`agent.session_id` after re-point) | the **unchanged** current id | `agent.session_id` at 5577 |
| `old_session_id` | the parent id (`_old_sid`) | **`""`** (empty) | `_old_sid or ""` at 5578 |
| `in_place` | `False` | `True` | the `in_place` flag at 5579 |
| `compression_count` | compressor in-memory counter | same | 5580 |

Critical asymmetry vs the neighbour notifications: the event uses **`_old_sid or ""`** (5578),
whereas the context-engine and memory notifications above use **`_boundary_parent`**
(`agent/conversation_compression.py:5490`), which in in-place mode is the *current* id, not empty.
So in in-place mode the event reports `old_session_id=""` while the memory/context-engine paths pass
the same non-empty id as parent. A Rust port must not "fix up" in-place `old_session_id` to the
current id; the frozen contract is empty string. **[VERIFIED]**

`session_id` may be `None` if `agent.session_id` is unset (it is passed straight through, not
coalesced to `""` like `old_session_id`). **[VERIFIED]** In the tested rotation path it is always the
new child id (`tests/run_agent/test_compression_boundary_hook.py:311`). **[OPEN]** whether a
`None` `session_id` can actually reach this line in production (rotation always sets a child id
first; in-place keeps the existing id). Treat `None` as a theoretical edge.

### 1.2 `compression_count` semantics

**[VERIFIED]** `compression_count` is a plain in-memory integer on the compressor:
- initialised `self.compression_count = 0` at `agent/context_compressor.py:3610`
- incremented `self.compression_count += 1` at `agent/context_compressor.py:8814`, **inside
  `compress()`**, i.e. when the summary is produced - *before* the SQLite split runs.
- It is process-lifetime only. It is **not** the persisted `_ineffective_compression_count`
  (a different counter, `context_compressor.py:2363` etc.). No per-session durable persistence of
  `compression_count` was found. **[VERIFIED]**

Consequence **[INFERRED]**: because the counter advances in `compress()` before the durable split,
a rotation that later fails and rolls back (§3.2) still emits an *incremented* `compression_count`.
The number is "attempts that produced a summary", not "durable compactions". A Rust port that keys
the count off a durable session column would diverge from Python here. **[OPEN]** whether that
divergence matters to any real subscriber - no shipped subscriber reads the field (§4).

---

## 2. Ordering relative to durable SQLite publication and the neighbour notifications

**[VERIFIED]** Within `_compress_context`'s success continuation, the order is fixed and linear:

1. **Durable SQLite publication** - rotation publishes prompt+compacted handoff atomically; in-place
   runs `update_system_prompt` - ending at `_session_commit_succeeded = True`
   (`conversation_compression.py:5364`). The split-failure handler is `5365-5479`.
2. **Boundary bookkeeping** computed once: `_old_sid`, `_is_boundary`,
   `_context_engine_boundary_committed`, `_boundary_parent` (`5485-5490`).
3. **Parent activity-label cleanup**, gated on `_old_sid and _session_commit_succeeded`
   (`5500-5515`).
4. **Context-engine notification** (deferred or immediate), gated on
   `_context_engine_boundary_committed` (`5523-5535`). *Sibling audit's scope.*
5. **Memory-manager `on_session_switch`**, gated on `_is_boundary and agent._memory_manager`,
   `reset=False`, `reason="compression"` (`5543-5552`). *Sibling audit's scope.*
6. **Repeated-compression warning/status** - `_emit_status` + stored `_compression_warning`, gated
   on `compression_count >= 2` (`5560-5567`).
7. **`session:compress` event** (`5573-5583`).
8. Post-event state: `_last_compaction_in_place` flags, usage-anchor invalidation,
   `record_completed_compaction`, file/skill dedup resets (`5589-5645`).

So the event is emitted **strictly after** durable publication, after context-engine adoption, after
memory switching, and after the warning/status. Any subscriber therefore observes a state where the
SQLite rows, the context engine and the memory providers already reflect the new boundary.
**[VERIFIED]**

### 2.1 Gating asymmetry (verified, important for parity)

**[VERIFIED]** The event's only guard is `getattr(agent, "event_callback", None)` (5573). It is
**not** gated on `_session_commit_succeeded`, `_context_engine_boundary_committed`, `_is_boundary`,
or `_compression_made_progress`. The block at `5481+` is reached by fall-through after the
`5365-5479` `except` handler (which catches and does not re-raise). Therefore:

- On a **committed** rotation/in-place: event fires with the real ids. Expected.
- On a **caught, rolled-back** split (`5365-5479`): the neighbour notifications (steps 3-5) are
  skipped because their `_session_commit_succeeded` / `_context_engine_boundary_committed` guards are
  false, **but the event still fires** - with `_old_sid` cleared to `None` on aborted rotation
  (`5458` clears `old_session_id`), i.e. `old_session_id=""`, `in_place=False`, `session_id=` the
  parent it rolled back to, and an already-incremented `compression_count`. **[VERIFIED / INFERRED]**
  This is a genuine behavioural edge: the event can announce a "compression" that produced no
  durable change. **[OPEN]** whether this is intended or latent; no test exercises the rollback
  emission (the shipped test only covers the happy rotation path,
  `test_compression_boundary_hook.py:290-313`).

---

## 3. Transport: gateway hook bridge → HookRegistry → subprocess handlers

### 3.1 The sync→async bridge

**[VERIFIED]** In the gateway, `agent.event_callback` is bound to
`TurnRunner._event_callback_sync` (`gateway/run.py:6458` binds `ctx._event_callback_sync`;
`gateway/run.py:31861` republishes it onto `turn_ctx`). The bridge:

```python
# gateway/run.py:5831
def _event_callback_sync(self, event_type: str, context: dict) -> None:
    ctx = self._ctx
    try:
        asyncio.run_coroutine_threadsafe(
            ctx._hooks_ref.emit(event_type, context),
            ctx._loop_for_step,
        )
    except Exception as _e:
        logger.debug("event_callback hook error: %s", _e)
```

**[VERIFIED]** Async/error isolation, three layers:
1. Emission site wraps the call in `try/except → logger.debug` (`conversation_compression.py:5582`).
2. The bridge is **fire-and-forget**: `run_coroutine_threadsafe` returns a `Future` that is
   **discarded** - it is never awaited or `.result()`-ed. The compression turn (running off the
   event loop, hence the threadsafe hop onto `ctx._loop_for_step`) does not block on hook completion
   and cannot see hook exceptions. A scheduling error is swallowed at debug.
3. `HookRegistry.emit` catches per-handler exceptions (`gateway/hooks.py:199-200`).

So a slow or throwing hook cannot delay or fail the compression, and cannot back-pressure the turn.
**[VERIFIED]**

### 3.2 HookRegistry dispatch

**[VERIFIED]** `gateway/hooks.py:177-200`. `emit` resolves handlers via `_resolve_handlers`
(`164-175`): exact `session:compress` matches first, then `session:*` wildcard matches. Each handler
is called `fn(event_type, context)`; coroutine results are awaited; exceptions are printed and
swallowed per-handler. Return values are discarded (this is `emit`, not `emit_collect`).

Handlers are discovered from a hooks directory: each `<hook>/HOOK.yaml` (`name`, `events`) +
`handler.py` with a top-level `handle` (`gateway/hooks.py:83-162`). Registration is per declared
event (`149-150`).

### 3.3 Who actually consumes `session:compress`

**[VERIFIED]** No shipped hook subscribes:
- `grep -rln "session:compress|session:\*" --include=*.yaml .` → **no matches**.
- No `plugins/` code references `session:compress`.
- The only in-tree references are the emitter, the codex emitter, the gateway bridge comment
  (`gateway/run.py:31859`), and the test.

**[INFERRED]** `session:compress` is a pure **extension point**. The comment names "MemPalace sync"
(`conversation_compression.py:5569`) as the intended external subscriber, delivered as a user/plugin
hook directory. There is no built-in consumer, and the memory-manager refresh is a *separate direct
call* (step 5), not driven by this event.

### 3.4 Relay / websocket

**[VERIFIED]** The event is **never** delivered to the relay or over the websocket. The only relay
touchpoints for compression are unrelated:
- `gateway/relay/command_manifest.py:96` and `ws_transport.py:190` expose a `/compress` *command*
  (user-initiated), not the completion event.
- The **warning/status** (step 6) is what reaches TUI/Telegram/Discord/relay, via
  `_emit_status` → `status_callback` → `adapter.send`, and via the stored `_compression_warning`
  replayed by `replay_compression_warning` (`run_agent.py:1479-1482`) once a late-bound
  `status_callback` is wired. That is a distinct channel from `event_callback`. **[VERIFIED]**

So "relay notification behavior" for a completed compression boundary is limited to the
repeated-compression warning; the `session:compress` event itself has no relay path. **[VERIFIED]**

---

## 4. Current Rust gateway surfaces

### 4.1 A HookRegistry already exists, unwired

**[VERIFIED]** `rust/crates/hermes-gateway/src/hooks.rs` is a faithful port of the Python
`HookRegistry`: `discover_and_load[_from]` (`59-133`), `resolve_handlers` with `base:*` wildcard
(`135-146`), `emit` (`149-158`) and `emit_collect` (`160-`). It spawns each handler as a
**subprocess**, passing the event type as `argv[1]` and `HERMES_HOOK_EVENT`, and the context as JSON
(module doc `hooks.rs:12-23`; `run_handler` around `235-242`). Errors are logged via `tracing::warn!`
and never propagated (`152-153`).

**[VERIFIED / DORMANT]** It has **no callers**. The module header states: *"Public API is ahead of
its callers (the lifecycle emit sites wire it)"* (`hooks.rs:3`). A workspace grep for `.emit(`
/ `HookRegistry` / `hook_registry` finds only the `think_scrubber` false positives - no gateway
emit site, and specifically **no `session:compress`** is emitted anywhere in Rust. The generic
event is currently **unported**.

### 4.2 The boundary seam that already exists in Rust

**[VERIFIED]** The completed-compression boundary is handled in
`rust/crates/hermes-gateway/src/automatic_compression.rs`:
- in-place: `publish_in_place_compression` (durable commit) then
  `notify_compression_boundary(context, &session_id, &session_id, true)` (`1108-1125`).
- rotation: `publish_compression` (durable commit), lease rebind, then
  `notify_compression_boundary(context, &session_id, &published.entry.session_id, false)`
  (`1126-1164`).

`notify_compression_boundary` (`native_agent.rs:1939-1961`) validates the ids and, if an extension
host exists, calls `host.session_switch(new, old, false, false, "compression")`
(`native_agent.rs:1959`). Its failure is **best-effort post-commit**: redacted and logged, never
failing the compression (`automatic_compression.rs:1122-1124`, `1157-1159`). This is the
memory/context-engine boundary (sibling scope). **There is no `session:compress` emission here.**
**[VERIFIED]**

### 4.3 The persistent Python extension host has no event surface

**[VERIFIED]** `hermes_cli/rust_extension_host.py` dispatches exactly:
`initialize, snapshot, call_tool, turn_start, turn_complete, session_switch, session_end,
flush_pending, pre_compress, shutdown` (`626-649`). It owns **only** a `MemoryManager`
(`72`, and the `session_switch` handler at `395-447` just calls
`self._memory_manager.on_session_switch(...)`). It holds **no `HookRegistry`, no `event_callback`,
and never fires `session:compress`.** The frozen prompt/tool snapshot lives here
(`snapshot`/`turn_start` render prompt sections; `pre_compress` touches memory only). **[VERIFIED]**

The Rust client mirror (`rust/crates/hermes-gateway/src/extension_host.rs`) exposes
`initialize/snapshot/call_tool/turn_start/pre_compress/session_switch/turn_complete/close` and has
**no** event/hook method (`Client` impl `205-391`).

---

## 5. Smallest safe production seam

### 5.1 Recommendation - Rust-native emit through the existing HookRegistry (no Python host round-trip)

**[INFERRED, recommended]** The generic `session:compress` payload is pure metadata
(`platform`, `session_id`, `old_session_id`, `in_place`, `compression_count`) with **no dependency
on Python plugin state, the frozen prompt, or the tool snapshot**. Shipped hooks are already
subprocess executables under both runtimes, and `hooks.rs` already spawns them. Therefore the
minimal seam is:

- Emit `session:compress` from Rust via the already-built `HookRegistry::emit`, placed **immediately
  after** the `notify_compression_boundary` call in both branches of
  `automatic_compression.rs` (after durable publish + boundary notification, matching Python steps
  1→5→7). Fire-and-forget (`tokio::spawn` or just `.await` on a best-effort basis with errors
  logged), mirroring Python's discard-the-future isolation so a hook cannot fail or delay the
  compression.

This preserves the frozen prompt/tool snapshot **trivially**, because the event never crosses the
extension-host boundary and never asks Python to re-render anything. It also matches the Python
ordering guarantee (subscriber sees committed SQLite + adopted context engine + switched memory).

Open wiring choices **[OPEN]**:
- The registry must be discovered/loaded once at gateway start and shared (`Arc`) into the
  compression path. That is the "lifecycle emit site wiring" the `hooks.rs:3` comment anticipates.
- `compression_count` has no Rust equivalent (§1.2). Parity requires a per-conversation in-memory
  counter incremented when a summary is produced. Decide whether to match Python's
  "counts-even-on-rollback" semantics or a stricter "durable-only" count; the safe default for
  parity is to match Python (increment at summary production).
- `platform` must be resolved from the gateway source, not the extension host.

### 5.2 If parity requires the emission to be *observed by in-process Python plugins*

**[INFERRED]** Only if a real subscriber needs live in-process Python plugin state (none exists
today) would the extension host need to participate. In that case add a **new host method** rather
than overloading `session_switch` (which is semantically the memory/context boundary and is validated
to reject an empty `new_session_id` - incompatible with the event's empty `old_session_id` in-place
contract). See §6 for the contract. This is **not recommended** unless such a subscriber materialises.

---

## 6. Strict JSONL contract, only if the Python host must participate

**[INFERRED - conditional, not currently needed]** If a future in-process Python subscriber forces
host participation, the minimal request/response over the existing one-object-per-line stdin/stdout
protocol (`rust_extension_host.py:614-655`) would be a new method, e.g. `session_compress`, kept
distinct from `session_switch`:

Request:
```json
{"id": <u64>, "method": "session_compress", "params": {
  "platform": "<string, possibly empty>",
  "session_id": "<string|null: new/current id>",
  "old_session_id": "<string, EMPTY in in-place mode>",
  "in_place": <bool>,
  "compression_count": <int>
}}
```

Response (success): `{"id": <u64>, "ok": true, "result": null}` - the event is fire-and-forget on
the Python side; a **null** result must be required and any non-null rejected by the Rust caller
(the same discipline already applied to `session_switch`, `extension_host.rs:363-369`).

Response (failure): `{"id": <u64>, "ok": false, "error": "<string>"}` - surfaced as `Err` by
`request`; the caller must treat it as **best-effort** (log-and-continue), never failing the
compression, matching Python's `try/except → debug` isolation.

Constraints the contract must preserve (all **[VERIFIED]** from §1.1/§2):
- `old_session_id` is **empty** in in-place mode (do not substitute the current id).
- `session_id` is nullable.
- The call must run **after** durable publication and the memory/context switch, never before.
- Timeout should be short and bounded (event carries no model/prompt work); if added, keep it in the
  `turn_complete` (5s) / `session_switch` (15s) band, not the `pre_compress` (310s) band.

**[REJECTED]** Reusing `session_switch` to also carry the event - it validates `new_session_id`
nonempty and is the memory-boundary call; conflating the two would fire memory switching twice and
break the empty-`old_session_id` in-place contract.

---

## 7. Tests that would prove parity and post-commit semantics

**[INFERRED]** Focused unit tests (Rust, alongside `automatic_compression.rs` / `hooks.rs` styles):
1. **Rotation payload shape** - after a committed rotation, exactly one `session:compress` with
   `session_id=child`, `old_session_id=parent`, `in_place=false`, `platform` resolved,
   `compression_count` advanced. Mirrors `test_compression_boundary_hook.py:290-313`.
2. **In-place payload shape** - `session_id=current`, **`old_session_id=""`**, `in_place=true`.
   This is the asymmetry most likely to regress.
3. **Ordering** - a recording hook observes the durable rows already published and the memory
   `session_switch` already delivered when it fires (emit strictly after
   `notify_compression_boundary`).
4. **Isolation** - a handler that errors / hangs does not fail or delay the compression (fire-and-
   forget parity with §3.1).
5. **No-subscriber safety** - compression completes with an empty registry (parity with
   `test_compression_boundary_hook.py:315` `test_no_callback_is_safe`).
6. **Rollback edge** - decide and pin the intended behaviour when the split is caught/rolled back
   (§2.1): does Rust emit like Python, or suppress? Add a test either way so the choice is explicit.

**[INFERRED]** Live/integration test: run the gateway with a temp hooks dir containing a
`session:compress` handler that appends its argv/JSON to a file, drive a real full compression
(rotation and in-place), and assert the file shows exactly one event per boundary with the frozen
payload and that the prompt/tool snapshot returned by the host `snapshot` is byte-identical before
and after (proving the event did not perturb the frozen prompt/tools).

---

## 8. Rejected / unsafe approaches

- **[REJECTED]** Emit the event from inside `notify_compression_boundary` / the extension-host
  `session_switch` path. That couples the metadata event to the memory boundary, risks double memory
  switching, and cannot represent the in-place empty-`old_session_id` contract.
- **[REJECTED]** Await the hook future / block the turn on hook completion. Python is explicitly
  fire-and-forget (`run.py:5834`, future discarded); blocking would add back-pressure and let a bad
  hook wedge compression.
- **[REJECTED]** Route the event through the relay/websocket. No Python path does this; it would
  invent delivery semantics and leak internal session ids to clients.
- **[REJECTED]** Gate the Rust emit on durable-commit-success to "clean up" the Python rollback edge
  without first confirming intent (§2.1). Silently diverging from Python's not-gated behaviour is a
  parity risk; the divergence should be a deliberate, tested decision.
- **[REJECTED]** Derive `compression_count` from a durable session column. Python's value is an
  in-memory attempt counter that advances before commit; a durable-only count would under-report
  relative to Python (§1.2).

---

## 9. Open questions

1. **[OPEN]** Is the rollback-path emission (§2.1) intended, or latent? No test covers it. Rust
   parity choice hinges on this.
2. **[OPEN]** Can `session_id` legitimately be `None`/empty at the emit site in production, and must
   Rust preserve nullability or coalesce to `""`?
3. **[OPEN]** Is any real external subscriber (e.g. "MemPalace sync") expected in the Rust
   deployment? If none, the Rust-native `HookRegistry` seam (§5.1) is sufficient and the JSONL host
   contract (§6) is unnecessary.
4. **[OPEN]** Should Rust carry the exact `compression_count` semantics (attempt counter) or a
   stricter durable count? Depends on whether any subscriber reads the field (none does today).
5. **[OPEN]** Does the Codex-runtime emitter (`codex_runtime.py:317`) need porting in the Rust
   gateway, or is that runtime out of the Rust deployment entirely?

---

## Appendix - primary citations

- Emitter: `agent/conversation_compression.py:5573-5583` (payload), boundary bookkeeping
  `5481-5490`, gating neighbours `5500-5567`, split-failure handler `5365-5479`, commit flag `5364`.
- `compression_count`: `agent/context_compressor.py:3610` (init), `8814` (increment).
- Codex emitter: `agent/codex_runtime.py:317-336`.
- Gateway bridge: `gateway/run.py:5831-5839`, `6458`, `31858-31861`.
- HookRegistry (Py): `gateway/hooks.py:83-200`.
- Warning/status path: `agent/conversation_compression.py:5560-5567`; `run_agent.py:1479-1482`.
- Relay command (unrelated): `gateway/relay/command_manifest.py:96`; `ws_transport.py:190`.
- Test: `tests/run_agent/test_compression_boundary_hook.py:256-324`.
- HookRegistry (Rust, dormant): `rust/crates/hermes-gateway/src/hooks.rs:3, 59-158`.
- Rust boundary seam: `rust/crates/hermes-gateway/src/automatic_compression.rs:1108-1164`;
  `native_agent.rs:1939-1961`.
- Extension host (Py) dispatch: `hermes_cli/rust_extension_host.py:626-649`, `395-447`.
- Extension host (Rust) client: `rust/crates/hermes-gateway/src/extension_host.rs:205-391`.
