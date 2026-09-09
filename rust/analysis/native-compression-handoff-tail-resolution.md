# Native compression handoff assembly and tail anchors

## Outcome

Native full compression now assembles the same durable handoff shapes as the
Python compressor. The planner retains the protected head and tail, removes or
unwraps older standalone and merged handoffs, selects the summary role against
template-visible neighbors, merges unavoidable collisions into the correct
tail carrier, and restores a real user anchor or the exact continuation
placeholder when no human turn survives.

The summarizer receives previous summary bodies separately from new turns.
Summary carriers outside the selected middle contribute their prior summary
without re-summarizing retained live content. Carriers inside the middle
contribute both their prior summary and their unwrapped live content. String and
structured list content follow the same rules.

All three production callers use the planner: manual compression, automatic
pre-turn compression, and same-turn compression after a durable tool batch.
The old fixed user-summary plus assistant-acknowledgment pair is gone.

SQLite publication now consumes the exact active snapshot and a complete
replacement plan. Retained rows are cloned in planned order with every durable
column preserved, then only content, API content, and the summary marker may be
rewritten. In-place publication archives omitted originals while hiding
superseded retained copies from search. Rotation leaves the parent transcript
immutable. Route, lineage lease, live-session, title-transfer, counter, and
rollback guarantees remain inside the immediate transaction.

The snapshot CAS covers all message replay fields, display metadata, and the
timestamp, not just the earlier compression projection. A concurrent
metadata-only change therefore rejects publication just like a concurrent
append. Stored adjacent user rows remain legal for the summary-plus-anchor
case because the provider-bound repair merges them before strict template
validation. Orphaned or incomplete tool-call groups are still rejected.

The active tool loop now stops before a provider call that would be driven only
by a reference handoff. A later real user turn, tool result, pending assistant
tool call, or merged carrier with a live ask keeps the call active.

## Team split and review

The work was split by independent output:

- AGY ran once behind the exclusive auth lock and implemented only the pure
  transcript replacement planner.
- Claude worked only on the SQLite replacement publisher and store interface.
  Its wrapper timed out after producing a substantial draft.
- The primary lane integrated all callers, added summary rehydration and call
  suppression, expanded exact-snapshot coverage, and corrected the draft's
  raw-alternation rule. The draft had also kept the obsolete assistant
  acknowledgment in tests, which was removed.

The two helpers did not edit the same files or duplicate a review lane.

## Proof

- Full Rust workspace: 1,698 passed, two ignored.
- Selected Python handoff suites: 73 passed.
- The 60-case source-executed Python handoff corpus regenerates byte-for-byte.
- Focused Rust tests cover all eight role-selection cases, all continuation and
  anchor cases, structured and string carrier normalization, all eleven
  reference-only call cases, exact snapshot mismatch, wide-column preservation,
  search deduplication, rollback, and immutable-parent rotation.
- Live HTTP and SQLite tests prove summary publication before the next provider
  request, strict provider-visible role repair, retained live input, archived
  source searchability exactly once, and same-turn continuation.
- Formatting, Clippy with warnings denied, and diff hygiene pass.

## Explicit remaining work

This closes the planned handoff assembly seam. The compression cluster still
needs mid-turn rotation, required memory checkpoints, extension notifications,
overflow recovery, configured multi-provider fallback chains, credential
rotation, and non-chat auxiliary transports.

The larger port still needs native plugin and external-memory managers,
terminal/file/browser/MCP breadth, approvals and clarification, delegation,
remaining provider behavior, additional platforms, jobs, and desktop/TUI
backend parity.
