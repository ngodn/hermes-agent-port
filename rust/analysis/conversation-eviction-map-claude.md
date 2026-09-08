# Conversation eviction map: bounded native conversation ownership and extension-child teardown

Analysis only. No implementation files were edited, no Cargo ran, nothing was committed or pushed.

Scope. This maps the current native conversation-agent cache and the extension-host child lifecycle in the Rust gateway, traces every path that can pin a cached client and its Python child indefinitely, checks them against the Python reference in `gateway/run.py` / `run_agent.py` / `gateway/agent_cache_pressure.py`, and proposes a production design that bounds the cache while preserving per-conversation prompt reuse and tearing children down promptly. It closes with what belongs in this checkpoint versus the broader prompt-invalidation and compression work.

All Rust paths are under `rust/crates/hermes-gateway/src/`. Python paths are the repo's original (outside `rust/`).

---

## 1. What exists today

### 1.1 The authoritative cache and its key

`ConversationAgent` (`conversation_agent.rs`) is the whole cache. It wraps a fallback agent and a factory, and holds:

```rust
type ClientCell = tokio::sync::OnceCell<Arc<dyn AgentClient>>;
type Clients = HashMap<(PathBuf, String), Arc<ClientCell>>;
clients: tokio::sync::Mutex<Clients>          // conversation_agent.rs:25-31
```

The authoritative key is `(home, session_id)`, built per turn at `conversation_agent.rs:83`:

```rust
let key = (home.to_owned(), crate::session_db::message_session_id(msg));
```

- `home` comes from `TurnContext.home`, which `TurnContext::from_database` sets to `db.profile_home()` (`agent.rs:43-48`), and `profile_home()` returns the session DB file's parent directory (`session_db.rs:440-442`). So `home` is the selected profile's install directory. This is what gives per-profile isolation.
- `session_id` is `message_session_id(msg)` (`session_db.rs:146-176`): the durable `resolved_session_id` when session resolution has run, otherwise a derived fallback (`{platform}:{channel_id}` lowercased, or a thread/workspace-scoped JSON encoding). In the live push path the dispatcher resolves the session first, so this is the durable conversation id.

Lookup, single-flight, and run, at `conversation_agent.rs:84-108`:

```rust
let cell = self.clients.lock().await
    .entry(key).or_insert_with(|| Arc::new(ClientCell::new())).clone();  // map lock released here
let client = cell.get_or_try_init(|| async {
    (self.factory)(home, msg, history, context.database).await ...        // provider I/O, no map lock
}).await?.clone();
client.run_turn(msg, history, events).await;
```

Two good properties already hold:

- The map mutex is released before the factory runs. The `.entry(...).or_insert_with(...).clone()` statement drops its guard at the semicolon, so `OnceCell::get_or_try_init` (which does all the I/O: config read, extension-host spawn, prompt snapshot) runs with no map lock held. The comment at `conversation_agent.rs:106-108` states this intent.
- Failed initialization is retryable. `OnceCell::get_or_try_init` does not store the value on `Err`, so a failed build leaves the cell uninitialized and the next turn retries. Distinct keys initialize in parallel because each has its own cell.

### 1.2 What a cached value owns

The factory (wired in `main.rs:894-914`) calls `build_conversation_client` (`main.rs:523-738`), which:

1. Decides whether extensions are configured (`extensions_configured`, `main.rs:219-235`: a `memory.provider`, any `plugins.enabled`, or an auto-loaded `kind: backend` toolset).
2. If so, spawns one persistent Python child via `extension_host::Client::spawn` (`main.rs:602`).
3. Builds a `NativeAgentClient` and moves the extension `Client` into it via `with_extension_host` (`main.rs:504-508`, `native_agent.rs:267-269`).

`NativeAgentClient` holds the child handle and the tool handles:

```rust
_extension_host: Option<crate::extension_host::Client>,   // native_agent.rs:214
tools: Vec<Arc<dyn crate::native_tools::Tool>>,           // native_agent.rs:219
```

Each extension `Tool` also holds a `Client` clone (`extension_host.rs:469-471`, `523-530`). `extension_host::Client` is just `mpsc::Sender<WorkerCommand>` plus an id counter (`extension_host.rs:107-111`), and it is `Clone`. `NativeAgentClient` is `Clone` too, and `run_turn` clones it per turn (`native_agent.rs:484`), so an in-flight turn holds its own `Client` clones independent of the cache.

