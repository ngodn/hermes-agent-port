# Conversation cache lifecycle review resolution

Date: 2026-09-08

This records the source-verified disposition of the Gemini and Claude helper
reviews for the bounded native conversation cache checkpoint. The reports were
advisory. Every material claim below was checked against the current Rust tree
and the live Python reference before acceptance or rejection.

## Accepted and fixed

| Finding | Resolution |
| --- | --- |
| A failed model turn decremented `pending_turns` inside `run_turn_with_context`, then the gateway's mandatory post-persist finalizer decremented it again | Removed the inner decrement. Initialization failure still removes an empty cell immediately, while every admitted production turn is released once by the existing HTTP or push finalizer. A regression test covers the failed-turn path. |
| Parent-only anonymous RSS missed the Python extension hosts owned by cached conversations | Pressure accounting now reads the Linux process table, finds descendants rooted at the gateway PID, and sums their anonymous RSS with total RSS as a compatibility fallback. A pure process-tree test covers nested descendants and the fallback. |
| `Notify::notify_waiters` could lose the shutdown wakeup when no waiter was registered yet | Replaced it with `notify_one`, whose permit survives until the bounded shutdown waiter consumes it. |
| Bounded shutdown needed direct coverage of both ordinary retirement and forced retirement after the active grace | Added shutdown tests that prove new checkout rejection, hard close of all idle clients, and deliberate forced close of a still-pending turn when the supplied budget is exhausted. |

## Verified as already correct

| Finding | Verification |
| --- | --- |
| Teardown task registration could race shutdown | Cache removal and retirement task registration are serialized by `retirement_gate`. Shutdown takes the task list only after scheduling all forced entries. The gate is scoped and is not held across task awaits. |
| Reset might retire the newly minted session instead of its predecessor | HTTP and push routing consume `prev_session_id` from `take_auto_reset_predecessor` and retire that old ID. The routing marker is consumed and persisted once. A real HTTP reset test proves two native clients and one old-session hard retirement. |
| Retirement could race the assistant SQLite write | `pending_turns` spans model execution, assistant persistence, and `finalize_turn_after_persist`. Every steady-state eviction path skips pending entries. Reset attaches deferred retirement to the live entry; expiry returns for a later watcher retry. |
| Reset promotion might deactivate the transcript before retirement reloads it | `promote_to_session_reset` updates the session row and conversation generation only. It does not deactivate message rows. Lifecycle loading also preserves structured content, `api_content`, assistant tool calls, and tool-result identity. |
| Extension-host teardown was only drop driven | `Client::close` now drains queued memory writes, optionally sends the durable transcript to `session_end`, requests shutdown, and awaits the worker completion signal. Each stage is bounded and later cleanup still runs after an earlier best-effort error. The real Python-child test proves the transcript event and shutdown sentinel. |

## Rejected or qualified findings

### Cap and pressure extraction

The Claude review describes Python soft eviction as never firing
`on_session_end` and derives an exactly-once contract from that premise. The
live source contradicts it. `GatewayRunner._commit_memory_before_soft_evict`
calls `AIAgent.commit_memory_session`, and that method directly calls
`MemoryManager.on_session_end`. It deliberately does so before a finite session
leaves the cache, because an uncached expiry cannot reconstruct that provider
instance.

Rust applies the same eager extraction for finite cap and pressure evictions.
Its cached client also exclusively owns a Python child, so removal must finish
with `shutdown_all` or the child and provider handles leak. A resumed session
gets a new client. A later real boundary can therefore run extraction again,
just as Python can after an evicted session is rebuilt. Provider extraction is
best-effort and may be repeated; the current reference does not provide an
exactly-once marker for this hook. Mode `none` entries use release without a
session transcript, and finite unexpired entries remain exempt from idle TTL,
matching the reference trigger policy.

### Second flush after `session_end`

The suggested second queue drain is not present in the Python hard-teardown
ordering. The reference drains pending turn writes, invokes `on_session_end`,
then closes the agent. `MemoryManager.shutdown_all` drains its serialized
worker before provider shutdown. Rust follows that ordering through
`flush_pending`, `session_end`, and `shutdown`.

### PID reuse in the `Process` drop backstop

No extra `reaped` flag is needed. Tokio documents that `Child::id()` returns
`None` after the child has been polled to completion specifically to avoid PID
reuse confusion. Before reaping, the operating system cannot reuse that PID.
The existing optional ID check is therefore the correct guard. Reference:
<https://docs.rs/tokio/latest/src/tokio/process/mod.rs.html>.

### Process memory versus cgroup memory

Linux cgroup v2 `memory.current` includes a cgroup and its descendants, but a
gateway can run in a shared or manually arranged cgroup. Treating that number
as this process tree's consumption could evict conversations because of
unrelated processes. The implementation retains cgroup values for deriving the
budget and uses exact gateway-descendant RSS for the measured usage. Kernel
reference: <https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html>.

### Abandoned finalizers

There is no steady-state timeout that guesses a turn is abandoned. Both native
production ingress paths own the turn in a detached task, join model execution,
persist the assistant result, and always call the post-persist finalizer. A
request waiter disconnect does not cancel that owner. Reset and eviction wait
for this boundary, and process shutdown has the explicit bounded-force
backstop. A future caller must preserve the same pairing contract.

## Deliberately deferred

- Explicit `/new`, `/reset`, and related native command handlers are not yet
  implemented. Auto-reset is live and covered. Explicit command retirement
  belongs with the native command-dispatch milestone.
- Compression-driven prompt invalidation and session rotation remain unported.
- The Python process registry is not native yet, so policy expiry cannot yet
  defer a boundary because a conversation-owned background process is live.
- Plugin `on_session_finalize` hooks remain part of the later native plugin
  manager milestone. This checkpoint closes the compatibility host and its
  external-memory provider.
- Transparent extension-host respawn after a fatal mid-conversation transport
  failure remains unported.
- A SIGTERM grace before the final process-group SIGKILL is a later descendant
  cleanup refinement. The current path first offers a bounded protocol-level
  graceful shutdown.

## Result

The checkpoint now has one cache-owned lifecycle boundary for LRU capacity,
idle TTL, process-tree pressure, automatic reset, policy expiry, and gateway
shutdown. No cache mutex is held across factory work, SQLite I/O, provider I/O,
or child teardown. Pending turns remain non-evictable through durable assistant
persistence, and each tracked teardown is joined or explicitly aborted within
the gateway shutdown budget.
