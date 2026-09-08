# Audit Review: Rust Conversation-Cache Lifecycle Implementation

**Target**: [`rust/crates/hermes-gateway/src/conversation_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs) and related lifecycle integration in [`dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs), [`message.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs), [`extension_host.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs), [`session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs), [`session_store.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs), and [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs).  
**Reference**: Python gateway implementation in [`gateway/run.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py), [`gateway/agent_cache_pressure.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/agent_cache_pressure.py), [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py), [`agent/memory_manager.py`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py), and [`hermes_cli/rust_extension_host.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py).

---

## 1. Executive Verdict & Verification Matrix

The uncommitted diff attempts to address the bounded lifecycle gaps identified in [`rust/analysis/conversation-cache-implementation-agy.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-cache-implementation-agy.md) and [`rust/analysis/conversation-lifecycle-wiring-claude.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/conversation-lifecycle-wiring-claude.md). However, the actual code deviates fundamentally from safe concurrency patterns and introduces severe correctness flaws:

1. **Active turn tracking is corrupted on turn failures** via double-decrement (`finish_turn` called in both [`run_turn_with_context`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L671-L674) and [`finalize_turn_after_persist`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L715-L717)).
2. **Cancelled turns permanently pin cache entries** because `pending_turns` lacks an RAII drop guard or timeout window.
3. **Idle TTL is completely inoperative for standard sessions** due to an unconditional `&& !entry.finalizable` guard in [`sweep_idle_at`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L268-L272).
4. **Memory pressure sensing is broken** because [`read_anon_rss_mb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L267-L282) reads the parent Rust binary's `/proc/self/status`, ignoring child Python extension host memory.
5. **Shutdown deadlocks Tokio worker threads** by holding a synchronous [`std::sync::MutexGuard`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L521) (`retirement_gate`) across an up-to-45-second `.await` timeout (`clippy::await_holding_lock`).
6. **Destructor SIGKILL PID reuse hazard** in [`Process::drop`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L136-L147) unconditionally executes `libc::kill(-(pid as i32), SIGKILL)` even after the child process cleanly exited and was reaped.
7. **Explicit reset (`/reset`, `/new`, `/clear`) has no production trigger** to retire cached clients in [`dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L259-L272).

| Area | Current Implementation Status | Correctness Verdict | Primary Evidence |
| :--- | :--- | :--- | :--- |
| **1. Cache Cap** | Evaluated on `checkout`; active excess skipped; cap eviction treated as `SessionEnd` if finalizable. | **FLAWED** | [`conversation_agent.rs:157-199`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L157-L199): Skipped active entries never re-checked on finish; premature child destruction on cap. |
| **2. Idle TTL** | Swept every 300s in maintenance loop; skips `entry.finalizable`. | **BROKEN** | [`conversation_agent.rs:268-272`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L268-L272): `!entry.finalizable` pins all sessions with idle/daily policies for their entire duration. |
| **3. Memory Pressure** | Swept every 300s; reads `read_anon_rss_mb()`. | **DEFECTIVE** | [`agent_cache_pressure.rs:267-282`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L267-L282): Measures parent Rust RSS (~30MB), not child subprocess memory (~1-4GB); never triggers. |
| **4. Automatic Reset** | Wire in `dispatch.rs` / `message.rs` reading `prev_session_id`. | **FLAWED** | [`dispatch.rs:336-347`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L336-L347): `prev_session_id` never consumed; retried on every subsequent turn. |
| **5. Expiry Watcher** | Sweeps `store.expired_sessions()`; closes cache before persisting SQLite marker. | **RISKY** | [`conversation_agent.rs:348-390`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L348-L390): Early `?` bail desynchronizes cache and store; 300s polling leaves expired child processes active. |
| **6. Process Shutdown** | Two-phase active grace + forced eviction + wait for tasks. | **HIGH SEVERITY** | [`conversation_agent.rs:521-536`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L521-L536): Holds `MutexGuard` across 45s `.await`; lost wakeup on `changed.notified()`. |
| **7. Pending Turn & Finalization** | `pending_turns: u32` incremented in `checkout`, decremented in `finish_turn`. | **CRITICAL** | [`conversation_agent.rs:671-674, 715-717`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L671-L674): Double-decrement on error; cancellation permanently pins counter `>= 1`. |
| **8. Transcript Reconstruction** | `load_lifecycle_messages` reads active messages and decodes content. | **FLAWED** | [`conversation_agent.rs:613-640`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L613-L640): Falls back to `Some(Vec::new())` on DB failure, wiping memory provider facts. |
| **9. Extension Host RPC Ordering** | `flush_pending` -> `session_end` -> `shutdown` -> child exit. | **FLAWED** | [`extension_host.rs:305-335`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L305-L335): Flushes *before* `session_end`, leaving post-session provider writes undrained. |
| **10. Process Exit Waiting** | `run_worker` waits for child exit; `Process::drop` kills `-pid`. | **HIGH SEVERITY** | [`extension_host.rs:136-147`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L136-L147): `libc::kill(-pid, SIGKILL)` fires on reaped PID, risking PID recycling kills. |
| **11. Task Tracking Races** | `retirement_tasks` holds `JoinHandle`s; drained during shutdown. | **FLAWED** | [`conversation_agent.rs:522`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L522): `std::mem::take` leaves tasks spawned during/after draining un-awaited. |
| **12. Profile / Session Identity** | Keyed by `(PathBuf, String)` where PathBuf is `database.profile_home()`. | **DEFECTIVE** | [`conversation_agent.rs:108-113`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L108-L113): Misses cache on `/resume` id rotation; bypasses cache entirely when database is `None`. |

---

## 2. Evidence-Backed Correctness Flaws by Topic

### 2.1 Cache Cap Enforcement & Eviction Semantics
* **Flaw 2.1.1: Unbounded Growth When Active Entries Occupy Excess Slots.**  
  In [`conversation_agent.rs:157-199`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L157-L199) (`enforce_cap_locked`), the cache sorts entries by `recency` and takes only `excess` items. If an entry in those slots has `pending_turns > 0`, it is skipped. When that in-flight turn finishes in [`finish_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L201-L227), `finish_turn` **does not re-evaluate the cap**. The cache remains silently over capacity indefinitely until a subsequent `checkout` occurs.