### 1.3 How a child is torn down (this part is correct)

The child is a process-group leader (`process_group(0)`, `extension_host.rs:196`) with `kill_on_drop(true)` (`extension_host.rs:194`). One detached worker task owns the `Process` (`tokio::spawn(run_worker(...))`, `extension_host.rs:216`). Teardown is drop-driven:

- When every `Client` clone drops, the worker's `receiver.recv()` returns `None`, and `run_worker` sends a graceful `shutdown` request (7 s budget), closes stdin, waits, then SIGKILLs the process group on timeout (`extension_host.rs:311-347`).
- A transport failure or a per-request timeout kills the process tree immediately (`exchange` timeout at `:436`, transport-fatal branch at `:329-333`, `kill_process_tree` at `:444-466`).
- `Process::drop` SIGKILLs the group as a backstop (`extension_host.rs:125-136`), and `kill_on_drop` covers the `Child` itself.

So the child dies correctly once the last `Client` clone drops or the worker task is dropped. The entire problem below is that nothing drops the cached `Client`.

### 1.4 The active-turn boundary

The turn lease (`turn_lease.rs`) serializes the load/run/flush region per `session_id`. The dispatcher acquires it at `dispatch.rs:354-366` keyed by `message_session_id(&msg)`, the same value that forms the second half of the cache key, and holds the token across `run_admitted_turn` (`dispatch.rs:371-380`), which calls `run_turn_with_context` (`dispatch.rs:401-410`). The lease registry is itself bounded and never evicts a held or contended lease (`DEFAULT_MAX_LEASES = 512`, `evict_idle` at `turn_lease.rs:237-251`).

Note the three distinct keyings in play:

- `ConversationAgent` cache: `(home, session_id)`.
- `turn_lease`: `session_id`.
- `SessionRegistry` state and `AgentSlot` running flag (`session_registry.rs`, `session_state.rs`): the routing `session_key`.

The turn lease is the one whose key aligns with the cache-key's session component. The design below does not depend on it though; it uses a per-entry active counter as the direct in-flight signal, which also covers callers that bypass the lease.

### 1.5 Bounds and sweeps: none are wired

`agent_cache_pressure.rs` is fully ported but inert. It is `#![allow(dead_code)]` (`:4`) and its own header says the sweep "depend[s] on the `AIAgent` type + `GatewayRunner`, so they land with the agent core loop." It provides:

- `AgentCacheBounds { max_size: Option<i64>, idle_ttl_secs: Option<f64>, memory_high_mb: Option<i64>, max_evictions_per_pass, protect_recent }` (`:47-53`), read from `agent.agent_cache` by `resolve_agent_cache_bounds` (`:217-249`).
- `resolve_memory_high_mb` (`:178-212`): `"auto"` derives `0.65 * cgroup memory.high/max` with a 512 MB floor.
- `read_anon_rss_mb` (`:255-270`) and `plan_pressure_evictions` (`:291-320`): LRU-first plan, capped at `max_evictions_per_pass` (16), protecting `protect_recent` (8, clamped to half the cache).

Nothing calls `plan_pressure_evictions` or `read_anon_rss_mb` against the `ConversationAgent` map. `memory_monitor.rs` only logs `[MEMORY]` lines (`start_memory_monitoring`, `memory_monitor.rs:78-113`); it does not sweep. `ConversationAgent` has no `remove`, `evict`, `close`, `shutdown`, or `Drop` (confirmed: the only references to `clients` are the field, its init, and the lookup).

### 1.6 The Python reference this diverges from

From `gateway/run.py` and `run_agent.py`:

