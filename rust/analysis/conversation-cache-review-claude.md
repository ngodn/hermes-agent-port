# Conversation-cache lifecycle: independent audit of the uncommitted Rust implementation

**Deliverable**: independent correctness/concurrency review of the current working-tree
`ConversationAgent` cache and its retirement lifecycle, against the live Python reference.
**Author**: Claude (Opus 4.8), independent audit pass.
**Method**: read the working-tree source directly (no `git diff`, no test runs, no impl edits).
Rust internals read in full: `conversation_agent.rs`, `agent_cache_pressure.rs`, and the exact
call sites in `dispatch.rs`, `message.rs`, `main.rs`, `native_agent.rs`, `extension_host.rs`,
`session_store.rs`, `session_db.rs`. Python cap/idle/pressure/reset/expiry/shutdown semantics
traced against `gateway/run.py`, `run_agent.py`, `agent/memory_manager.py`,
`hermes_cli/rust_extension_host.py`, `gateway/slash_commands.py`, `tools/process_registry.py`
(a parallel source-grounded trace, cited inline).

This implementation is a real advance over both prior design docs
(`conversation-eviction-map-claude.md`, `conversation-cache-implementation-agy.md`). Most of the
flaws those docs argued about are closed here: the pending-turn window is one indivisible counter
that spans persist and the post-persist finalizer, retirement is decoupled from the leaf lock and
runs on tracked join handles that shutdown awaits, reset keys on `prev_session_id`, and the whole
thing is wired into `main.rs`. The bounds module is no longer inert. Credit is in section 4.

The audit still found one correctness-grade divergence and a set of medium/low issues, plus a test
suite that bakes the divergence in as the expected contract.

---

## 1. Verdict at a glance

| Trigger | Rust retirement kind | Python behavior | Match? |
| :-- | :-- | :-- | :-- |
| LRU cap (`enforce_cap_locked`) | `SessionEnd` if `finalizable` else `Release` (`conversation_agent.rs:172-176`) | SOFT release; at most extraction-only `commit_memory_session` for finite sessions, provider stays warm (`run.py:30578-30659`, `30351`, `30296-30349`) | **No (see H1)** |
| Idle TTL (`sweep_idle_at`) | `Release` only, skips `finalizable` (`:256-259`, `:268`) | SOFT release, skips finalizable-not-yet-expired (`run.py:30661-30748`, `30727-30733`) | Yes |
| Memory pressure (`sweep_pressure_at`) | `SessionEnd` if `finalizable` else `Release` (`:388-393`) | SOFT release + `malloc_trim`; post-persist guarded by `transcript_persistence_caught_up` (`run.py:30434-30547`, `30487`) | **No (see H1)** |
| Auto-reset (`retire_session`) | `SessionEnd`, defers past pending finalizer (`:222-243`) | HARD (`_cleanup_agent_resources` fires `on_session_end`), bounded 30s (`slash_commands.py:186-209`) | Yes |
| Session expiry (`expire_session`) | `SessionEnd`, teardown before durable marker (`:336-362`) | HARD, teardown then `set_expiry_finalized` (`run.py:15470-15589`) | Yes (see M1) |
| Gateway shutdown (`shutdown`) | force-close all as `SessionEnd`, budget 45s, active grace ≤10s (`:431-500`) | HARD, `_CLEANUP_TIMEOUT_S=30.0`, proceeds on timeout (`run.py:12235`, `16815-16837`) | Yes (see M2) |

Soft vs hard mapping in Rust: `Release` = `close_conversation(None)` = flush_pending + shutdown RPC,
no `session_end` (`conversation_agent.rs:544`, `extension_host.rs:307-345`). `SessionEnd` =
`close_conversation(Some(transcript))` = flush_pending + `session_end` (fires provider
`on_session_end`) + shutdown (`shutdown_all`). Both tear the Python child down; Rust has no way to
keep a provider warm-but-detached the way Python's `release_clients()` does
(`run_agent.py:5001-5061`), so every Rust eviction respawns on the next turn.

---

## 2. HIGH: cap and pressure fire a full end-of-session teardown on a live conversation