* **Flaw 2.1.2: Premature Subprocess Destruction on Transient LRU Eviction.**  
  When an entry is evicted under cap pressure in [`enforce_cap_locked`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L187-L193), it sets `kind = if entry.finalizable { RetirementKind::SessionEnd } else { RetirementKind::Release }`.  
  If `finalizable == true`, this schedules [`close_retired`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L613-L646), which executes `session_end` and `shutdown` RPCs, killing the Python extension host.  
  **Python Discrepancy**: In [`gateway/run.py:30648-30659`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L30648-L30659), Python's `_enforce_agent_cache_cap` invokes `_commit_then_release_soft` -> `_release_evicted_agent_soft`, which **keeps the session resumable**, preserves background processes, and runs `commit_memory_session` *without* provider teardown (`run.py:30310-30315`). Tearing down the Python child on LRU eviction permanently resets plugin state for an active session.

### 2.2 Idle TTL Exemption Flaw
* **Flaw 2.2.1: Total Exemption of Finalizable Sessions From Idle TTL.**  
  In [`conversation_agent.rs:268-272`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L268-L272) (`sweep_idle_at`):
  ```rust
  entry.pending_turns == 0
      && !entry.finalizable
      && now.saturating_duration_since(entry.last_used).as_secs_f64() > ttl
  ```
  In [`session_store.rs:64-66`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L64-L66), `is_session_finalizable` returns `true` for any session whose policy is not `"none"` (i.e. all `"idle"` and `"daily"` reset modes).  
  Because of `!entry.finalizable`, **any conversation with an idle or daily reset policy is unconditionally excluded from idle sweeping**. If a user configures a 7-day idle reset policy, its Python child process and SQLite handles are pinned in memory for 7 days, rendering the default `idle_ttl_secs = 3600.0` completely useless.
* **Flaw 2.2.2: Memory Leak When `session_store` Is Absent.**  
  In [`main.rs:1004-1008`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1004-L1008), if `session_store` is `None`, `cache.start_maintenance` never calls `expired_sessions()`. Coupled with `!entry.finalizable`, cached agents are permanently leaked.

