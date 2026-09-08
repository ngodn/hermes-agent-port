# Native manual compression resolution

Date: 2026-09-08

## Implemented contract

Native HTTP and push sessions now own `/compress` and its `/compact` alias.
The parser supports preview and dry-run aliases, partial `here` and
`up-to-here` boundaries, `--keep`, and focus text. Preview is read-only and
never calls a provider. Aggressive mode and mandatory memory checkpoints fail
closed because those native consumers are not connected yet.

Live compression captures a durable message-id watermark, builds one bounded
and explicitly data-delimited summary request, and calls the current native
model without tools or streaming. The prompt includes structured content,
API-sidecar content, tool calls, tool names, and tool-call ids. A mandatory
programmatic redactor sanitizes prompt input, focus text, provider errors, and
the returned checkpoint before logging or persistence.

Publication is one `BEGIN IMMEDIATE` transaction. It verifies the stable route
and durable turn-lease owner, inserts the child with the frozen prompt and
tool/plugin state, transfers title provenance, writes the summary pair, clones
the protected and post-watermark tail, updates the route, and closes the parent
last. A failed check or injected database error rolls the whole transition
back. The in-memory route changes only after commit.

All native turns now share Python's `session_turn_leases` table, keyed by the
compression lineage root. A turn keeps that lease through history load, model
I/O, and assistant persistence. Waiters queue for the Python-compatible
30-minute budget. After acquiring, they reload the exact durable route from
SQLite so a rotation committed by another process cannot send them to the
ended parent. Lease refresh, owner-checked release, expired/dead owner reclaim,
and same-process orphan reclaim are covered by tests.

The next model turn uses the compression lineage root as its provider cache
scope. The physical parent client is released without firing session-end
memory hooks, preserving the logical conversation cache boundary while freeing
the obsolete client object.

## First review disposition

The initial AGY and Claude reviews produced ten distinct findings.

1. Tool calls were absent from summary input. Fixed by loading and serializing
   `tool_calls`, `tool_call_id`, and `tool_name` with the message content.
2. Strict redaction was absent. Fixed with deterministic recursive and textual
   redaction at every compression boundary, including provider error logs.
3. Rotation changed prompt-cache identity. Fixed by resolving the
   compression-lineage root for native provider cache scope.
4. Title and `title_source` were lost. Fixed by moving both from parent to
   child inside publication while respecting the unique-title index.
5. Cross-process exclusion was absent. Fixed with the Python-compatible
   durable turn lease, refresh loop, ownership fence, and lineage-root key.
6. Anti-growth compared wrapper overhead with raw history. Fixed by comparing
   the unwrapped body and then expanded to count structured/API/tool payloads.
7. Live failures said preview failed. Fixed with mode-neutral failure text.
8. Short partial histories diverged from Python. Fixed by retaining the
   earliest available user boundary when fewer than the requested exchanges
   exist.
9. Provider errors could disclose secrets in logs. Fixed by redacting before
   logging and returning a generic user-facing failure.
10. The durable lock needed publication-time ownership proof. Fixed by checking
    the non-expired holder inside the same immediate transaction.

## Fix re-review disposition

The second AGY review found five items, and Claude independently found three of
them.

- Retained tool groups: valid. The tail validator now accepts complete
  assistant/tool/final-assistant groups, checks every tool result against an
  advertised call id, and rejects missing, duplicate, unknown, or dangling
  calls. Dynamic cloning preserves every message column.
- Five-second ingress timeout: valid. Route, transcript, and durable turn
  admission now wait up to 1,800 seconds, matching Python, rather than dropping
  push input during a slow summary. The waits are asynchronous.
- Structured shrink accounting: valid. The comparison now includes encoded
  model content, `api_content`, tool calls, call ids, and tool names.
- Detached release race and same-process wedge: valid. Manual mutation allows
  a 250 ms teardown grace, active Rust holder identities are registered, and a
  stale same-process holder is safely reclaimable. Owner-checked delete cannot
  remove a replacement lease. A refresh finishing during shutdown no longer
  logs a false ownership-loss error.
- Cross-process route healing: valid. Post-wait admission now reloads the
  durable route even after normal one-shot recovery is complete, then retries
  if another process rotated it. The regression test leaves the in-memory route
  stale on purpose and proves the turn adopts the SQLite child.
- Lock-table mismatch: rejected. The review inspected only Python's separate
  `compression_locks` methods. Python also defines, acquires, refreshes, and
  releases `session_turn_leases` at `hermes_state.py:9306-9468` with the exact
  `conversation_id`, `holder`, `acquired_at`, and `expires_at` schema used by
  Rust. The two runtimes therefore coordinate through the same table.

## Explicitly deferred

- Automatic threshold compression and pruning/micro-compaction.
- Configurable in-place compression. Native manual compression currently uses
  safe child rotation even when Python's default is in-place.
- Auxiliary compression-model routing, fallback, cooldown, and token budgets.
- Pre-compression memory checkpoint and context-engine/plugin hooks.
- Aggressive history deletion.
- Full native tool execution and tool-message persistence for newly executed
  turns. The checkpoint faithfully handles compatible rows already in SQLite.

These are remaining port work, not behavior claimed by this checkpoint.

## Verification

- Real SQLite and local HTTP tests prove provider-before-publication ordering,
  strict secret removal, child rotation, tail reuse, and exclusion of old head
  turns from the next model request.
- SQLite tests cover title/frozen-state transfer, complete tool groups,
  post-watermark cloning, stale routes, incomplete tails, trigger rollback,
  and one winner across two database connections.
- Admission tests cover both same-process route replacement and a DB-only
  cross-process rotation during a durable wait.
- Lease tests cover exclusivity, refresh beyond TTL, release, and same-process
  orphan reclaim.
- Both requested helper scripts were used for mapping, implementation review,
  and fix re-review. Every reported issue above was checked against source
  before acceptance or rejection.

The transaction structure follows the repository ACID discipline: short
immediate transactions, no provider I/O while SQLite is locked, deterministic
lease ordering, durable publication before memory, and fail-closed ownership
checks.
