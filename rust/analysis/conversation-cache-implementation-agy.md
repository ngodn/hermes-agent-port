# Bounded ConversationAgent Ownership & Extension-Child Teardown: Source-Grounded Architecture & Review

**Location**: [`rust/analysis/conversation-cache-implementation-agy.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-cache-implementation-agy.md)  
**Status**: Concrete Implementation Design  
**Author**: Antigravity (Advanced Agentic Coding Pair)  
**Date**: September 2026  

---

## Executive Summary

The Hermes Rust gateway port (`rust/crates/hermes-gateway`) currently caches native per-conversation clients in [`ConversationAgent`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L28-L32). While this preserves upstream LLM prompt-cache prefixes across turns, the cache is completely unbounded: it has no capacity ceiling, no idle-TTL expiration, no memory-pressure shed, and no teardown on conversation reset or shutdown. Furthermore, each cached [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L248-L252) pins a live Python extension host subprocess (`python -m hermes_cli.rust_extension_host`), including its SQLite connections and memory-provider handles.

This document reviews the existing analysis in [`rust/analysis/conversation-eviction-map-claude.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md), audits the production Rust and Python codebases, exposes five critical race and teardown flaws in the previous design, and establishes a source-grounded implementation specification for bounded [`ConversationAgent`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L28-L32) ownership.

### Core Verification Matrix

| Area | Current Rust State (Verified Fact) | Python Reference (Verified Fact) | Proposed Rust Specification |
| :--- | :--- | :--- | :--- |
| **Cache Storage** | [`HashMap<(PathBuf, String), Arc<OnceCell>>`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L26) behind [`tokio::sync::Mutex`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L31); grows indefinitely. | `OrderedDict[session_key -> tuple]` behind `threading.Lock` ([`gateway/run.py:7792-7793`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7792-L7793)). | Bounded `HashMap` behind `std::sync::Mutex<Inner>` tracking `last_used`, `active`, and `finalizer_pending_until`. |
| **Capacity Cap** | None. | `_AGENT_CACHE_MAX_SIZE = 128` ([`gateway/run.py:85`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L85)), LRU-evicted on insert ([`gateway/run.py:30578-30660`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30578-L30660)). | `DEFAULT_MAX_SIZE = 128`, LRU-evicted during `checkout` while skipping in-flight turns. |
| **Idle TTL** | None. [`AgentCacheBounds.idle_ttl_secs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L49) parsed but unused. | `_AGENT_CACHE_IDLE_TTL_SECS = 3600.0` ([`gateway/run.py:86`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L86)), swept on ~300s timer ([`gateway/run.py:30661-30749`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30661-L30749)). | `DEFAULT_IDLE_TTL_SECS = 3600.0`, swept via periodic background sweep task. |
| **Memory Pressure** | [`agent_cache_pressure.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L1-L320) ported but uncalled (`#![allow(dead_code)]`). | `_sweep_agent_cache_under_pressure` ([`gateway/run.py:30434-30548`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30434-L30548)) reads anon RSS against cgroup budget. | Wire [`read_anon_rss_mb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L255-L270) and [`plan_pressure_evictions`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L291-L320) to `ConversationAgent::sweep_pressure`. |
| **Active Turn Guard** | None. Cache is lookup-only. | `running_ids = {id(a) for _, a in self._running_agent_items()}` guards all eviction paths ([`gateway/run.py:30245`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30245)). | `active: u32` counter with RAII guard + `finalizer_pending_until: Option<Instant>` post-persist window. |
| **Finalizer Safety** | [`finalize_turn_after_persist`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L113-L135) looks up map; silent no-op if missing. | In-process agent kept alive throughout turn persistence and delivery. | State machine holds entry non-evictable until finalizer completes or 30s timeout expires. |
| **Teardown Sequence** | Only drop-driven SIGKILL on process exit ([`extension_host.rs:133-144`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L133-L144)). | `flush_pending` -> `on_session_end(messages)` -> `shutdown_all()` -> close ([`gateway/run.py:12380-12420`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L12380-L12420)). | Structured 3-step sequence: `flush_pending` RPC -> `session_end` RPC -> `shutdown` RPC -> graceful child exit. |
| **Live Reset Hook** | None. Reset mints new ID; old entry & child leak forever. | `_evict_cached_agent(session_key)` ([`gateway/run.py:15412`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L15412), [`21391`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L21391)). | Hook in [`dispatch.rs:334`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L334) calling `ca.evict_conversation(home, prev_id)` when `was_auto_reset` fires. |
| **Process Shutdown** | Runtime drops detached workers; children SIGKILLed abruptly ([`main.rs:1078-1090`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1078-L1090)). | `_finalize_shutdown_agents` + drain `_agent_cache` with timeout ([`gateway/run.py:16815-16838`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L16815-L16838)). | `ca.shutdown(timeout).await` right after `axum::serve` returns in [`main.rs:1080`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1080). |

---

## 1. Audit of the Existing Claude Eviction Map: Critical Errors & Risky Assumptions

The previous design document, [`rust/analysis/conversation-eviction-map-claude.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md), correctly identified that `ConversationAgent` never bounds its map and leaks Python child processes. However, a rigorous audit against [`dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs), [`message.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs), [`native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs), [`extension_host.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs), and [`hermes_cli/rust_extension_host.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py) reveals five critical errors and risky assumptions:

### Error 1: The Post-Persist Finalizer Race Condition (High Severity)
* **Claude's Assumption** ([`conversation-eviction-map-claude.md:176-185`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md#L176-L185)):  
  Claude proposed an RAII `ActiveGuard` dropped at the exit of `run_turn_with_context`:
  ```rust
  let (cell, _guard) = self.checkout(key.clone());
  ...
  client.run_turn(msg, history, events).await;
  // _guard drops here: decrements active to 0
  ```
* **Source-Grounded Reality**:  
  In both push turns ([`dispatch.rs:402-464`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L402-L464)) and HTTP turns ([`message.rs:200-247`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L200-L247)), turn execution is a **two-phase** process:
  1. `run_turn_with_context` streams chunks and completes (`agent_task.await` or `turn.await`).
  2. The gateway records the assistant reply in SQLite via [`crate::session_db::end_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L453) (synchronous disk I/O).
  3. The gateway invokes [`agent.finalize_turn_after_persist(context, &msg, &reply, succeeded).await`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L454-L464).
  
  In [`conversation_agent.rs:127-130`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L127-L130), `finalize_turn_after_persist` looks up the entry by key in `self.clients`. If `_guard` dropped at step 1, `active` became 0 during step 2. Any concurrent insert (`enforce_cap`), idle sweep, or pressure pass during the SQLite write can evict the entry!  
  When step 3 executes, `self.clients.get(&key)` returns `None`, and `finalize_turn_after_persist` silently returns `Ok(())`!