### 2.3 Memory Pressure Sensing & Teardown Flaws
* **Flaw 2.3.1: Cross-Process Blindness in Anon RSS Measurement.**  
  In [`agent_cache_pressure.rs:267-282`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent_cache_pressure.rs#L267-L282), `read_anon_rss_mb()` reads `/proc/self/status` of the current process.  
  In Python, the gateway and agents share one process (`gateway/agent_cache_pressure.py:227`). In Rust, the memory-intensive workloads (Python runtime, PyTorch/ONNX, SQLite caches, plugin heaps) live in spawned **child subprocesses** (`python -m hermes_cli.rust_extension_host`). The parent Rust gateway typically consumes <40MB RSS.  
  Comparing parent `/proc/self/status` against `memory_high_mb` (e.g. 50% of container limit = 4096MB) will **never trigger**, allowing child processes to exhaust container RAM until killed by the Linux cgroup OOM killer.
* **Flaw 2.3.2: Pressure Eviction Treats Evictions as SessionEnd.**  
  Like Flaw 2.1.2, [`conversation_agent.rs:418-424`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L418-L424) marks memory-pressure-evicted entries as `SessionEnd`, breaking session continuity instead of performing soft client release.

### 2.4 Automatic Reset Propagation & Stale Parent Retires
* **Flaw 2.4.1: `prev_session_id` Is Never Cleared After Retirement.**  
  In [`dispatch.rs:336-347`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L336-L347) and [`message.rs:162-171`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L162-L171):
  ```rust
  let previous_session_id = entry.fields["prev_session_id"]
      .as_str()
      .filter(|previous| *previous != entry.session_id)
      .map(str::to_owned);
  if let Some(previous_session_id) = previous_session_id {
      self.agent.retire_conversation(..., &previous_session_id);
  }
  ```
  `prev_session_id` remains stored in `entry.fields` for the entire lifetime of the new session. On **every turn** (turn 2, 3, 4...), the gateway executes `retire_conversation` for the parent session.  
  **Python Discrepancy**: In [`gateway/run.py:21392`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L21392), Python consumes the flag immediately: `session_entry.was_auto_reset = False`.

### 2.5 Expiry Watcher Disconnects & Error Handling
* **Flaw 2.5.1: Non-Atomic Error Return Leaves Store/Cache Desynchronized.**  
  In [`conversation_agent.rs:370-389`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L370-L389) (`expire_session`):
  ```rust
  if let Some(retired) = retired {
      close_retired(retired).await?;
  }
  tokio::task::spawn_blocking(move || store.finalize_expired_session(&expired)).await...??;
  ```
  If `close_retired` fails (e.g. extension host communication timeout), the function bails via `?`. The entry has already been removed from `state.entries`, but `finalize_expired_session` was never run. The session remains unfinalized in SQLite while being evicted from cache.
* **Flaw 2.5.2: Coarse 300s Polling Loop Delays Teardown.**  
  [`conversation_agent.rs:82`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L82) hardcodes `MAINTENANCE_INTERVAL = Duration::from_secs(300)` and `INITIAL_MAINTENANCE_DELAY = 60s`. Expired sessions and idle children remain uncollected for up to 5 minutes after expiration.

### 2.6 Shutdown Synchronization, Lock Inversion & Lost Wakeups
* **Flaw 2.6.1: Tokio Worker Deadlock via `clippy::await_holding_lock`.**  
  In [`conversation_agent.rs:521-536`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L521-L536):
  ```rust
  let _gate = self.retirement_gate.lock().unwrap();
  let mut tasks = std::mem::take(&mut *self.retirement_tasks.lock().unwrap());
  let wait_budget = budget.saturating_sub(started.elapsed());
  if tokio::time::timeout(wait_budget, async {
      for task in &mut tasks { let _ = task.await; }
  }).await.is_err() { ... }
  ```
  The synchronous `MutexGuard` `_gate` is held across `.await` for up to **45 seconds**.  
  Any concurrent turn calling [`finish_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L202) (`let _gate = self.retirement_gate.lock().unwrap()`) will block an OS worker thread. If worker threads are saturated, `tasks` cannot make progress, resulting in a **deadlock**.
* **Flaw 2.6.2: Lost Wakeup on `self.changed.notify_waiters()`.**  
  In [`conversation_agent.rs:223`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L223), `finish_turn` calls `self.changed.notify_waiters()`.  
  In [`conversation_agent.rs:501`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L501), shutdown waits with `self.changed.notified()`.  
  Tokio's `notify_waiters()` **does not buffer a permit**. If an active turn finishes between line 497 (`entries.is_empty()` check) and line 501 (`notified().await`), the notification is permanently lost, causing shutdown to stall for the full 10-second `active_grace` period.
* **Flaw 2.6.3: Forced Eviction Races Active In-Flight Turns.**  
  At [`conversation_agent.rs:510-518`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L510-L518), `forced` eviction takes all remaining entries (regardless of `pending_turns > 0`) and sends `shutdown` RPCs while active turns are in flight, killing Python processes mid-query.

### 2.7 Pending Turn Double-Finish & Permanent Leak Hazards
* **Flaw 2.7.1: Double-Decrement Underflow on Turn Errors.**  
  In [`conversation_agent.rs:671-674`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L671-L674):
  ```rust
  if let Err(error) = client.run_turn_with_context(...).await {
      self.finish_turn(&key, &cell, false); // <--- DECREMENT 1
      return Err(error);
  }
  ```
  In [`dispatch.rs:434-464`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L434-L464) and [`message.rs:248-261`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L248-L261), the gateway **always** invokes `finalize_turn_after_persist` regardless of whether the turn succeeded or failed (`succeeded = false`).  
  In [`conversation_agent.rs:715-717`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L715-L717):
  ```rust
  if let Some(cell) = &cell {
      self.finish_turn(&key, cell, false); // <--- DECREMENT 2 (DOUBLE FINISH!)
  }
  ```
  When a turn fails (e.g. LLM rate limit, invalid request, network refusal), `finish_turn` is invoked **twice**. If a second turn checked out concurrently, its `pending_turns` counter is decremented to 0 prematurely while running, exposing it to mid-flight eviction.
* **Flaw 2.7.2: Permanent Active Turn Leak on Request Abort / Connection Drop.**  
  There is no RAII guard around `pending_turns`. If an HTTP client disconnects or Tokio task aborts while `run_turn_with_context` is awaiting model output, the future is cancelled. Neither `finish_turn` nor `finalize_turn_after_persist` is called. `pending_turns` remains permanently `>= 1`, completely disabling eviction and expiry for that session.

### 2.8 Transcript Reconstruction Fallback Defects
* **Flaw 2.8.1: Silent Fact Erasure on Missing Database.**  
  In [`conversation_agent.rs:633-637`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L633-L637) (`close_retired`):
  ```rust
  None => Some(Vec::new()),
  ```
  If `database_path` is `None` or opening SQLite fails, `load_lifecycle_messages` passes `Some(Vec::new())` (empty array `[]`) to `close_conversation(messages)`.  
  In [`agent/memory_provider.py:263-271`](file:///home/eins0fx/development/hermes-agent-port/agent/memory_provider.py#L263-L271), providers use `messages` for end-of-session fact extraction. Passing `[]` causes providers to treat the session as having zero history, bypassing memory persistence.

### 2.9 Extension-Host RPC Sequencing Hazards
* **Flaw 2.9.1: Inverted Flush / Session-End Ordering.**  
  In [`extension_host.rs:305-330`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L305-L330) (`Client::close`):
  1. `flush_pending`
  2. `session_end`
  3. `shutdown`  
  In [`hermes_cli/rust_extension_host.py:395-404`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_extension_host.py#L395-L404), `session_end` invokes `self._memory_manager.on_session_end(messages)`. Providers may enqueue asynchronous tasks to the background sync worker. Because `flush_pending` was called *before* `session_end`, those writes are never drained before `shutdown` runs, dropping memory updates.

### 2.10 Process Exit Waiting & Drop-Driven PID Reuse
* **Flaw 2.10.1: Destructor Blindly SIGKILLs Reaped Process Group.**  
  In [`extension_host.rs:136-147`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L136-L147):
  ```rust
  impl Drop for Process {
      fn drop(&mut self) {
          #[cfg(unix)]
          if let Some(pid) = self.child.id() {
              unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
          }
      }
  }
  ```
  When `run_worker` executes clean shutdown, it calls `process.child.wait().await` ([`line 473`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/extension_host.rs#L473)) and reaps the child process.  
  When `process` is later dropped, `Process::drop` fires `libc::kill(-(pid as i32), SIGKILL)` on the reaped PID. If the operating system has recycled that PID to an unrelated daemon or user process group, **SIGKILL is sent to an innocent process**.

### 2.11 Task Tracking Races
* **Flaw 2.11.1: Detached Retirement Leak at Shutdown.**  
  In [`conversation_agent.rs:522`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L522), `tasks` is drained with `std::mem::take`. Any retirement tasks spawned after line 522 (e.g. from forced eviction failures or late turn cancellations) are added to `retirement_tasks` but never awaited, leaking child processes on exit.

### 2.12 Profile and Session Identity
* **Flaw 2.12.1: Cache Miss on Session ID Rotation.**  
  The cache key is `(home: PathBuf, session_id: String)` ([`conversation_agent.rs:91`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L91)).  
  **Python Discrepancy**: In [`gateway/run.py:7792-7793`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7792-L7793), Python keys on `session_key` (the routing key, e.g. `telegram:12345`) and stores `session_id` in the tuple. When an ID rotates via `/resume`, Python preserves the agent instance to keep the upstream prompt-cache prefix warm (#54947). In Rust, any ID rotation causes a cache miss and rebuilds a brand new client from scratch.
* **Flaw 2.12.2: Silent Cache Bypass When Context Database Is None.**  
  [`conversation_agent.rs:108-113`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L108-L113) computes `context.home?.to_owned()`. If `database` is not supplied, `context.home` is `None`, and `ConversationAgent` silently bypasses caching and falls back to `self.fallback`.

---

## 3. Missing Production Triggers

The implementation lacks production wiring for three critical conversation lifecycle events:

1. **Explicit `/reset`, `/new`, `/clear` Slash Commands**:  
   In [`dispatch.rs:259-272`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L259-L272) and [`slash.rs:84-90`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slash.rs#L84-L90), slash command handling only handles `help`, `whoami`, and `status`. Destructive reset commands (`/reset`, `/new`, `/clear`) are not recognized as builtins and do not trigger `retire_conversation`.
2. **Post-Turn Cache Cap Re-Evaluation**:  
   When `enforce_cap_locked` skips an LRU candidate because it is mid-turn, there is no trigger in `finish_turn` to re-check the cap once that turn concludes.
3. **Cross-Process Memory Pressure Trigger**:  
   There is no background monitor aggregating child extension host RSS or reading `/sys/fs/cgroup/memory.current`.

---

## 4. Missing Tests & Checkpoint Claim Completeness

The checkpoint claim is incomplete because existing tests do not exercise real-world concurrency or real subprocess lifecycles:

1. **No Real Python-Child Integration Test for Conversation Cache**:  
   All tests in [`conversation_agent.rs:732-1175`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L732-L1175) use dummy `RecordedAgent` stubs with atomic counters. Not a single test validates `ConversationAgent` evicting a real `NativeAgentClient` or terminating `python -m hermes_cli.rust_extension_host`.
2. **Missing Test for Turn Cancellation / HTTP Disconnect**:  
   No test verifies that dropping a turn future releases `pending_turns`.
3. **Missing Test for Turn Failure Double-Finish**:  
   No test exercises `run_turn_with_context` returning `Err` followed by `finalize_turn_after_persist(succeeded=false)`.
4. **Missing Test for PID Reuse Safety in `Process::drop`**:  
   No test verifies that clean process exit does not issue a redundant SIGKILL.

---

## 5. Prioritized Remediation Checklist

1. **Fix Double-Finish (High)**: Remove `self.finish_turn(&key, &cell, false)` from `run_turn_with_context` error exit ([`conversation_agent.rs:672`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L672)) or bind it to an RAII guard so that each turn can only decrement `pending_turns` exactly once.
2. **Eliminate Tokio Mutex Deadlock (High)**: Remove `let _gate = self.retirement_gate.lock().unwrap()` across `await` in `shutdown` ([`conversation_agent.rs:521`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L521)). Use a Tokio channel or async lock.
3. **Fix Idle TTL Finalizable Exemption (High)**: Remove `&& !entry.finalizable` from [`sweep_idle_at`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L269). Mirror Python: if the session has expired or has exceeded idle TTL, soft-release the agent.
4. **Prevent PID Reuse SIGKILL (Medium)**: In `Process`, track `reaped: bool` and only execute `libc::kill` in `Drop` if `!self.reaped`.
5. **Correct RPC Ordering (Medium)**: In `Client::close()`, re-order RPCs: `flush_pending` -> `session_end` -> second `flush_pending` (or wait behind worker queue) -> `shutdown`.
6. **Wire Explicit Slash Reset (Medium)**: Add `/reset`, `/new`, `/clear` handling in `dispatch.rs` that explicitly calls `ca.retire_session(...)`.
