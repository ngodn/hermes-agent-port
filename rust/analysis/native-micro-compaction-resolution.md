# Native micro-compaction resolution

## Shipped behavior

The native conversation client now owns an opt-in rolling compaction state for
the lifetime of one conversation. The post-persist finalizer runs at most one
auxiliary compression request after a successful completed turn, before
external-memory completion and before another provider request can be admitted.

The pure pass uses the existing Python-compatible token tail selector. It
protects the configured head and recent tail, finds one complete
assistant/tool exchange, keeps every user message, and replaces the exchange
with an assistant summary marker. A later cumulative pass supersedes only a
marker whose content was rehydrated into the rolling summary. Removing that
marker can make user rows adjacent, so plain-text rows are joined with `\n\n`
and their stale `api_content` is cleared. Structured user rows are not merged.

Three failures at one cursor skip that exchange without deleting it.
Defragmentation takes priority when the rolling summary reaches its configured
token threshold, rewrites only the contained marker, and does not absorb an
exchange in the same pass.

## Auxiliary boundary

The request is non-streaming and tool-free. It uses the current native route,
the `compression` usage task, a 1,500-token ceiling, and temperature `0.1`
subject to provider fixed/omit rules. The serializer strips inline assistant
reasoning, replaces media delivery directives, labels images, bounds tool
arguments and large bodies, and redacts secrets. Output is redacted again and
is rejected when it is empty, reasoning-only, tool-calling, or stopped by the
length limit.

## Atomic publication

`SessionDb::publish_gateway_micro_compaction` uses one immediate SQLite
transaction. It verifies the unexpired lineage turn lease, open session, exact
active snapshot, candidate row identity, and complete role/tool sequence.

Carried-forward rows are cloned from their source IDs, preserving every wider
column. Their archived originals are marked `active=0, compacted=0` so FTS does
not return duplicates. Absorbed assistant/tool rows are marked
`active=0, compacted=1`, keeping them searchable. The new marker is inserted
with `_compressed_summary=1`, and active message/tool counters are reconciled
before commit. In-memory rolling state advances only after that commit.

## Helper division and corrections

The requested helper scripts were used on disjoint work:

- AGY ran once behind its exclusive auth lock and mapped the Python runtime,
  transaction, and post-turn contracts.
- Claude generated and verified a 21-case source-executed Python oracle and
  golden corpus.
- The primary lane owned Rust types, production code, integration, helper
  corrections, full validation, audit scoring, and commit publication.

The AGY report initially proposed obsolete or unnecessary Rust seams. Those
were corrected against the live tree. Configuration belongs in the existing
frozen `AutomaticCompressionPolicy`, and the safe runtime seam is the cached
client's `finalize_turn_after_persist`, which already receives the exact
`SessionDb` and durable lease holder.

## Verification

- Full Rust workspace: 1,647 passed, two ignored.
- Selected Python compression contracts: 249 passed, one skipped.
- Micro-compaction oracle: 21 cases regenerated and checked against Python.
- Focused Rust coverage includes cadence, complete tool exchanges, cumulative
  supersession, user-byte retention, resume, failed rehydration, defrag,
  bounded failures, serializer safety, truncated output, exact-snapshot and
  lease rejection, injected rollback, and live HTTP plus SQLite adoption.
- Formatting, Clippy with warnings denied, and diff hygiene passed.

## Remaining compression work

Same-turn LLM summary compression, exact remaining synthetic/multi-user tail
anchors, configurable auxiliary max-summary cap, auxiliary model routing and
fallback, checkpoint hooks, extension notifications, overflow recovery, and
structural backoff remain. The larger native plugin, memory, tool, provider,
and platform surfaces also remain.