**Where**: `conversation_agent.rs:172-176` (cap) and `:388-393` (pressure) select
`RetirementKind::SessionEnd` whenever `entry.finalizable` is true. `finalizable` is
`policy.mode != "none"` (`session_store.rs:64-66`), i.e. any session with a real reset policy,
which is the common case, not an edge case.

**Python contract** (verified): the LRU cap (`run.py:30578-30659`), the idle sweep
(`:30661-30748`), and the memory-pressure sweep (`:30434-30547`) are all SOFT. They call
`release_clients()`, which frees the LLM socket pool and drops the transcript but leaves the memory
provider object alive and running and **never** calls `on_session_end` (`run_agent.py:5001-5061`,
docstring: "memory provider ... keeps running ... Distinct from close() ... the hard teardown for
actual session boundaries"). The only memory work a cap/pressure eviction may do is
`commit_memory_session` (`run.py:30296-30349`), which is extraction *without provider teardown* and
only for finalizable-not-yet-expired sessions. `on_session_end` + `shutdown_all` fire only at true
boundaries: `/new` (`slash_commands.py:189`), expiry (`run.py:15540`), process shutdown
(`run.py:16835`). The trace's own closing note: "A Rust port firing `on_session_end` on a bare
pressure/idle/cap shed would be a correctness divergence."

**Rust `SessionEnd` on cap/pressure does exactly that.** `close_retired` for `SessionEnd`
(`conversation_agent.rs:521-547`) loads the transcript and calls `close_conversation(Some(...))`,
which sends the `session_end` RPC → `on_session_end` and then `shutdown` → `shutdown_all`
(`native_agent.rs:720-725`, `extension_host.rs:330-345`). Full end-of-session teardown, triggered by
a memory-pressure valve or a 129th cached session.

**Concrete failure (double `on_session_end`)**: session S has a daily-reset policy (finalizable).
Memory climbs, the 300s maintenance pass runs `sweep_pressure` and evicts S as `SessionEnd`:
`on_session_end(transcript)` fires, long-term extraction #1, child torn down. The routing entry for
S is untouched: `expiry_finalized` is still false, `session_id` unchanged. The user sends another
message on S; `checkout` builds a fresh child, `initialize(S)` runs again, the conversation
resumes. Hours later S actually expires; `expired_sessions()` still lists it
(`session_store.rs:81` filters on `expiry_finalized`), `expire_session` evicts the now-cached entry
as `SessionEnd`, and `on_session_end(transcript)` fires a **second time**, extraction #2 over an
overlapping transcript. Python fires it exactly once because the provider stayed warm across the
soft eviction. Any provider whose `on_session_end` is not idempotent (summarizers, fact extractors
that append) double-writes long-term memory for one logical conversation.

**The trap on the "obvious" fix**: flipping cap/pressure to `Release` to match Python opens the
symmetric hole. `expire_session` only fires `on_session_end` when the entry is still cached; if the
lookup misses it takes the `None` branch (`conversation_agent.rs:344,350-351`) and writes the
durable expiry marker via `finalize_expired_session` **without** any `on_session_end`. So a
finalizable session that gets soft-evicted and then expires while uncached would get its
end-of-session extraction skipped entirely, i.e. at-most-once with a real miss. This is precisely
the data-loss case Python compensates for with `commit_memory_session` at soft-evict time
(`run.py:30297-30349`, comment "a finite session later expires with no cached agent and
on_session_end is silently skipped: memory [lost]").

**Assessment**: Rust cannot reproduce Python's exactly-once invariant with the current RPC surface,
because it cannot keep a provider warm after eviction and has no extraction-without-teardown RPC
(only `session_end`, which also runs `on_session_end` + `shutdown_all`). The current code chose
at-least-once (eager teardown, double on resume); the naive match to Python would give at-most-once
(miss on evict-then-expire). Both diverge. This needs a deliberate decision, not a silent default.
The most faithful fix is a commit-only path: a new host RPC that runs the provider's end-of-session
extraction without `shutdown_all`, invoked on the cap/pressure soft path for finalizable sessions,
mirroring `commit_memory_session`. Short of that, if `SessionEnd` on cap/pressure is kept as a
pragmatic "capture eagerly" choice, `on_session_end` must be made idempotent per session and the
double-extraction behavior documented, because it is not what the Python reference does.

---

## 3. Medium and low findings

### M1: `expire_session` skips `on_session_end` for an uncached expired session (silent)
`conversation_agent.rs:344,350-351`: when the entry is not in the cache the `None` branch falls
straight through to `finalize_expired_session`, writing `expiry_finalized`/`end_reason` with no
provider teardown. Today this is masked because cap/pressure evict finalizable sessions as
`SessionEnd` (H1), so a finalizable session rarely leaves the cache without already firing
`on_session_end`. It becomes a real end-of-session-memory miss the moment H1 is fixed toward soft
eviction, and it is already reachable for a session that is cap/pressure-evicted, never resumed, and
then expires. Python's expiry watcher fires cleanup on the cached agent
(`run.py:15539-15547`) and, for the uncached case, relies on the earlier commit compensation.
Recommend: decide expiry's contract jointly with H1; an uncached expiry that means to finalize
memory has to spin up a short-lived provider to run `session_end`, or the cap/pressure path has to
have committed already.

### M2: shutdown force-closes in-flight turns and drops their finalizer
`shutdown` sets `shutting_down`, waits `active_grace = budget.min(10s)` for `pending_turns` to
drain (`:442-473`), then `std::mem::take`s **all** remaining entries including `pending_turns > 0`
and closes them `SessionEnd` (`:475-481`). A model turn that runs longer than 10s at shutdown is
truncated: its child is hard-closed while the turn still holds its own client `Arc`, and its
post-persist `finalize_turn_after_persist` then finds the entry gone and no-ops
(`:634,640`), so that final turn's `turn_complete` external-memory write is lost. The 10s active
grace is a reasonable mirror of Python's `_FINALIZE_TIMEOUT_S = 10.0`, and Python's shutdown is
likewise bounded and proceeds on timeout, so this is a bounded shutdown-window edge rather than a
steady-state bug. Worth a comment noting the truncation is deliberate, and worth a test.

### M3: retirement transcript is reloaded from SQLite, not the live transcript Python uses
`close_retired` for `SessionEnd` loads the transcript via
`SessionDb::open_shared(path).load_lifecycle_messages(session_id)` filtered on `active = 1`
(`conversation_agent.rs:521-543`, `session_db.rs:1635-1677`). Python passes the agent's in-memory
`_session_messages`, explicitly **not** a SQLite reload (`run.py:12398-12408`,
`session_messages = getattr(agent, "_session_messages", None)`). Two consequences: (a) any message
not yet persisted at retirement time is absent from the `session_end` payload (mitigated for the
last turn because `end_turn` persists synchronously before `finalize`, but not guaranteed for a
retirement racing an unpersisted turn); (b) the payload depends on messages staying `active = 1`
under the retired `session_id`. For reset specifically, the auto-reset path writes the reset marker
on the old session during resolution (`dispatch.rs:320-333`, `SessionStore::transition` →
`promote_to_session_reset`) **before** `retire_conversation` fires (`dispatch.rs:344-349`), which is
the opposite ordering from expiry (teardown before marker). If reset or any compaction ever flips
the old session's rows to `active = 0`, `load_lifecycle_messages` returns empty and `session_end`
fires with a blank transcript. I did not confirm that `promote_to_session_reset` leaves rows active;
flagging it as a dependency to verify, lower confidence.

### L1: `pending_turns` has no timeout; an abandoned finalizer pins an entry forever
The pending window is a plain counter that `run_turn_with_context` arms on success and never
decrements (`:609-611`); only `finalize_turn_after_persist` (or a `run_turn` error) releases it via
`finish_turn`. There is no time-based fallback (the agy design used a 30s deadline for exactly this
reason). An entry stuck at `pending_turns > 0` is immune to cap, idle, pressure, reset, and expiry
until process shutdown force-takes it. In practice both production callers pair the two calls inside
one detached task that reaches `finalize` even on turn panic (`dispatch.rs:461-487`,
`message.rs:248-262`), and the detach-via-spawn survives ingress-waiter cancellation, so this does
not leak today. It is a fragile invariant with no self-healing: any future caller that runs a turn
without pairing the finalizer, or a panic in the reply-accumulation loop between the two, pins the
entry and its child until shutdown. Consider a defensive deadline or a debug assertion that every
armed turn is finalized.

### L2: child termination is graceful-RPC-then-SIGKILL, no SIGTERM grace step
`run_worker` sends the shutdown RPC, closes stdin, waits `SHUTDOWN_TIMEOUT` (7s), then
`kill_process_tree` SIGKILLs the whole process group (`extension_host.rs:436-478`, `576-596`; drop
backstop `:137-144`). Python SIGTERMs the tree with a `terminal.daemon_term_grace_seconds` grace
(default 2.0s) before escalating to SIGKILL (`process_registry.py:912-947`). The shutdown RPC is a
clean-exit path, so the missing SIGTERM only matters for grandchild tool subprocesses that outlive
the parent and would have wanted their own SIGTERM handler; a process-group SIGKILL is thorough but
gives them no clean-exit window. Minor divergence.

### L3: `open_shared` on the retirement path
Each `SessionEnd` retirement opens a fresh DB connection through `open_shared`
(`conversation_agent.rs:527`, `session_db.rs:781`). A prior review noted `open_shared` takes an
IMMEDIATE write lock; taking a write lock to run a read-only `SELECT` at retirement, potentially for
many entries at once during a shutdown fan-out, is wasteful and can contend. Low priority.

---

## 4. What is correct and worth keeping

- **One indivisible pending window across persist + finalizer.** `run_turn_with_context` leaves
  `pending_turns` armed on success (`:609-611`) and only `finalize_turn_after_persist` releases it
  (`:642-643`). Every eviction path skips `pending_turns > 0` (cap `:168`, idle `:257`, pressure via
  the evictable flag `:376`, reset `:230`, expiry `:345`). This is strictly stronger than Python,
  which releases `running_ids` before the disk flush and needs a separate
  `transcript_persistence_caught_up` guard on the pressure path (`run.py:30487`,
  `agent_cache_pressure.py:257-275`). The Rust window subsumes that guard, so its absence is fine.
- **`finish_turn` identity guard.** The `Arc::ptr_eq(&entry.cell, cell)` check (`:198`) makes a
  late finalizer for an already-evicted-and-rebuilt key a no-op instead of corrupting the new
  entry's count. Double `finish_turn` (error path then finalizer) is safe via `saturating_sub`.
- **Reset waits for the finalizer instead of racing it.** `retire_session` marks
  `retirement = SessionEnd` and defers removal while `pending_turns > 0` (`:229-237`); `finish_turn`
  completes the hard close once the finalizer lands (`:204-208`). Tested at
  `conversation_agent.rs:846-915`.
- **Reset keys on `prev_session_id`.** `dispatch.rs:336-349` retires the superseded id, filtered
  against the current id, closing the agy-flagged "evict the wrong key" gap.
- **Decoupled, awaited retirement.** Eviction removes under the leaf lock and hands `RetiredEntry`
  values to `schedule_retirements`, which spawns off-lock and retains the `JoinHandle`
  (`:411-427`); `shutdown` awaits those handles within budget and aborts the stragglers
  (`:485-498`). This closes the agy "detached worker abort" concern: the leaf lock is never held
  across a factory, model call, DB read, or lifecycle RPC.
- **Bounds module is live.** `agent_cache_pressure.rs` no longer carries `#![allow(dead_code)]`;
  `effective_max_size`/`effective_idle_ttl`/`plan_pressure_evictions`/`read_anon_rss_mb` are all
  wired, and `memory_high_mb` defaults to `"auto"` (cgroup-derived), so pressure eviction is on by
  default rather than dormant.
- **Idle correctly defers finalizable sessions** (`:256-259`) and uses the soft `Release` kind,
  matching Python's finite-session deferral.

---

## 5. Production wiring status

- **Cache construction**: native path only, `main.rs:895-918`, bounds from
  `resolve_agent_cache_bounds(&user_config)` (`:915`). Python-bridge path keeps no cache. Good.
- **Maintenance loop**: `cache.start_maintenance(shutdown, session_store)` at `main.rs:994-999`,
  60s initial delay then 300s cadence, driving expiry scan + idle + pressure in one loop
  (`conversation_agent.rs:283-331`). Note Rust folds expiry into the 300s maintenance loop rather
  than Python's dedicated expiry watcher, so expiry granularity is ~300s. Acceptable.
- **Graceful shutdown**: `cache.shutdown(45s)` at `main.rs:1093-1095`, after `axum::serve` returns
  and `shutdown.cancel()`, before the runtime drops. Good; this is the barrier the agy doc argued
  was missing.
- **Auto-reset**: wired in `dispatch.rs:344-349`.
- **Explicit `/new` slash reset**: no Rust call site (consistent with the prior lifecycle review).
  Auto-reset via `prev_session_id` covers the policy-driven case; the interactive `/new` teardown is
  still unported. Call out, not block.

---

## 6. Test assessment: the suite encodes the H1 divergence as the contract

The unit tests (`conversation_agent.rs:659-1103`) drive stub `RecordedAgent`s whose
`close_conversation` records only `session_messages.is_some()` (`:682-688`), so they verify the
soft/hard *selection* but never a real child teardown, a real `load_lifecycle_messages` transcript,
or a real `session_end` payload shape.

- **`idle_and_pressure_use_distinct_modes_and_skip_pending_turns` asserts the divergent behavior as
  correct.** At `:948-964` it runs a `finalizable_turn`, sweeps pressure, and asserts
  `closes == [false, true]`, i.e. it *requires* that pressure-evicting a finalizable entry fires
  the `SessionEnd` (hard) close. This bakes the H1 divergence into the test contract. A test written
  to Python's contract (pressure of a live finalizable session is soft, no `on_session_end`) would
  fail against this code. This test should be the trigger for the H1 decision, not evidence of
  correctness.
- **No end-to-end retirement test with a real Python child.** The strong lifecycle test that spawns
  the real host and checks the `session_end` + shutdown sentinel exists only at the
  `extension_host::Client::close` level (`extension_host.rs:1002-1044`), not wired through
  `ConversationAgent` retirement. The agy design's `test_real_python_child_eviction_and_graceful_
  teardown` is absent, so nothing proves cap/pressure/reset/expiry actually spawn → tear down a real
  child with the correct transcript.
- **Untested paths**: double `on_session_end` on evict-then-resume (H1); `expire_session`'s uncached
  `on_session_end` skip (M1); `shutdown` budget, force-close of pending entries, and lost finalizer
  (M2); `shutdown` has no direct unit test at all; the `pending_turns` no-timeout leak (L1);
  reset's marker-before-teardown transcript dependency (M3).
- **Well tested**: reset-waits-for-finalizer (`:846-915`), expiry-waits-for-finalizer + durable
  marker (`:1011-1102`, though against a stub whose transcript is never asserted to reach a
  provider), LRU cap + finalizer protection (`:793-844`), retryable failed init (`:971-1009`), and
  the pure bounds/pressure-planning logic (`agent_cache_pressure.rs:334-452`).

---

## 7. Recommended priority

1. Decide H1 explicitly. Cap and pressure of a finalizable session must not silently run a full
   end-of-session teardown that Python never runs. Either add a commit-only (extraction without
   `shutdown_all`) retirement path to match `commit_memory_session`, or keep eager `SessionEnd` as a
   documented, idempotent choice. Fix M1 in the same change so the two paths agree on who owns the
   single `on_session_end`.
2. Replace the `idle_and_pressure` assertion (`:964`) with the chosen contract, and add the
   end-to-end real-child retirement test.
3. Add a `shutdown` test (M2) and a defensive `pending_turns` deadline or debug assertion (L1).
4. Verify M3's `active = 1` transcript dependency across the reset path; then L2/L3 as cleanups.
</content>
</invoke>
