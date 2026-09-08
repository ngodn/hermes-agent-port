# Native in-place compression resolution

## Outcome

Manual `/compress` and `/compact` now follow Python's default
`compression.in_place: true` policy on both HTTP and push ingress. The gateway
keeps the same session ID, route, transcript lease identity, frozen prompt, and
conversation client. Explicit `compression.in_place: false` retains the
previous atomic rotation path.

## Durable publication contract

The in-place branch performs provider summarization before opening SQLite's
write transaction. Publication then runs in one `BEGIN IMMEDIATE` transaction:

1. Verify the live route, open session, and optional lineage-root turn-lease
   holder.
2. Resolve the protected snapshot tail and every append above the captured
   watermark. Reject incomplete user/assistant/tool sequences.
3. Soft-archive summarized originals as `active=0, compacted=1` so recall and
   FTS search still find them.
4. Mark verbatim tail originals as `active=0, compacted=0` so search does not
   return duplicate copies.
5. Insert the redacted summary/checkpoint pair and clone retained plus
   concurrent rows byte-for-byte, including tool metadata and wider Python
   schema columns.
6. Recompute live `message_count` and `tool_call_count`, then commit without
   closing the session or changing the route.

An injected insert failure proves every flag change and insert rolls back.
The counter migration also keeps fresh Rust databases and wider shared Python
databases consistent.

## Prompt-cache and lifecycle decision

In-place compression does not rebuild or replace the system prompt. It changes
only the active transcript rows under the current session's mutation leases.
The client is therefore retained, and no rotation/session-switch notification
is appropriate. A command-level test proves the release callback is not called.

Rotation remains available when configured. That path still publishes a child,
rebinds the transcript lease, closes the parent, and evicts the obsolete
physical client while retaining the compression-lineage cache scope.

## Helper split and disposition

The requested helpers were assigned different bounded jobs:

- AGY mapped only automatic thresholds, retry/rearm state, proactive tool
  pruning, and micro-compaction insertion points. Its report is
  [automatic-compression-map-agy.md](automatic-compression-map-agy.md).
- Claude mapped only auxiliary summarizer routing, failure cooldowns,
  checkpoint-required memory behavior, extension notifications, and in-place
  versus rotation policy. Its report is
  [compression-hooks-model-map-claude.md](compression-hooks-model-map-claude.md).

The reports were source maps, not duplicate implementation reviews. Their
pre-implementation statements that in-place store/command wiring was absent are
superseded by this checkpoint. Their remaining automatic, auxiliary, memory,
and hook findings stay open.

## Validation

- Full Rust workspace: 1,572 passed, two ignored.
- Focused native compression suite: 21 passed.
- Python in-place compaction, rollback, and persisted-marker oracle: 15 passed.
- `cargo fmt --all -- --check`: passed.
- Clippy for all workspace targets with warnings denied: passed.
- `git diff --check`: passed.

## Deferred work

Automatic request-pressure triggering, provider-usage capture, retry/rearm and
anti-thrash state, proactive tool-result pruning, micro-compaction, auxiliary
summary routing/fallback, durable cooldowns, pre-compression memory checkpoints,
context-engine/plugin notifications, and aggressive deletion remain separate
production checkpoints.