* **Consequence**:  
  In [`native_agent.rs:701-716`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L701-L716), `finalize_turn_after_persist` takes `pending_memory_turn` and sends `turn_complete` to the Python extension host. If the entry was evicted, `turn_complete` is never sent, the assistant reply is never recorded in external memory, and conversational recall is permanently broken for that turn!

### Error 2: False Equivalence of Drop-Driven `shutdown` and `on_session_end` / `flush_pending` (High Severity)
* **Claude's Assumption** ([`conversation-eviction-map-claude.md:110`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md#L110), [`351`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md#L351)):  
  Claude claimed: *"the child's graceful shutdown request is the Rust analog of Python's provider.shutdown() / on_session_end"* and dismissed awaiting `on_session_end` / `flush_pending` as "optional polish."
* **Source-Grounded Reality**:  
  In [`hermes_cli/rust_extension_host.py:416-429`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L416-L429), the `shutdown` method executes:
  ```python
  def shutdown(self) -> None:
      if self._memory_manager is not None:
          self._memory_manager.shutdown_all()
          self._memory_manager = None
      if self._plugin_manager is not None:
          self._plugin_manager.unload()
  ```
  `shutdown_all()` ([`agent/memory_manager.py:1339-1357`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L1339-L1357)) **does not call** `on_session_end`! `on_session_end` is only invoked when the explicit [`session_end`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L395-L404) RPC is sent with `messages`.  
  Furthermore, [`turn_complete`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L363-L394) dispatches memory sync tasks to a background single-worker thread pool (`_sync_executor`). In [`gateway/run.py:12380-12397`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L12380-L12397), Python explicitly calls `flush_pending(timeout=10)` to drain queued memory writes before tearing down providers.
* **Consequence**:  
  Drop-driven teardown alone completely skips `on_session_end` and can abort in-flight SQLite/external memory sync writes queued during the final turns.