- Cache is `self._agent_cache: OrderedDict[session_key -> (agent, config_signature, message_count, session_id)]` under `_agent_cache_lock` (`run.py:7792-7793`). The key is the rotation-stable `session_key`, and `session_id` lives in the value so a rotated session id under the same key still reuses the agent (#54947), specifically to keep the prompt cache warm across `/resume`-style switches.
- Bounds: `_AGENT_CACHE_MAX_SIZE = 128`, `_AGENT_CACHE_IDLE_TTL_SECS = 3600.0` (`run.py:85-86`), overridable through the same `resolve_agent_cache_bounds`.
  - LRU cap enforced on every insert by `_enforce_agent_cache_cap` (`run.py:30578`), using `move_to_end` on each hit.
  - Idle TTL enforced by a sweep loop `_sweep_idle_cached_agents` (`run.py:30661`) run from the ~300 s session-expiry watcher, deferring eviction while `on_session_end` still needs to fire.
  - Pressure sweep `_sweep_agent_cache_under_pressure` (`run.py:30434`) using the same pure planner.
- Active-turn guard: every eviction path computes `running_ids = {id(a) for _, a in self._running_agent_items()}` and skips members (cap `run.py:30627`, idle `:30689`, pressure `_is_evictable` `:30485`, direct evict `:30245-30251`). Pressure adds `transcript_persistence_caught_up(agent)` so a session whose live transcript has not reached disk is never soft-evicted.
- Soft vs hard teardown:
  - Soft (`_release_evicted_agent_soft` -> `release_clients()`, `run_agent.py:5001`): frees the httpx pool and per-turn child subagents but preserves terminal sandbox, browser daemon, background processes, and the memory provider so a rebuilt agent inherits them; the transcript is rebuilt from the persisted session next turn. Cap, idle, pressure, and cross-process invalidation are all soft.
  - Hard (`_cleanup_agent_resources` -> `agent.close()`, `run_agent.py:5065`): `shutdown_memory_provider` (fires `on_session_end`), `process_registry.kill_all(task_id)` (SIGTERM then SIGKILL after a ~2 s grace, `tools/process_registry.py:930-1048`), sandbox/browser/computer-use cleanup, then DB `end_session`. Reset (`/new`, `/reset` in `slash_commands.py:150`), true session expiry (`run.py:15540`), and shutdown (`run.py:16815-16837`, which clears the cache and hard-cleans every idle agent off-loop with a bound) are hard.
- One structural difference to keep in mind: Python has no memory or plugin child subprocess. The memory provider runs in-process and is torn down via `MemoryManager.shutdown_all()`. In the Rust port the extension host bundles the memory provider and plugin backends into one child, and the child's graceful `shutdown` request is the Rust analog of Python's `provider.shutdown()` / `on_session_end`. So the Rust extension-child teardown maps onto Python's provider teardown, not onto a Python subprocess kill.

---

## 2. Every path that pins a cached client and its child indefinitely

1. No cap. The map only grows: one entry per distinct `(home, session_id)` ever seen. Each entry pins a `NativeAgentClient`, and when extensions are configured, a live Python child, its provider connections, and its sqlite handles, for the whole gateway lifetime. This is the #80764 hoarding failure the pressure module was written for, now with a subprocess attached to each entry.

2. No idle TTL. A conversation touched once and never again keeps its entry and child forever. `idle_ttl_secs` resolves but nothing consumes it.

3. No pressure sweep. `read_anon_rss_mb` and `plan_pressure_evictions` exist but are never called against this map, so anonymous RSS growth cannot shed anything.

4. Reset orphans the old entry. `ConversationAgent`'s own header says "resets receive new IDs." A reset mints a new `session_id`, so `message_session_id` returns a new value, a new cell is created, and the old `(home, old_session_id)` entry is never looked up again and never removed. Its child stays alive until process exit. This is the sharpest indefinite-pin path: every reset leaks one child.

5. No explicit close. There is no `/new` / `/stop` hook into `ConversationAgent`. Even where the dispatcher bumps the run generation to invalidate a stale turn (`session_registry.rs:126-137`), the cache entry and its child are untouched.

6. Failed-init empty cells accumulate. A key whose factory fails leaves an empty `Arc<OnceCell>` in the map (retryable, correct) but the empty entry is never removed, so a stream of distinct failing keys grows the map unboundedly with empty cells. Small compared with (1) to (4), but real.

7. Shutdown teardown is ungraceful. On process exit the tokio runtime drops the worker tasks, so `Process::drop` / `kill_on_drop` SIGKILL each child's group. Children do die, but by SIGKILL, so the graceful `shutdown` request (and any provider flush behind it) is skipped. Whether the `Client` senders drop before the runtime aborts the worker is racy, so the graceful path in `run_worker` is not reliably taken at shutdown.

Net: (1) to (5) can pin a `NativeAgentClient` and its Python child for the gateway's whole life; (6) grows the map with empty cells; (7) is a teardown-quality gap at shutdown.

---

## 3. Proposed design

Goal: bound the map, preserve verbatim per-conversation prompt reuse, never kill an active turn's child out from under it, keep failed init retryable, hold no lock across provider or model I/O, and tear the extension child down promptly on reset, explicit close, idle expiry, pressure, and gateway shutdown. Reuse `AgentCacheBounds` and the `agent_cache_pressure` planner. No new config keys.

### 3.1 Data structures

Replace the bare map with a small metadata map behind a std mutex (it is never held across `.await`, so an async mutex is unnecessary and misleading):

```rust
use std::sync::Mutex;
use std::time::Instant;

type Key = (PathBuf, String);

struct Entry {
    cell: Arc<tokio::sync::OnceCell<Arc<dyn AgentClient>>>,
    last_used: Instant,   // MRU refresh point, drives LRU order and idle age
    active: u32,          // in-flight turns holding this entry; never evict when > 0
}

struct Inner {
    entries: HashMap<Key, Entry>,
}

pub struct ConversationAgent {
    fallback: Arc<dyn AgentClient>,
    factory: Box<Factory>,
    inner: Mutex<Inner>,
    bounds: AgentCacheBounds,      // from resolve_agent_cache_bounds(&user_config)
}
```

No ordered-map crate is needed. `last_used: Instant` plus a sort at sweep time gives LRU order, and the map is bounded to ~128 entries, so the sort is negligible. `plan_pressure_evictions` already wants entries in LRU to MRU order, which is `sort_by_key(last_used)` ascending.

### 3.2 Turn path with an RAII active guard

```rust
async fn run_turn_with_context(&self, context, msg, history, events) -> Result<()> {
    let Some(home) = context.home else { return self.run_turn(msg, history, events).await; };
    let key = (home.to_owned(), message_session_id(msg));

    // checkout: lock, get-or-insert, active += 1, last_used = now, enforce cap, clone cell.
    let (cell, _guard) = self.checkout(key.clone());   // _guard decrements active on drop

    let client = cell.get_or_try_init(|| async {
        (self.factory)(home, msg, history, context.database).await.map_err(...)
    }).await?.clone();                                  // I/O, no map lock

    client.run_turn(msg, history, events).await
    // _guard drops here (or on cancel/panic): checkin decrements active, refreshes last_used,
    // and removes the entry if it is now idle and still uninitialized (failed init).
}
```

`checkout` and the guard's `checkin` are the only two places that touch the map, and both are pure synchronous map ops:

```rust
fn checkout(&self, key: Key) -> (Arc<OnceCell<...>>, ActiveGuard) {
    let mut g = self.inner.lock().unwrap();
    let entry = g.entries.entry(key.clone()).or_insert_with(|| Entry {
        cell: Arc::new(OnceCell::new()), last_used: Instant::now(), active: 0,
    });
    entry.active += 1;
    entry.last_used = Instant::now();
    let cell = entry.cell.clone();
    let evicted = enforce_cap(&mut g, &self.bounds);   // collect Arcs to drop, do not drop under lock
    drop(g);
    drop(evicted);                                     // teardown fires here, off the lock
    (cell, ActiveGuard { agent: self, key })
}

// ActiveGuard::drop
fn checkin(&self, key: &Key) {
    let mut g = self.inner.lock().unwrap();
    if let Some(e) = g.entries.get_mut(key) {
        e.active = e.active.saturating_sub(1);
        e.last_used = Instant::now();
        if e.active == 0 && e.cell.get().is_none() {
            g.entries.remove(key);                     // failed init: drop empty cell, retryable
        }
    }
}
```

`checkin` is synchronous, so it runs even when the async turn is cancelled (Drop fires on the pending future). That is why the active count uses an RAII guard rather than a manual decrement.

### 3.3 Eviction helpers, all Drop-driven

Every eviction removes entries from the map under the lock, collects the removed `Entry` values (hence their `Arc<dyn AgentClient>`) into a local `Vec`, releases the lock, and then drops the `Vec`. Dropping a `NativeAgentClient` drops its `_extension_host` `Client` and every tool's `Client` clone; when that is the last clone, the detached worker performs the graceful `shutdown` then SIGKILLs the group. So teardown is asynchronous and non-blocking from the caller's view, matching Python's off-loop cleanup. `Client` has no custom `Drop` and its drop does no I/O, so even dropping the `Vec` under the lock would be cheap, but dropping it after unlock keeps the "no work under the lock" rule absolute.

```rust
pub fn sweep_idle(&self, now: Instant) {
    let ttl = effective_idle_ttl(&self.bounds);        // bounds.idle_ttl_secs or DEFAULT_IDLE_TTL_SECS
    let evicted = { let mut g = self.inner.lock().unwrap();
        let stale: Vec<Key> = g.entries.iter()
            .filter(|(_, e)| e.active == 0 && now.duration_since(e.last_used).as_secs_f64() > ttl)
            .map(|(k, _)| k.clone()).collect();
        stale.into_iter().filter_map(|k| g.entries.remove(&k)).collect::<Vec<_>>()
    };
    drop(evicted);
}

pub fn sweep_pressure(&self, rss_mb: impl Fn() -> Option<i64>) {
    let Some(budget) = self.bounds.memory_high_mb else { return; };
    let Some(rss) = rss_mb() else { return; };
    if rss <= budget { return; }
    let evicted = { let mut g = self.inner.lock().unwrap();
        let mut ordered: Vec<(Key, ())> = g.entries.iter()
            .map(|(k, e)| (k.clone(), e.last_used, e.active)).collect::<Vec<_>>()
            .also_sorted_by_last_used_ascending()      // LRU -> MRU
            .into_iter().map(|(k, _, active)| (k, active)).collect();
        let plan = plan_pressure_evictions(ordered_keys_with_active,
            |_k, active| *active == 0,                 // is_evictable = not in flight
            self.bounds.max_evictions_per_pass, self.bounds.protect_recent);
        plan.into_iter().filter_map(|(k, _)| g.entries.remove(&k)).collect::<Vec<_>>()
    };
    drop(evicted);
}

pub fn evict_conversation(&self, home: &Path, session_id: &str) {
    let key = (home.to_owned(), session_id.to_owned());
    let evicted = self.inner.lock().unwrap().entries.remove(&key);
    drop(evicted);                                     // reset/explicit close: unconditional
}

pub fn evict_all(&self) {
    let evicted: Vec<Entry> = std::mem::take(&mut self.inner.lock().unwrap().entries)
        .into_values().collect();
    drop(evicted);                                     // shutdown drain
}
```

`enforce_cap` (called from `checkout` after inserting) mirrors the sweeps: while `len > effective_max_size(bounds)`, remove the least-recently-used entry whose `active == 0`, collecting removed entries to drop after unlock. Like Python's `_enforce_agent_cache_cap`, it can deliberately stay over cap when the only candidates are active.

The `agent_cache_pressure.rs` additions are two consts and two accessors that only supply defaults for the existing `Option` fields, so no new config key appears:

```rust
pub const DEFAULT_MAX_SIZE: i64 = 128;         // mirrors Python _AGENT_CACHE_MAX_SIZE
pub const DEFAULT_IDLE_TTL_SECS: f64 = 3600.0; // mirrors Python _AGENT_CACHE_IDLE_TTL_SECS
pub fn effective_max_size(b: &AgentCacheBounds) -> usize { b.max_size.unwrap_or(DEFAULT_MAX_SIZE).max(1) as usize }
pub fn effective_idle_ttl(b: &AgentCacheBounds) -> f64 { b.idle_ttl_secs.unwrap_or(DEFAULT_IDLE_TTL_SECS) }
```

### 3.4 Never evicting an active turn, including reset

The active counter is the direct analog of Python's `running_ids` guard. `sweep_idle`, `sweep_pressure`, and `enforce_cap` all filter on `active == 0`, so a turn that has checked out its entry (increment under the map lock, before releasing it) is never evicted by a bound.

`evict_conversation` (reset, explicit close) is deliberately unconditional. Removing the map entry mid-turn is safe because the in-flight turn already holds its own `Arc<dyn AgentClient>` clone (from `cell.get_or_try_init(...).clone()`) and, inside `run_turn`, a per-turn `NativeAgentClient` clone with its own `Client` clones (`native_agent.rs:484`). So the child stays alive for the running turn and is torn down only when the turn's clones drop at turn end. Reset also bumps the run generation, so the stale turn's late result is dropped anyway. This satisfies "tear down promptly on reset" while never killing an active turn's child from under it. If `active == 0` at reset, teardown is immediate.

### 3.5 Wiring

- Construction (`main.rs:894-914`): build the concrete agent once, keep a typed handle, and coerce a clone to the trait object:
  ```rust
  let bounds = agent_cache_pressure::resolve_agent_cache_bounds(&user_config);
  let ca = Arc::new(conversation_agent::ConversationAgent::new(agent, factory, bounds));
  let agent: Arc<dyn AgentClient> = ca.clone();
  ```
- Sweep task: one background task like `memory_monitor::start_memory_monitoring`, ~300 s cadence (Python's watcher cadence), cancelled by the existing shutdown `CancellationToken`. Each tick calls `ca.sweep_idle(Instant::now())` then `ca.sweep_pressure(agent_cache_pressure::read_anon_rss_mb)`.
- Shutdown: when the shutdown token is cancelled (`main.rs:970-976`), call `ca.evict_all()` before the runtime is dropped. This drops every cached client, so each worker takes its graceful `shutdown` path instead of the runtime-abort SIGKILL. For a bounded graceful shutdown that actually waits for provider flush (Python's off-loop bounded `close()`), the extension host would need to expose the worker `JoinHandle` (today it is discarded at `extension_host.rs:216`) through a small registry that `evict_all` can join with a timeout. That await-the-children refinement is optional and can follow; `evict_all` plus the existing SIGKILL backstop already tears every child down at shutdown.
- Reset hook: at the site that mints a new `session_id` on reset (the Rust reset path built on `session_reset.rs` / the session store), call `ca.evict_conversation(home, old_session_id)`. If that reset path is not yet ported in this checkpoint, the bounds sweeps already reclaim the orphaned old entry within one idle-TTL or under pressure, so the explicit hook upgrades reset teardown from "eventual" to "prompt" rather than being load-bearing for correctness.

### 3.6 Lock order and race handling

Lock order. `ConversationAgent::inner` is a leaf mutex. It is acquired only for synchronous map mutations (checkout, checkin, the three sweeps, `enforce_cap`), never held across `.await`, never held while calling the factory, `OnceCell` init, the extension host, provider or model I/O, or while dropping evicted values. No other lock is taken while it is held, and it is never taken while holding `turn_lease`'s async mutex or `SessionRegistry`'s std mutex (checkout/checkin do not touch those), so it cannot participate in a deadlock cycle. This directly satisfies "avoid locks across provider/model I/O": all I/O is inside `OnceCell::get_or_try_init`, which holds only the per-key cell's own single-flight state, not the map lock, and distinct keys have distinct cells.

Races:

- Evict versus active turn. A turn increments `active` under the map lock before releasing it and starting I/O. A concurrent sweep taking the lock either sees `active > 0` and skips, or sees no entry yet (turn has not inserted) and has nothing to evict. There is no window where an entry is evicted after its turn incremented `active`.
- Evict versus in-flight init. Because `active > 0` for the duration, sweeps skip the entry during init. Even under an unconditional `evict_conversation`, the initializing turn holds its cloned `Arc<OnceCell>`, so its init and run complete against a valid cell; only reuse is lost, which is acceptable for a reset.
- Concurrent init failure. `OnceCell` runs one initializer; other checkouts on the same key await it and all receive the same `Err`. Each holds its own guard, so each decrements `active`; the last one out (`active == 0`, `cell.get().is_none()`) removes the empty entry. Retryable and no empty-cell leak.
- Drop cost under the lock. Avoided: evicted entries are collected and dropped after the guard is released. `Client` drop is non-blocking regardless.

---

## 4. Tests

Unit tests extend `conversation_agent.rs`'s existing `RecordedAgent` harness. Inject `now: Instant` into `sweep_idle` and an `rss_mb` closure into `sweep_pressure` so age and pressure are deterministic (the pure `plan_pressure_evictions` is already unit-tested in `agent_cache_pressure.rs`).

1. `eviction_skips_active_entry`: hold a turn on key A (a factory that blocks on a barrier so `active` stays 1), run `sweep_pressure` forcing the budget below RSS, assert A survives and an idle key B is removed.
2. `lru_cap_evicts_oldest_nonactive`: `max_size = 2`, touch keys A, B, C in order, assert A is removed, B and C remain, count == 2, and MRU order respects `last_used`.
3. `idle_ttl_evicts_only_stale`: set A's `last_used` to `now - 2*ttl` via the injected clock, `sweep_idle`, assert A removed and a fresh B kept.
4. `failed_init_is_retryable_and_not_leaked`: factory bails on the first call for a key, assert the entry is removed (`len() == 0`) and a second turn rebuilds successfully.
5. `reset_evicts_but_inflight_turn_completes`: start a blocking turn on A (active), call `evict_conversation(A)`, assert the entry is gone immediately, then release the barrier and assert the turn still completes against its held clone.
6. `sweep_does_not_block_on_inflight_init`: start a turn whose factory awaits a barrier (init in flight, map lock not held), run a sweep on another thread and assert it returns without blocking, proving I/O is outside the map lock.

Real child teardown evidence (integration, current-thread runtime, reusing the fixture harness already in `extension_host.rs:654-918` where `FixtureProvider.shutdown` writes `extension-shutdown-<session>`):

7. Build a `ConversationAgent` whose factory spawns a real extension host against the fixture plugin. For each teardown trigger, assert the shutdown sentinel file appears (proves the Python child received the graceful `shutdown` RPC, i.e. real teardown rather than an orphan) and that `libc::kill(pid, 0)` returns `ESRCH` (the OS process is gone):
   - a. `evict_conversation` -> sentinel appears, pid gone.
   - b. LRU cap overflow evicting the LRU entry -> its sentinel appears.
   - c. `sweep_idle` past TTL -> sentinel appears.
   - d. `sweep_pressure` with a forced budget of 1 -> sentinel appears.
   - e. `evict_all` -> sentinel appears.
   - f. Never-evict-active: hold a turn active, force pressure, assert the sentinel does not appear until the turn completes and its clones drop.

Test 7 gives the concrete "real child teardown evidence" the checkpoint asks for, on the exact fixture that the extension-host tests already exercise.

---

## 5. This checkpoint versus deferred work

Implementable in this checkpoint:

- Bounded `ConversationAgent` map: LRU cap, idle TTL, and pressure sweep wired to the existing `AgentCacheBounds` and `agent_cache_pressure` planner, with only two default consts added (no new config key).
- Per-entry `active` counter and RAII checkin, so no bound ever evicts an in-flight turn.
- Failed-init stays retryable and no longer leaks empty cells.
- Drop-driven teardown on every trigger: `evict_conversation` (reset / explicit close), `sweep_idle`, `sweep_pressure`, `enforce_cap`, and `evict_all` (shutdown).
- Background sweep task and the shutdown-time `evict_all`, plus the reset hook where the reset path exists.
- The full test set, including real child teardown evidence.

Deferred to the broader prompt-invalidation and compression work (out of scope here):

- Config-signature invalidation. Python rebuilds when `cached[1] != sig` (config drift mid-conversation). The Rust port deliberately freezes the prompt, tool prefix, and plugin snapshot for the conversation (`native_agent.rs:207-219`), so signature-driven rebuild belongs with the prompt-invalidation work, not with eviction.
- Rotation-stable keying (#54947). Python keys by `session_key` and reuses across a rotated `session_id`; Rust keys by `session_id`, so a rotation makes a new entry. Unifying under a rotation-stable key is tied to compression session rotation, which `turn_lease::rebind` already anticipates. Adjacent: `gateway_session_key` stability across resets so external-memory providers do not re-bucket (extension-host review finding 4).
- Soft versus hard eviction split. Python preserves terminal sandbox, browser daemon, background processes, and the memory provider across soft evictions. Rust owns none of those subsystems yet, and the extension child bundles memory and plugins into one process, so there is a single teardown kind (drop equals full close). The split lands when those subsystems are ported.
- `transcript_persistence_caught_up` guard. Rust native persistence is synchronous around the turn (persist-before-I/O), and the `active` counter already protects in-flight turns, so the extra "transcript flushed to disk" guard is only needed once background/async transcript flushing exists.
- Mid-conversation extension-host respawn after an unhealthy worker (a separate known gap: once a worker is marked unhealthy there is no reconnect). Bounding plus rebuild-on-next-turn is the interim recovery, but true mid-conversation respawn is its own change.
- Graceful bounded shutdown that awaits each child's provider `on_session_end` within a timeout, via exposing the worker `JoinHandle`. `evict_all` plus the SIGKILL backstop already tears every child down at shutdown; the await-for-flush refinement is optional polish.
