# Conversation lifecycle wiring: bounded cached conversations and graceful teardown

Independent audit. No implementation files were edited. No Cargo, formatter, or git command was run. This report is the only output.

Scope: what the live Rust gateway can prove today for the seven lifecycle
triggers (cache cap, idle TTL, memory pressure, automatic reset, explicit reset,
expiry, process shutdown), the exact Python behavior and ordering it must match
(including soft vs hard release), the smallest production-complete Rust interface
and its real call sites, the concurrency hazards, whether drop-driven child
shutdown is actually awaitable, and a prioritized list of flaws in the existing
proposed design at `rust/analysis/conversation-eviction-map-claude.md`.

Rust paths are under `rust/crates/hermes-gateway/src/`. Python paths are the
repo originals outside `rust/`. Every line reference below was read from source
for this audit.

---

## 0. One-paragraph verdict

The existing map (`conversation-eviction-map-claude.md`) gets the cache model,
the key, the single-flight, and the LRU/TTL/pressure planner reuse right, and its
core proposal (bounded map, per-entry `active` counter, drop-driven eviction) is
the correct shape. But it has three correctness-grade gaps it either dismisses or
files under "optional polish": (1) its active guard is scoped to `run_turn` and
does not cover the post-persist finalizer, so a cap/pressure/reset eviction in the
gap between the turn and `finalize_turn_after_persist` silently drops that
conversation's memory sync; (2) drop-driven child teardown is not awaitable
(the worker `JoinHandle` is discarded) and its proposed `evict_all` at shutdown
therefore does not actually achieve the bounded graceful teardown Python
guarantees; (3) the Rust graceful path sends only `shutdown`, never
`session_end`/`flush_pending`, so every proposed eviction maps to Python's SOFT
release with the memory finalizer boundary missing, not to the HARD teardown that
reset/expiry/shutdown require. Details and priority below.

---

## 1. Exact Python behavior and ordering (verified)

### 1.1 The cache

`gateway/run.py:7792-7793` defines the cache and its lock:

```python
self._agent_cache: "OrderedDict[str, tuple]" = OrderedDict()
self._agent_cache_lock = _threading.Lock()
```

The insert shape is a 4-tuple (`run.py:6383-6384`):

```python
_cache[ctx.session_key] = (agent, _sig, _current_msg_count, ctx.session_id)
```