### Error 3: Detached Worker Abort at Process Shutdown (Teardown Race)
* **Claude's Assumption** ([`conversation-eviction-map-claude.md:289-293`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md#L289-L293)):  
  Claude claimed: *"when the shutdown token is cancelled, call ca.evict_all() before the runtime is dropped. This drops every cached client, so each worker takes its graceful shutdown path instead of the runtime-abort SIGKILL."*
* **Source-Grounded Reality**:  
  In [`extension_host.rs:224`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L224), the worker task is detached:
  ```rust
  tokio::spawn(run_worker(process, receiver));
  ```
  The `JoinHandle` is discarded. When `ca.evict_all()` drops the `Client` senders, `receiver.recv().await` returns `None`, starting `run_worker`'s graceful shutdown. But `ca.evict_all()` returns synchronously! In [`main.rs:1080-1090`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1080-L1090), `main()` immediately proceeds to exit. When `main()` returns, the Tokio runtime drops and **cancels all running tasks immediately**.  
  `run_worker` is cancelled mid-flight, triggering [`Process::drop`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L133-L144):
  ```rust
  unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
  ```
* **Consequence**:  
  Every child process is still SIGKILLed on gateway shutdown. Dropping senders without an async await barrier is ineffective.

### Error 4: Ambiguous Keying on Conversation Reset
* **Claude's Assumption** ([`conversation-eviction-map-claude.md:293-294`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md#L293-L294), [`347`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md#L347)):  
  Claude deferred rotation-stable keying without addressing the concrete eviction key for resets.