- Key is `session_key` (the rotation-stable routing key).
- Value carries `session_id` as its 4th element, so a rotated session id under the
  same `session_key` still reuses the cached agent (#54947). That is what keeps
  the provider prompt prefix warm across a `/resume`-style id rotation. The header
  comment at `run.py:7785` still says "2-tuple"; the live value is a 4-tuple with
  legacy 2/3-tuple tolerance throughout (`_snapshot_sid = cached[3] if len(cached) > 3`,
  `run.py:30033`).
- Defaults: `_AGENT_CACHE_MAX_SIZE = 128`, `_AGENT_CACHE_IDLE_TTL_SECS = 3600.0`
  (`run.py:85-86`), both overridable via `agent.agent_cache` through
  `resolve_agent_cache_bounds` (`gateway/agent_cache_pressure.py:185-224`).

### 1.2 Active-turn detection

`run.py:7549-7557`:

```python
def _running_agent_items(self) -> List[tuple]:
    return [(key, state.turn.agent)
            for key, state in self._sessions_map().items()
            if state.turn.agent is not None]
```

Every eviction path builds `running_ids = {id(a) for _, a in self._running_agent_items() ...}`
and skips those agents. "Active turn" means the session's `state.turn.agent` slot
is populated (a live turn or the pending sentinel).

### 1.3 Soft vs hard release

Soft, `_release_evicted_agent_soft` (`run.py:30362-30397`) then
`release_clients()` (`run_agent.py:5001-5063`). Frees per-turn child subagents,
the shared httpx/OpenAI pool, and the cached wire client. Preserves, verbatim
(`run_agent.py:5007-5012`): process_registry entries, terminal sandbox, browser
daemon, computer-use backend, and the memory provider ("has its own lifecycle;
keeps running"). Does not fire `on_session_end`. The transcript is rebuilt from
the persisted session on the next turn.

Hard, `_cleanup_agent_resources` (`run.py:12374-12431`), ordering:

1. `_mm.flush_pending(timeout=10)` (drain the memory queue), `run.py:12395`
2. `shutdown_memory_provider(session_messages)` which fires providers'
   `on_session_end`, `run.py:12408-12412`
3. `agent.close()`, `run.py:12419-12420`
4. `cleanup_stale_async_clients()`, `run.py:12428`

`agent.close()` (`run_agent.py:5065+`): `on_session_end` again (idempotent),
`process_registry.kill_all(task_id)` (SIGTERM then SIGKILL after a grace window,
default `terminal.daemon_term_grace_seconds = 2.0`, `tools/process_registry.py:913-928`),
sandbox/browser/computer-use cleanup, child agents, clients,
`session_db.end_session(session_id, "agent_close")` gated on `_end_session_on_close`.

### 1.4 Trigger-by-trigger (soft/hard, ordering, on_session_end)

| Trigger | Site | Soft/Hard | Fires on_session_end | Guards |
|---|---|---|---|---|
| LRU cap `_enforce_agent_cache_cap` | `run.py:30578` | SOFT | yes, via pre-evict commit for finalizable-not-yet-expired | skips `id(a) in running_ids`; can stay over cap when all candidates active (`30627-30628`) |
| Idle TTL `_sweep_idle_cached_agents` | `run.py:30661`, driven from expiry watcher `run.py:15610` | SOFT | no (finite-unexpired deferred to the watcher `30727-30733`; `mode=none` skips) | `running_ids`; `(now - _last_activity_ts) > ttl` |
| Pressure `_sweep_agent_cache_under_pressure` | `run.py:30434`, driven from watcher `run.py:15626` | SOFT | yes, via `_commit_then_release_soft` for finalizable-pre-expiry | `_is_evictable` = not sentinel AND `id ∉ running_ids` AND `transcript_persistence_caught_up(agent)` (`30482-30487`) |
| Automatic (session) expiry | `run.py:15539-15547` | HARD | yes (provider); `end_session` via `set_expiry_finalized` | bounded off-loop cleanup, `_end_session_on_close=False` |
| Explicit reset `/new`,`/reset` | `gateway/slash_commands.py:150` | HARD | yes | see ordering below |
| Shutdown | `run.py:16822-16837` | HARD | yes (provider); `end_session` skipped | drains + clears cache under lock, then per-agent bounded off-loop cleanup |

Explicit reset ordering (`slash_commands.py:150-248`), the canonical
"bounded, proceeds-on-timeout" model:

1. `_invalidate_session_run_generation(session_key)` (`:156`) so a late in-flight
   completion's guarded release returns False and cannot resurrect the slot.
2. `_release_running_agent_state(session_key)` (`:162`).
3. capture `_old_agent`, run `_cleanup_agent_resources` off-loop under
   `asyncio.wait_for(..., timeout=_RESET_CLEANUP_TIMEOUT_S)` where
   `_RESET_CLEANUP_TIMEOUT_S = 30.0` (`slash_commands.py:58-63, 186-208`); on
   timeout the reset proceeds and the teardown is left to finish or leak (#35994,
   `:199-205`).
4. `_evict_cached_agent(session_key)` (`:209`) (idempotent by now).
5. `_clear_conversation_scope(session_key)` (`:215`).
6. `reset_session(session_key)` rotates the session id (`:248`).
7. emit `session:reset` hook (`:278`).

The load-bearing pattern for Rust: teardown of the old conversation is HARD and
runs to a bounded timeout, the run generation is bumped first, and the session id
is rotated last.

Shutdown ordering (`run.py:16822-16837`):

```python
with _cache_lock:
    _idle_agents = list(_cache.values())
    _cache.clear()
for _entry in _idle_agents:
    _agent = _entry[0] if isinstance(_entry, tuple) else _entry
    await self._cleanup_agent_resources_off_loop(_agent, context="shutdown idle-cache")
```

Cache emptied atomically under the lock, then each agent gets a bounded off-loop
cleanup (`_CLEANUP_TIMEOUT_S`, `run.py:12352-12366`) so one wedged provider cannot
hang SIGTERM (#53175).

---

## 2. What the live Rust paths can prove today

| Trigger | Live Rust status |
|---|---|
| Cache cap | Not enforced. `ConversationAgent` is `Mutex<HashMap<(PathBuf,String), Arc<OnceCell<Arc<dyn AgentClient>>>>>` (`conversation_agent.rs:25-32`) with no cap, no `remove`/`evict`/`close`/`Drop`. |
| Idle TTL | Not enforced. `AgentCacheBounds.idle_ttl_secs` resolves (`agent_cache_pressure.rs:49,244`) but nothing consumes it. |
| Memory pressure | Not enforced. `read_anon_rss_mb` / `plan_pressure_evictions` exist and are unit-tested (`agent_cache_pressure.rs:255-320`) but are never called against the map. Whole module is `#![allow(dead_code)]` (`:4`). |
| Automatic reset | Reset itself is live: dispatch resolves the session via `store.get_or_create_with_legacy` (`dispatch.rs:319-330`), which applies auto-reset and mints a new id (`session_store.rs:198-256`), then sets `msg.resolved_session_id` (`dispatch.rs:334`). But no cache entry is evicted, so the old `(home, prev_session_id)` entry and its Python child are orphaned. |
| Explicit reset | Not ported. `slash.rs` has no `/new`/`/reset`. No call site for an explicit-reset eviction. |
| Expiry | Reset-reason decision logic is ported (`session_reset.rs`), and the store applies it on resolution, but there is no separate expiry watcher that tears a cached client down. |
| Process shutdown | Children die, but by SIGKILL. The `CancellationToken` is cancelled on signal (`main.rs:970-978`) and the HTTP server does a graceful shutdown (`main.rs:1079`), but nothing drains the cache or the in-flight turns; when `main` returns the runtime drops and aborts the detached worker tasks, so `Process::drop` SIGKILLs each group (`extension_host.rs:133-144`) and the graceful `shutdown` RPC is skipped. |

Net: none of the seven triggers is production-complete today. Reset and shutdown
"work" only in the degraded sense that the OS eventually reaps the children.

---

## 3. The current turn lifecycle and where teardown must hook

Per push turn (`dispatch.rs`):

1. resolve session (auto-reset applied), set `resolved_session_id`, `:319-334`
2. `session_id = message_session_id(&msg)`, `:354`
3. `generation = self.generation.fetch_add(...)`, `:355`
4. `lease.acquire(&session_id, ...)` serializes same-session turns, `:356-366`
5. spawn `run_admitted_turn` (awaited), `:372-380`
6. inside: `begin_turn` loads history, `:395`
7. spawn `agent_task` = `agent.run_turn_with_context(...)`, `:402-411`
8. accumulate `reply` from the stream, `:419-437`
9. `end_turn` persists the assistant row, `:453`
10. `agent.finalize_turn_after_persist(..., &reply, succeeded)`, `:454-464`

`ConversationAgent::run_turn_with_context` (`conversation_agent.rs:73-111`) is the
checkout: lock, get-or-insert the `OnceCell`, clone it, release the lock, then
`get_or_try_init` runs the factory (all provider I/O) with no map lock held, then
`client.run_turn_with_context`. `finalize_turn_after_persist`
(`conversation_agent.rs:113-134`) re-locks, looks up the cell, and if it is
missing or uninitialized returns `Ok(())` silently.

The finalizer body (`native_agent.rs:694-718`) takes the shared
`pending_memory_turn` (`Arc<Mutex<Option<PendingMemoryTurn>>>`, `native_agent.rs:251`),
appends the assistant reply, and calls `host.turn_complete(...)` (the memory
`sync_turn`/`queue_prefetch` boundary). This is the Rust analog of Python's
post-turn memory commit, and it runs in step 10, after the turn future from step
7 has already resolved.

The extension child (`extension_host.rs`): `Client` is a `Clone` wrapper around
`mpsc::Sender<WorkerCommand>` plus an id counter (`:115-119`). `spawn` starts a
detached worker with `tokio::spawn(run_worker(process, receiver))` and discards
the `JoinHandle` (`:224`). When the last `Client` sender drops, `receiver.recv()`
returns `None`, and `run_worker` (if still `healthy`) sends one `shutdown` request
(7 s), closes stdin, waits 7 s, then SIGKILLs the group (`:381-392`).
`Process::drop` SIGKILLs the group as a backstop (`:133-144`).

Crucially, the worker's graceful path sends only `{"method":"shutdown"}`
(`:382`). There is no public `Client::session_end` or `Client::flush_pending`
method; those RPCs are exercised only inside the extension-host test via the
private `request` (`extension_host.rs:916-951`), and no production code calls
them. So the memory provider's `on_session_end(messages)` (the finalizer that
extracts and persists memory from the full transcript) never fires on teardown;
only the provider's `shutdown()` does.

---

## 4. Smallest production-complete Rust interface

The goal is parity with Python's soft/hard split and its bounded, observable
teardown, added with the fewest moving parts. Two layers change: the cache
(`ConversationAgent`) and the child handle (`extension_host`).

### 4.1 `extension_host::Client`: make teardown awaitable and complete

The single most important change. Today teardown is fire-and-forget. Add:

```rust
// A joinable owner for the detached worker. The cache entry holds one.
pub struct HostHandle {
    client: Client,                 // the Clone sender used by tools + the agent
    worker: tokio::task::JoinHandle<()>,  // retained, not discarded
}

impl Client {
    // Explicit RPCs, gated to the HARD triggers.
    pub async fn session_end(&self, messages: &[Value]) -> Result<()>;   // fires on_session_end
    pub async fn flush_pending(&self, timeout_s: f64) -> Result<bool>;   // drains the memory queue
}

impl HostHandle {
    // Bounded, observable graceful close. Mirrors Python's flush_pending(10) ->
    // shutdown_memory_provider(on_session_end) -> provider shutdown, then joins
    // the worker within `budget`, SIGKILL on timeout. Returns whether it was clean.
    pub async fn close_graceful(self, messages: &[Value], budget: Duration) -> bool;
}
```

`spawn` keeps the `JoinHandle` instead of dropping it at `extension_host.rs:224`.
`run_worker` gains an explicit "graceful close" command variant so the caller can
drive `flush_pending` + `session_end` + provider `shutdown` in order and then
`await` the worker's exit with a timeout, rather than relying on sender-drop and a
runtime that may abort the task first.

Soft vs hard maps cleanly: a SOFT eviction (cap/idle/pressure) drops the
`HostHandle`, taking today's drop-driven `shutdown`-only path (no `session_end`).
A HARD eviction (reset/expiry/shutdown) calls `close_graceful(messages, budget)`
first, so `on_session_end` fires and the queue drains within a bound. Note this
is stricter than Python's soft path, which keeps the provider process alive; see
flaw 4.

### 4.2 `ConversationAgent`: bounded map with a guard that spans the finalizer

```rust
struct Entry {
    cell: Arc<OnceCell<Arc<dyn AgentClient>>>,
    last_used: Instant,
    active: u32,        // in-flight turns; never soft-evict when > 0
    finalizing: bool,   // set at run end, cleared after finalize; never soft-evict when true
}

pub struct ConversationAgent {
    fallback: Arc<dyn AgentClient>,
    factory: Box<Factory>,
    inner: std::sync::Mutex<HashMap<Key, Entry>>,  // leaf lock, never held across .await
    bounds: AgentCacheBounds,
}

impl ConversationAgent {
    // sweeps and explicit eviction; all collect removed Arcs and drop them AFTER unlock
    pub fn sweep_idle(&self, now: Instant);
    pub fn sweep_pressure(&self, rss_mb: impl Fn() -> Option<i64>);
    pub async fn evict_conversation(&self, home: &Path, session_id: &str, hard: bool, budget: Duration);
    pub async fn evict_all(&self, budget: Duration);  // shutdown drain, HARD
}
```

Two consts in `agent_cache_pressure.rs` supply the defaults for the existing
`Option` fields, so no new config key appears: `DEFAULT_MAX_SIZE = 128`,
`DEFAULT_IDLE_TTL_SECS = 3600.0` (mirroring `run.py:85-86`).

The difference from the existing proposal is the `finalizing` bit and the
`hard`/`budget` parameters on the evict calls. Both are required for parity (flaws
1, 2, 3).

### 4.3 Call sites (real, in the live tree)

- Construction: `main.rs:894-914`, where `ConversationAgent::new` is already built.
  Thread `resolve_agent_cache_bounds(&user_config)` in, keep a typed
  `Arc<ConversationAgent>` in `AppState` alongside the `Arc<dyn AgentClient>` so
  the sweep task, shutdown, and the reset hook can reach the concrete type.
- Sweep task: model it on `memory_monitor::start_memory_monitoring(Duration::from_secs(300), shutdown.clone())`
  (`main.rs:989`). Each tick calls `ca.sweep_idle(Instant::now())` then
  `ca.sweep_pressure(agent_cache_pressure::read_anon_rss_mb)`. 300 s matches
  Python's watcher cadence and the existing monitor.
- Auto-reset hook: `dispatch.rs`, right after `msg.resolved_session_id = Some(entry.session_id)`
  (`:334`). When the resolved entry reports it was an auto-reset it carries
  `prev_session_id` (`session_store.rs:250-255`). Call
  `ca.evict_conversation(home, prev_session_id, /*hard=*/true, budget)` there,
  after the generation bump (`dispatch.rs:355`) so a stale finalizer on the old id
  cannot resurrect it. This is a present, wireable call site (see flaw 5).
- Shutdown: after `shutdown.cancelled()` resolves and the HTTP server's graceful
  shutdown returns (`main.rs:1079-1089`), before `main` returns and the runtime
  drops, call `ca.evict_all(budget)` and `.await` it. That is the only point where
  a bounded graceful child teardown can happen; the current drop-at-runtime-exit
  path cannot (flaw 2).
- Explicit reset: no call site yet (`slash.rs` has no reset). When the explicit
  slash reset is ported it should follow Python's ordering in 1.4 and call
  `evict_conversation(..., hard=true, budget)`.

---

## 5. Concurrency hazards

1. In-flight initialization. Handled correctly today and in the proposal: the map
   lock is released before `OnceCell::get_or_try_init` runs the factory
   (`conversation_agent.rs:84-104`), distinct keys have distinct cells so they
   init in parallel, and a failed init leaves the cell uninitialized and retryable
   (OnceCell does not cache `Err`). The `active` counter must be incremented under
   the map lock at checkout (before the lock is released) so a concurrent sweep
   either sees `active > 0` and skips, or sees no entry yet and has nothing to
   evict. Keep this invariant.

2. Turn completion vs the shared `pending_memory_turn`. `NativeAgentClient` is
   `#[derive(Clone)]` (`native_agent.rs:228`) and `pending_memory_turn` is an
   `Arc<Mutex<Option<..>>>` (`:251`), so all clones share one slot.
   `run_native_turn` clears it at turn start (`:583`) and writes it at turn end
   (`:660`); `finalize_turn_after_persist` takes it (`:701`). This is safe only
   because the turn lease serializes same-session turns (`dispatch.rs:356`) and the
   cache key includes `session_id`, so exactly one turn per cached client at a
   time. If a future rotation or lease bypass ever lets two turns share one client,
   the slot is clobbered and one turn's memory sync is lost. Document the
   dependency; do not rely on an "active counter covers lease-bypass" claim for
   this slot specifically.

3. The post-persist finalizer vs eviction (the sharp one). `finalize_turn_after_persist`
   is a separate trait call made after the `run_turn` future has resolved
   (`dispatch.rs:439-454`). Any active-turn guard scoped to `run_turn` has already
   been released by the time the finalizer runs. In that gap the entry has
   `active == 0`, so cap enforcement on another key's checkout, a background
   pressure/idle sweep, or an auto-reset can remove it; then
   `ConversationAgent::finalize_turn_after_persist` looks up a missing cell and
   returns `Ok(())` silently (`conversation_agent.rs:128-130`), dropping the
   `turn_complete`/`sync_turn` memory commit with no error. This is exactly what
   Python's `transcript_persistence_caught_up` guard and idle finalizable-deferral
   exist to prevent. Fix: gate soft eviction on `finalizing == false` (set the bit
   at run end, clear it after the finalizer), or extend the RAII guard across both
   calls. See flaw 1.

4. Eviction vs child teardown. Removing entries under the lock and dropping the
   collected `Arc`s after unlock is correct and must be kept; `Client` drop does no
   blocking I/O. But drop only starts a SOFT teardown. For HARD triggers the evict
   call must first `close_graceful(...).await` (which needs the retained worker
   `JoinHandle`), off the map lock. See flaws 2 and 3.

5. Child teardown vs runtime shutdown. Because the worker task is detached and its
   `JoinHandle` discarded (`extension_host.rs:224`), whether the graceful path runs
   at process shutdown is a race between the last sender dropping and the runtime
   aborting the task. Today the runtime usually wins, so SIGKILL. Retaining the
   handle and awaiting it in `evict_all` removes the race.

---

## 6. Is drop-driven child shutdown awaitable? No.

Directly: no. The worker `JoinHandle` is discarded at `extension_host.rs:224`, so
no caller can await a specific child's teardown, and there is no completion signal.
Dropping the last `Client` clone only makes `receiver.recv()` return `None` the
next time the worker task is polled; nothing guarantees it is polled before the
runtime is torn down. At process shutdown the runtime drop aborts the task, and
`Process::drop` SIGKILLs the group (`:133-144`), so the graceful `shutdown` RPC at
`:381-384` is skipped. The existing proposal's `evict_all` (drop-and-return) does
not change this: `main` returns immediately after, the runtime drops, and the same
SIGKILL race reappears. So the proposal's claim that `evict_all` "tears every
child down at shutdown" is only true in the SIGKILL sense, not the graceful sense.

To make shutdown / `session_end` / queue-drain bounded and observable:

1. Retain the worker `JoinHandle` in a `HostHandle` the cache entry owns (4.1).
2. Add explicit `Client::flush_pending(timeout_s)` and `Client::session_end(messages)`
   public methods (the RPCs already exist on the Python side; the test drives them
   at `extension_host.rs:916-951`).
3. `HostHandle::close_graceful(messages, budget)` runs, in Python's order,
   `flush_pending(10)` then `session_end(messages)` then the provider `shutdown`,
   then `tokio::time::timeout(budget, worker)` and SIGKILL on expiry. Return the
   clean/unclean bool.
4. Drive it from real bounded call sites: `evict_all(budget)` at
   `main.rs:1079-1089` before the runtime drops; `evict_conversation(..., hard=true, budget)`
   from the auto-reset hook and (later) the explicit reset. Use budgets matching
   Python: 30 s for reset (`_RESET_CLEANUP_TIMEOUT_S`), a per-agent
   `_CLEANUP_TIMEOUT_S`-style bound at shutdown.
5. Observability: increment eviction counters by trigger and log a warning when
   `close_graceful` hits its timeout (Python logs on over-cap and on reset-cleanup
   timeout; match that). Without this the SOFT-vs-HARD behavior is invisible in
   production.

Queue drain of in-flight turns at shutdown is a separate, currently-unbounded gap:
`run_admitted_turn` turns are spawned (`dispatch.rs:372,402`) and are not joined at
shutdown, so the runtime drop aborts them mid-turn. Bounding that (await the
in-flight set with a timeout on cancel) belongs with the same shutdown work.

---

## 7. Test cases

Deterministic unit tests (extend the existing `RecordedAgent` harness in
`conversation_agent.rs:137-256`; inject `now: Instant` into `sweep_idle` and an
`rss_mb` closure into `sweep_pressure`):

1. `cap_evicts_lru_nonactive`: `max_size = 2`; touch A, B, C; assert A gone, B and
   C kept, and MRU order tracks `last_used`.
2. `cap_stays_over_when_all_active`: two active turns with `max_size = 1`; assert
   neither is evicted and the map stays at 2 (Python's over-cap tolerance).
3. `idle_ttl_evicts_only_stale`: set A `last_used = now - 2*ttl`; `sweep_idle`;
   assert A gone, fresh B kept.
4. `pressure_skips_active`: hold A active on a factory barrier; force RSS above the
   budget; assert A survives and idle B is removed.
5. `failed_init_retryable_not_leaked`: factory bails once; assert the empty entry
   is removed and the next turn rebuilds.
6. `finalizer_not_dropped_by_eviction` (guards flaw 1): run a turn on A to
   completion so `pending_memory_turn` is set and `finalizing` is true; on another
   thread force a cap/pressure sweep; assert A is NOT evicted while `finalizing`,
   then call `finalize_turn_after_persist` and assert the memory `turn_complete`
   actually reached the client (record it in the fake).
7. `sweep_does_not_block_on_inflight_init`: factory awaits a barrier (lock not
   held); a sweep on another thread returns immediately.

Real process-teardown and prompt-cache evidence (integration, current-thread
runtime, reusing the real fixture plugin already in `extension_host.rs:699-1051`,
whose `FixtureProvider.shutdown` writes `extension-shutdown-<session>` at
`:794-795` and whose `on_session_end` appends a `session_end` line at `:780-781`):

8. Prompt-cache reuse: run two turns on the same `(home, session_id)`; assert the
   factory ran once (client reused = warm prompt prefix), the child pid is stable,
   and no `extension-shutdown` sentinel appears between turns.
9. Real teardown per trigger: for `evict_conversation(hard=true)`, LRU-cap
   overflow, `sweep_idle`, `sweep_pressure(budget=1)`, and `evict_all`, assert the
   evicted conversation's `extension-shutdown-<session>` sentinel appears and
   `libc::kill(pid, 0) == ESRCH` (the OS process is really gone), not merely that
   the `Arc` dropped.
10. HARD fires `session_end`, SOFT does not (parity check for flaw 3): a HARD evict
    (reset/expiry/shutdown) must produce the `session_end` line in
    `extension-lifecycle-<session>` before the `shutdown` sentinel; a SOFT evict
    (cap/idle/pressure) must produce the `shutdown` sentinel with no preceding
    `session_end`. This is the test that would have caught the missing finalizer.
11. Bounded teardown: point the fixture provider's `shutdown`/`on_session_end` at a
    long sleep, run `evict_all(budget = short)`, and assert it returns within the
    budget and the child is SIGKILLed (proves the timeout-and-proceed path).
12. Never-evict-active: hold a turn active, force pressure, assert no sentinel until
    the turn completes and its clones drop.

Tests 6, 8, 10, and 11 are the ones the existing proposal's test list does not
cover and that its design would fail.

---

## 8. Prioritized flaws in the existing proposed design

Ranked most severe first. All references are to
`rust/analysis/conversation-eviction-map-claude.md`.

**HIGH 1: the active guard does not cover the post-persist finalizer, so eviction
silently drops the memory sync.** The proposal scopes the RAII `active` guard to
`run_turn_with_context` (its section 3.2) and, in section 5, dismisses
`transcript_persistence_caught_up` as "only needed once background/async transcript
flushing exists" because "the active counter already protects in-flight turns."
That reasoning is wrong: `finalize_turn_after_persist` is a distinct trait call
made after the guarded future resolves (`dispatch.rs:439-454`), and
`ConversationAgent::finalize_turn_after_persist` returns `Ok(())` on a missing cell
(`conversation_agent.rs:128-130`). `enforce_cap` runs on every checkout of any key
(proposal 3.2), and the background sweep and the auto-reset hook run independently,
so in the gap between turn end and the finalizer the entry has `active == 0` and is
freely evictable, dropping that conversation's `turn_complete`/`sync_turn` with no
error. This is a real memory-loss bug, not deferred polish. Fix: a `finalizing`
guard bit (section 4.2 here) or extend the guard across both calls; it is the Rust
analog of Python's pressure `transcript_persistence_caught_up` and idle
finalizable-deferral, which exist for exactly this reason.

**HIGH 2: drop-driven teardown is not awaitable, so the proposed `evict_all` does
not deliver bounded graceful shutdown.** The proposal (sections 3.3, 3.5, and its
"deferred/optional" note in section 5) treats exposing the worker `JoinHandle` as
optional polish and has `evict_all` drop entries and return. But the handle is
discarded (`extension_host.rs:224`), so after `evict_all` returns and `main` exits,
the runtime drop aborts the workers and SIGKILLs the groups, which is the very gap
the proposal itself lists as its point 7. Bounded, awaited teardown is the
mechanism that closes that gap, not an enhancement. Python guarantees it (bounded
off-loop cleanup, #53175/#35994). Fix: retain the handle and `await` it with a
timeout in `evict_all`/`evict_conversation` (sections 4.1 and 6 here).

**HIGH 3: no eviction fires `session_end`/`flush_pending`, so every proposed
eviction is a SOFT release missing the memory finalizer, not the HARD teardown
reset/expiry/shutdown require.** The proposal states (section 1.6) that the child's
graceful `shutdown` request "is the Rust analog of Python's provider.shutdown() /
on_session_end" and (section 5) that "drop equals full close." Source contradicts
this: the worker's graceful path sends only `{"method":"shutdown"}`
(`extension_host.rs:382`), which calls the provider's `shutdown()` (sentinel at
`:794`), never `on_session_end(messages)` (`:780`). Python's hard cleanup fires
`flush_pending(10)` then `shutdown_memory_provider` → `on_session_end` as distinct
steps before provider teardown (`run.py:12395-12412`). The `session_end` and
`flush_pending` RPCs exist but have no public `Client` method and no production
caller (only the test at `extension_host.rs:916-951`). Fix: add the two public
methods and drive them from a HARD `close_graceful` (section 4.1 here).

**MEDIUM 4: the "single teardown kind" claim is wrong for the memory provider and
makes Rust soft evictions both more destructive and less complete than Python's.**
The proposal argues (section 5) that because Rust owns no sandbox/browser/process
subsystems there is one teardown kind and soft == hard == drop. But the memory
provider does live in the extension child, and Python's soft path deliberately
keeps that provider process alive and does not fire `on_session_end`
(`run_agent.py:5007-5012`), so a soft-evicted conversation rebuilds cheaply and its
memory boundary is untouched. In Rust a soft eviction drops the `Client`, which
kills the whole child (provider included) and, per flaw 3, without `on_session_end`.
So Rust cap/idle/pressure evictions are hard-in-effect process kills that also
silently skip the finalizer: each one forces a full Python respawn on the
conversation's next turn and loses its pending memory commit. That is a real
behavior change from Python and a cost-model change (every pressure pass respawns
processes), and it should be stated, not dismissed. At minimum, soft evictions
should fire `session_end` too if the child is going to die.

**MEDIUM 5: the reset hook is mischaracterized: auto-reset is already live and
wireable now, while explicit reset has no call site at all.** The proposal (section
3.5) says the reset hook may not be wireable "in this checkpoint" and that
otherwise "the bounds sweeps already reclaim the orphaned old entry within one
idle-TTL or under pressure." In fact the auto-reset actuation is live:
`dispatch.rs:319-334` calls `store.get_or_create_with_legacy`, which mints a new id
(`session_store.rs:198-256`) and exposes `prev_session_id`
(`session_store.rs:250-255`). So `evict_conversation(home, prev_session_id, hard)`
has a concrete call site today (dispatch, right after the id is set, after the
generation bump). Relying on idle-TTL instead leaks one live Python child per
auto-reset for up to 3600 s, which under a reset-heavy workload is precisely the
#80764 hoarding the module targets. Conversely, the proposal implies the explicit
`/new`/`/reset` hook exists to be wired; it does not (`slash.rs` has no reset), so
that hook currently has no call site. State which reset is which.

**MEDIUM 6: ordering of eviction against id rotation and the run generation is
unspecified.** Python bumps the run generation before eviction and rotates the id
last (`slash_commands.py:156-248`). In Rust the id is rotated during resolution,
before the turn (`dispatch.rs:334`), so the old-entry eviction must be driven from
that point using `prev_session_id` and must be ordered after the generation bump
(`dispatch.rs:355`) so a late finalizer on the old id cannot rebuild it. The
proposal's `evict_conversation(home, session_id)` signature does not say who
supplies the old id or how it orders against the generation, leaving the sharpest
leak path under-specified.

**LOW 7: session_id keying strands finalizers on rotation.** The proposal
correctly defers rotation-stable keying (#54947) to the compression work but does
not note that, combined with flaws 1 and 5, keying by `session_id` is where memory
syncs actually get stranded: the new turn uses the new key while the old entry may
still hold an unflushed `pending_memory_turn`. Worth a sentence so the deferral is
not read as "harmless."

**LOW 8: the pressure planner call drops the `transcript_persistence_caught_up`
predicate.** The proposal's `sweep_pressure` pseudocode (section 3.3) passes only
`active == 0` as `is_evictable`, whereas Python's `_is_evictable` also requires
`transcript_persistence_caught_up(agent)` (`run.py:30482-30487`). The Rust analog
is "no outstanding `pending_memory_turn`/`finalizing`" (flaw 1). Same root cause,
noted separately because it is a concrete missing predicate in the planner call.

Not a flaw, confirmed correct in the proposal: the cache key derivation, the
lock-released-before-factory single-flight, the retryable failed-init, the
drop-after-unlock discipline, the reuse of `AgentCacheBounds` /
`plan_pressure_evictions` with two default consts and no new config key, and the
LRU-cap "stay over cap when all candidates active" semantics all match source and
Python.

---

## 9. Summary of the delta this audit adds

1. Add a `finalizing` guard so soft eviction cannot run in the turn-to-finalizer
   gap (flaw 1). This is a correctness fix, not polish.
2. Retain the worker `JoinHandle` and add `Client::session_end` /
   `Client::flush_pending` / `HostHandle::close_graceful(messages, budget)` so HARD
   teardown fires `on_session_end`, drains the queue, and is awaitable within a
   bound (flaws 2, 3; section 4.1).
3. Split soft vs hard at the evict call (`hard: bool`, `budget`): soft drops,
   hard `close_graceful`s first (flaw 4).
4. Wire the auto-reset eviction from `dispatch.rs:334` using `prev_session_id`,
   after the generation bump; wire `evict_all(budget)` at `main.rs:1079-1089`
   before the runtime drops; run the 300 s sweep task like `memory_monitor`
   (flaws 5, 6; section 4.3).
5. Add tests 6, 8, 10, 11 (finalizer-not-lost, prompt-cache reuse, hard-fires-
   session_end vs soft-does-not, bounded teardown) on top of the proposal's set.