* **Source-Grounded Reality**:  
  In Python ([`gateway/run.py:7792`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7792)), the cache is keyed on the stable `session_key` (e.g. `cli:chat-1`), and `session_id` lives in the value. When a reset occurs, Python calls `_evict_cached_agent(session_key)`.  
  In Rust ([`conversation_agent.rs:83`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L83)), the cache is keyed on `(home: PathBuf, session_id: String)`. When [`SessionStore::transition`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L131-L270) detects an auto-reset or forced reset, it creates a new `SessionEntry` with a *new* `session_id` and records the prior ID in [`context.prev_session_id`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_entry.rs#L76).  
  Calling `evict_conversation(home, &entry.session_id)` would look for the *new* ID (which is not cached yet) and fail to evict the orphaned old client!
* **Consequence**:  
  The eviction call at reset must explicitly use `prev_session_id`, or the old child process remains leaked.

### Error 5: Synchronous `drop(evicted)` Disregards Child Resource Cleanup
* **Claude's Assumption** ([`conversation-eviction-map-claude.md:199-202`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-eviction-map-claude.md#L199-L202)):  
  Claude insisted that all eviction functions should synchronously collect `Entry` values and drop them outside the lock, claiming teardown is "purely drop-driven."
* **Source-Grounded Reality**:  
  Dropping `NativeAgentClient` drops the channel sender, but provides zero guarantee of write completion. In Python ([`gateway/run.py:30254-30266`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30254-L30266), [`30536-30543`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30536-L30543), [`30654-30660`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30654-L30660)), eviction spawns worker threads (`_commit_then_release_soft`) to run bounded `flush_pending` and `commit_memory_session` before dropping client sockets. Rust must decouple cache mutation from asynchronous teardown.

---

## 2. Deep, Small `ConversationAgent` Interface & Architecture

### Design Principles (Ousterhout's Depth)
A deep interface exposes a minimal, straightforward surface to callers while hiding complex concurrency, single-flight lazy initialization, LRU tracking, memory pressure resolution, and subprocess lifecycle mechanics underneath:
- **Leaf Mutex**: The internal state is protected by a synchronous `std::sync::Mutex`. It is **never held across `.await`**, never held during factory execution, never held during provider I/O, and never held while executing teardown RPCs.
- **Two-Phase Turn Lifecycle**: The cache natively tracks active turn execution and post-persist finalization windows so that `finalize_turn_after_persist` is guaranteed to find its live client.
- **Decoupled Eviction & Teardown**: Eviction removes entries from the map synchronously under the lock and returns them for asynchronous background drain.

### Core Data Structures

```rust
// In crates/hermes-gateway/src/conversation_agent.rs

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;
use crate::agent::{AgentClient, TurnContext};
use crate::agent_cache_pressure::AgentCacheBounds;

pub type Key = (PathBuf, String);
type ClientCell = OnceCell<Arc<dyn AgentClient>>;

/// Tracks conversation entry lifecycle state.
struct Entry {
    cell: Arc<ClientCell>,
    last_used: Instant,
    /// Number of concurrent callers actively executing run_turn_with_context.
    active: u32,
    /// Absolute deadline until which the entry is protected for end_turn + finalize_turn_after_persist.
    finalizer_pending_until: Option<Instant>,
}

impl Entry {
    fn new() -> Self {
        Self {
            cell: Arc::new(OnceCell::new()),
            last_used: Instant::now(),
            active: 0,
            finalizer_pending_until: None,
        }
    }

    /// True if the entry is actively processing a turn or awaiting post-persist finalization.
    fn is_in_use(&self, now: Instant) -> bool {
        self.active > 0 || self.finalizer_pending_until.map_or(false, |until| now < until)
    }
}

struct Inner {
    entries: HashMap<Key, Entry>,
}

pub struct ConversationAgent {
    fallback: Arc<dyn AgentClient>,
    factory: Box<Factory>,
    inner: Mutex<Inner>,
    bounds: AgentCacheBounds,
}
```

### The Deep Public Surface

```rust
impl ConversationAgent {
    /// Construct with operator-configured cache bounds.
    pub fn new(
        fallback: Arc<dyn AgentClient>,
        factory: impl for<'a> Fn(
                &'a Path,
                &'a Message,
                &'a [crate::session_db::HistoryMessage],
                Option<&'a crate::session_db::SessionDb>,
            ) -> FactoryFuture<'a>
            + Send + Sync + 'static,
        bounds: AgentCacheBounds,
    ) -> Self;

    /// Checkout an entry for turn execution, enforcing capacity limits.
    /// Returns the single-flight cell and an RAII ActiveGuard.
    pub fn checkout(&self, key: Key) -> (Arc<ClientCell>, ActiveGuard);

    /// Evict entries idle past the configured idle TTL. Returns evicted clients for teardown.
    pub fn sweep_idle(&self, now: Instant) -> Vec<Arc<dyn AgentClient>>;

    /// Evict LRU entries under memory pressure using the provided RSS probe closure.
    pub fn sweep_pressure(&self, rss_mb: impl Fn() -> Option<i64>) -> Vec<Arc<dyn AgentClient>>;

    /// Explicitly evict a specific conversation (e.g. on session reset or expiry).
    pub fn evict_conversation(&self, home: &Path, session_id: &str) -> Option<Arc<dyn AgentClient>>;

    /// Gracefully evict and drain all cached clients on gateway shutdown.
    pub async fn shutdown(&self, timeout: Duration) -> usize;

    /// Current number of entries in the cache (diagnostics/tests).
    pub fn len(&self) -> usize;

    /// Inspect in-use status of a specific entry (tests).
    pub fn is_entry_in_use(&self, home: &Path, session_id: &str) -> bool;
}
```

---

## 3. Active-Turn & Post-Persist-Finalizer Race Safety

### The Execution Timeline & Race Window

In [`dispatch.rs:383-472`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L383-L472) and [`message.rs:215-265`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L215-L265), turn handling follows this exact sequence:

```
[Phase 1: In-Flight Model Turn]
  1. ConversationAgent::run_turn_with_context()
     - checkout(key) -> active += 1
     - OnceCell::get_or_try_init (single-flight build)
     - client.run_turn_with_context()
     - active -= 1
     - finalizer_pending_until = now + 30s  <--- CRITICAL PROTECTION ARMED

[Inter-Phase Window: Synchronous Disk I/O]
  2. crate::session_db::end_turn() (writes assistant reply to SQLite)
     * Concurrent sweeps check: entry.is_in_use(now) == TRUE
     * Result: Entry CANNOT be evicted by LRU cap, idle TTL, or memory pressure!

[Phase 2: Post-Persist Finalization]
  3. ConversationAgent::finalize_turn_after_persist()
     - Lookup key in inner.entries -> GUARANTEED HIT
     - client.finalize_turn_after_persist() -> external-memory turn_complete sent
     - finalizer_pending_until = None       <--- PROTECTION SAFELY DISARMED
     - last_used = now
```

### RAII Guard & State Transition Implementation

```rust
pub struct ActiveGuard<'a> {
    agent: &'a ConversationAgent,
    key: Key,
    armed_finalizer: bool,
}

impl<'a> Drop for ActiveGuard<'a> {
    fn drop(&mut self) {
        let mut g = self.agent.inner.lock().unwrap();
        if let Some(entry) = g.entries.get_mut(&self.key) {
            entry.active = entry.active.saturating_sub(1);
            if self.armed_finalizer {
                // Protect the entry for up to 30 seconds for end_turn + finalizer.
                entry.finalizer_pending_until = Some(Instant::now() + Duration::from_secs(30));
            }
            // Failed initialization cleanup: if uninitialized and idle, remove cell.
            if entry.active == 0 && entry.cell.get().is_none() {
                g.entries.remove(&self.key);
            }
        }
    }
}
```

In `AgentClient for ConversationAgent`:

```rust
#[async_trait]
impl AgentClient for ConversationAgent {
    async fn run_turn_with_context(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
        events: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let Some(home) = context.home else {
            return self.run_turn(msg, history, events).await;
        };
        let key = (home.to_owned(), crate::session_db::message_session_id(msg));

        // 1. Checkout under leaf lock
        let (cell, mut guard) = self.checkout(key);

        // 2. Single-flight initialize without map lock
        let client = cell
            .get_or_try_init(|| async {
                (self.factory)(home, msg, history, context.database)
                    .await
                    .map_err(|error| {
                        hermes_core::Error::Other(format!(
                            "conversation agent initialization failed: {error}"
                        ))
                    })
            })
            .await?
            .clone();

        // 3. Execute turn
        let result = client.run_turn_with_context(context, msg, history, events).await;
        if result.is_ok() {
            guard.armed_finalizer = true;
        }
        result
    }

    async fn finalize_turn_after_persist(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        reply: &str,
        succeeded: bool,
    ) -> Result<()> {
        let Some(home) = context.home else {
            return self.fallback.finalize_turn_after_persist(context, msg, reply, succeeded).await;
        };
        let key = (home.to_owned(), crate::session_db::message_session_id(msg));

        // Retrieve client and disarm finalizer window under leaf lock
        let client = {
            let mut g = self.inner.lock().unwrap();
            if let Some(entry) = g.entries.get_mut(&key) {
                entry.finalizer_pending_until = None;
                entry.last_used = Instant::now();
                entry.cell.get().cloned()
            } else {
                None
            }
        };

        let Some(client) = client else {
            return Ok(());
        };

        client.finalize_turn_after_persist(context, msg, reply, succeeded).await
    }
}
```

### Safety Proof: Invariant Guarantees
1. **Zero Lock Across I/O**: `self.inner.lock()` is acquired only to update primitive integers and `Instant` timestamps; it is held for microseconds. The factory future, model HTTP streaming, and SQLite I/O run completely unlocked.
2. **Never Evict Mid-Turn**: Any entry with `active > 0` evaluates `is_in_use(now) == true` and is skipped by `enforce_cap`, `sweep_idle`, and `sweep_pressure`.
3. **Never Evict During Finalizer Window**: Between turn return and finalization, `is_in_use(now) == true` via `finalizer_pending_until`.
4. **Panic / Cancellation Safety**: If an inbound connection cancels or panics before calling `finalize_turn_after_persist`, the 30-second deadline automatically expires on subsequent clock reads, ensuring abandoned entries are eventually reclaimed.

---

## 4. Exact Default and Config Compatibility

The Rust port must match Python's runtime configuration verbatim. All configuration lives under `agent.agent_cache` in `$HERMES_HOME/config.yaml`. Zero new configuration keys are added.

### Configuration Constants

Add the following public defaults to [`agent_cache_pressure.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs):

```rust
// Matches Python gateway/run.py:85
pub const DEFAULT_MAX_SIZE: usize = 128;

// Matches Python gateway/run.py:86
pub const DEFAULT_IDLE_TTL_SECS: f64 = 3600.0;

/// Return effective max entries: configured override, or 128 default. Minimum 1.
pub fn effective_max_size(bounds: &AgentCacheBounds) -> usize {
    bounds.max_size.map(|v| v.max(1) as usize).unwrap_or(DEFAULT_MAX_SIZE)
}

/// Return effective idle TTL in seconds: configured override, or 3600.0 default.
pub fn effective_idle_ttl_secs(bounds: &AgentCacheBounds) -> f64 {
    bounds.idle_ttl_secs.unwrap_or(DEFAULT_IDLE_TTL_SECS)
}
```

### Bounds Resolution Verification

In [`agent_cache_pressure.rs:217-249`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L217-L249), [`resolve_agent_cache_bounds`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L217) parses:
- `max_size`: integer > 0 (or `None` -> 128)
- `idle_ttl_secs`: float > 0 (or `None` -> 3600.0)
- `memory_high_mb`: `"auto"` (calculates `0.65 * cgroup/RAM` with 512MB floor), numeric MB, or `None` if disabled
- `max_evictions_per_pass`: default 16
- `protect_recent`: default 8 (supports explicit `0` to shed all non-active entries)

---

## 5. Teardown Sequencing: `session_end`, `flush_pending`, and Child Shutdown

In Python, [`gateway/run.py:12380-12420`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L12380-L12420) and [`run_agent.py:4882-4909`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L4882-L4909) sequence teardown to guarantee data durability. In Rust, the Python child subprocess is hosted via [`extension_host::Client`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L116-L119), which speaks JSONL over stdin/stdout to [`hermes_cli/rust_extension_host.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py).

### Public Extension Host Client Additions

[`extension_host::Client`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L116-L119) must expose public async methods for the underlying protocol operations:

```rust
impl Client {
    /// Drain queued memory manager writes behind its serialized background queue.
    pub async fn flush_pending(&self, timeout: Duration) -> Result<bool> {
        let value = self.request(
            "flush_pending",
            json!({"timeout": timeout.as_secs_f64()}),
            timeout + Duration::from_secs(1),
        ).await?;
        Ok(value.as_bool().unwrap_or(false))
    }

    /// Commit end-of-session memory provider state with the conversation history.
    pub async fn session_end(&self, messages: &[serde_json::Value]) -> Result<()> {
        self.request(
            "session_end",
            json!({"messages": messages}),
            Duration::from_secs(15),
        ).await?;
        Ok(())
    }
}
```

### The Three Eviction Pipelines

```
                   [Eviction Event]
                          │
         ┌────────────────┼────────────────┐
         ▼                ▼                ▼
   [Cache Cap /      [Session Reset /     [Gateway
    Idle TTL /        Session Expiry]     Shutdown]
   Pressure Shed]         │                │
         │                ▼                ▼
         │         1. Load History       1. Parallel Fan-Out
         │            from SQLite           across all cached
         │                │                 clients
         │                ▼                │
         └────────► 2. flush_pending ◄─────┘
                       (2s - 5s timeout)
                          │
                          ▼
                    3. session_end
                       (messages: history)
                          │
                          ▼
                    4. Drop Client Clones
                       (triggers worker shutdown RPC: 7s timeout)
                          │
                          ▼
                    5. Process Closes Stdin
                       Child Exits Gracefully
                          │ (if timeout)
                          ▼
                    6. SIGKILL Backstop
```

1. **Cache Eviction (LRU Cap, Idle TTL, Pressure Sweep)**:
   - Call [`flush_pending(2s)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L933-L942) so background sync tasks queued by `turn_complete` land in SQLite.
   - Drop the client. Worker sends `shutdown` RPC, drains, and exits.
2. **Session Reset / Expiry (True Conversation Boundary)**:
   - Call [`flush_pending(5s)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L933-L942).
   - Call [`session_end(messages)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L944-L951) with session history so memory providers perform long-term extraction.
   - Call [`flush_pending(5s)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L933-L942) to persist extraction writes.
   - Drop the client.
3. **Gateway Process Shutdown**:
   - Concurrently fan out across all cached clients with a global bounded timeout (e.g. 5 seconds).
   - Execute `flush_pending`, drop senders, and await worker exit before dropping the Tokio runtime.

---

## 6. Where Live Reset, Expiry, and Gateway Shutdown Call It Today

### 1. Live Reset Hook Site: [`dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs)

In [`dispatch.rs:320-347`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L320-L347), incoming messages resolve their session against [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16). When a reset policy triggers (idle reset, daily reset, or `/new`), [`SessionStore::transition`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L243-L256) mints a new `session_id` and records `prev_session_id`.

**Exact Hook Insertion** ([`dispatch.rs:334`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L334)):
```rust
match resolved {
    Ok(Ok((entry, db))) => {
        // DETECT RESET AND EVICT ORPHANED PREDECESSOR
        if let Some(prev_id) = entry.fields.get("prev_session_id").and_then(|v| v.as_str()) {
            if let Some(home) = db.as_ref().and_then(|db| db.profile_home()) {
                info!(prev_session_id = prev_id, new_session_id = %entry.session_id, "evicting reset conversation agent");
                self.agent.evict_conversation(home, prev_id);
            }
        }
        msg.resolved_session_id = Some(entry.session_id);
        routing_key = Some(entry.session_key);
        turn_db = db;
    }
```

### 2. Periodic Expiry & Pressure Sweep Task: [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)

In [`main.rs:989`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L989), `memory_monitor::start_memory_monitoring` starts a 300s timer. We start a dedicated cache sweep task alongside it:

**Exact Hook Insertion** ([`main.rs:990`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L990)):
```rust
// Start periodic agent-cache idle & pressure sweep task (300s cadence)
let sweep_agent = ca.clone();
let sweep_shutdown = shutdown.clone();
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    loop {
        tokio::select! {
            _ = sweep_shutdown.cancelled() => break,
            _ = interval.tick() => {
                let idle_evicted = sweep_agent.sweep_idle(Instant::now());
                let pressure_evicted = sweep_agent.sweep_pressure(agent_cache_pressure::read_anon_rss_mb);
                if !idle_evicted.is_empty() || !pressure_evicted.is_empty() {
                    tracing::info!(
                        idle = idle_evicted.len(),
                        pressure = pressure_evicted.len(),
                        "agent cache sweep evicted stale clients"
                    );
                    // Spawn off-loop background drain for evicted clients
                    tokio::spawn(async move {
                        for client in idle_evicted.into_iter().chain(pressure_evicted) {
                            let _ = client.flush_pending(Duration::from_secs(2)).await;
                            drop(client);
                        }
                    });
                }
            }
        }
    }
});
```

### 3. Gateway Graceful Shutdown: [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)

In [`main.rs:1078-1081`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1078-L1081), `axum::serve` completes when `shutdown.cancelled()` fires:

**Exact Hook Insertion** ([`main.rs:1081`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1081)):
```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async move { shutdown.cancelled().await })
    .await?;

// GRACEFUL CACHE DRAIN BEFORE RUNTIME SHUTDOWN
tracing::info!("draining conversation agent cache");
let drained = ca.shutdown(Duration::from_secs(5)).await;
tracing::info!(drained, "conversation agent cache drained successfully");
```

---

## 7. Deterministic Unit Tests & Python Integration Test Plan

### Part 1: Deterministic Unit Tests ([`conversation_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs))

1. **`test_lru_cap_evicts_oldest_idle_entry`**:
   - Set `max_size: 2`.
   - Run turns on session A, session B, session C.
   - Assert cache size == 2, keys B and C remain, key A was evicted.
2. **`test_active_turn_is_never_evicted_by_cap`**:
   - Set `max_size: 1`.
   - Start turn on session A with a barrier that holds inside `run_turn_with_context`.
   - Start turn on session B.
   - Assert cache size == 2 (stays temporarily over cap).
   - Release barrier on A. A completes. Next insert cleans up A.
3. **`test_post_persist_finalizer_race_safety`**:
   - Set `max_size: 1`.
   - Complete `run_turn_with_context` on session A. `ActiveGuard` drops, arming 30s finalizer window.
   - Call `enforce_cap()`.
   - Assert session A is **not** evicted because `is_in_use` is true.
   - Call `finalize_turn_after_persist` on session A. Assert it succeeds.
   - Finalizer window disarms. Subsequent `enforce_cap()` evicts A.
4. **`test_idle_ttl_sweep_with_injected_clock`**:
   - Initialize session A and B.
   - Advance mock clock past 3600s for session A. Keep session B fresh.
   - Run `sweep_idle(mock_now)`.
   - Assert session A is evicted, session B is retained.
5. **`test_memory_pressure_sweep_respects_protect_recent`**:
   - Insert 6 entries (A, B, C, D, E, F) in LRU order.
   - Mock RSS = 2000MB, `memory_high_mb = 1000MB`, `protect_recent = 2`, `max_evictions = 2`.
   - Run `sweep_pressure`.
   - Assert entries A and B evicted; C, D, E, F preserved.
6. **`test_failed_initialization_is_retryable_and_does_not_leak`**:
   - Factory returns error on first attempt for session A.
   - Assert error is propagated.
   - Assert map contains 0 entries (empty cell cleaned up).
   - Second turn succeeds and creates valid cached client.
7. **`test_lock_safety_no_blocking_during_slow_factory`**:
   - Thread 1 runs factory for session A, blocked on an async barrier.
   - Thread 2 immediately checks out or sweeps session B.
   - Assert Thread 2 acquires map lock without delay, proving factory runs unlocked.

### Part 2: Real Python-Child Integration Test

Reusing the fixture infrastructure from [`extension_host.rs:700-978`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L700-L978):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_real_python_child_eviction_and_graceful_teardown() {
    let home = TempHome::new("real-eviction");
    let plugin = home.0.join("plugins/fixture-extension");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        home.0.join("config.yaml"),
        "plugins:\n  enabled: [fixture-extension]\nmemory:\n  provider: fixture-extension\nagent:\n  agent_cache:\n    max_size: 1\n",
    ).unwrap();
    
    // Write FixtureProvider plugin that records shutdown to a sentinel file
    std::fs::write(plugin.join("plugin.yaml"), "name: fixture-extension\nversion: 1.0.0\nkind: exclusive\n").unwrap();
    std::fs::write(plugin.join("__init__.py"), FIXTURE_PLUGIN_CODE).unwrap();

    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap();
    let python = repo.join(".venv/bin/python");

    let bounds = AgentCacheBounds {
        max_size: Some(1),
        ..Default::default()
    };

    let agent = Arc::new(ConversationAgent::new(
        Arc::new(FallbackStub),
        move |home, msg, history, db| {
            // Real factory spawning python -m hermes_cli.rust_extension_host
            ...
        },
        bounds,
    ));

    // Turn 1 on session-one: spawns Python child 1
    let msg1 = make_test_message("session-one");
    agent.run_turn_with_context(TurnContext::from_home(&home.0), &msg1, &[], tx.clone()).await.unwrap();
    agent.finalize_turn_after_persist(TurnContext::from_home(&home.0), &msg1, "reply 1", true).await.unwrap();
    assert_eq!(agent.len(), 1);

    // Turn 2 on session-two: causes LRU cap overflow! session-one must be evicted
    let msg2 = make_test_message("session-two");
    agent.run_turn_with_context(TurnContext::from_home(&home.0), &msg2, &[], tx.clone()).await.unwrap();
    agent.finalize_turn_after_persist(TurnContext::from_home(&home.0), &msg2, "reply 2", true).await.unwrap();

    // Assert session-one was evicted from cache
    assert_eq!(agent.len(), 1);

    // Verify graceful child teardown: sentinel file written by FixtureProvider.shutdown()
    let shutdown_sentinel = home.0.join("extension-shutdown-session-one");
    let mut terminated = false;
    for _ in 0..100 {
        if shutdown_sentinel.is_file() {
            terminated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(terminated, "Python child did not execute graceful shutdown RPC");
    assert_eq!(std::fs::read_to_string(&shutdown_sentinel).unwrap(), "session-one");
}
```

---

## 8. Deferred Scope & Checkpoint Boundaries

To maintain velocity and isolate verification, the following items are explicitly categorized:

### Included in This Checkpoint
- Replacement of bare `tokio::sync::Mutex<HashMap>` with bounded `ConversationAgent` (`std::sync::Mutex<Inner>`).
- RAII `ActiveGuard` + `finalizer_pending_until` two-phase turn lifecycle.
- Capacity enforcement (`max_size`), idle TTL sweep (`idle_ttl_secs`), and memory pressure sweep (`memory_high_mb`).
- Default constants (`DEFAULT_MAX_SIZE = 128`, `DEFAULT_IDLE_TTL_SECS = 3600.0`) in `agent_cache_pressure.rs`.
- Public `flush_pending` and `session_end` methods on `extension_host::Client`.
- Wiring live reset hook in [`dispatch.rs:334`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L334).
- Wiring periodic sweep task and graceful shutdown drain in [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs).
- Comprehensive deterministic unit tests and real Python-child integration test.

### Deferred to Future Milestones (Phase 4 Agent Core Loop)
- **Rotation-Stable Session Key Unification**: Transitioning `ConversationAgent` key from `(home, session_id)` to `session_key` (depends on context compression rotation and `turn_lease::rebind`).
- **Config-Signature Invalidation**: Rebuilding agents mid-session on dynamic `config.yaml` edits (prompt bytes are currently frozen for the session).
- **Session Expiry Watcher Port**: Full background task scanning `SessionStore` for expired sessions (currently swept via idle TTL).
- **Unhealthy Worker Auto-Restart**: Dynamic reconnect/respawn of a failed Python worker mid-turn.
