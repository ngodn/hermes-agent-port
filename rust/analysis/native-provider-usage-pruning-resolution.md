# Native provider usage and proactive pruning resolution

## Result

This checkpoint connects three previously separate native seams:

1. Provider responses now produce canonical usage buckets and persist them by
   main or auxiliary task.
2. The count-based deterministic tool-result prune is live at the next
   admitted pre-turn boundary and publishes through an atomic wide-row rewrite.
3. Native tool-call groups are persisted incrementally and replayed on later
   turns, so the pruning target is the provider-visible transcript rather than
   dead database state.

The immutable system prompt and frozen tool schema remain unchanged for the
conversation. A successful prune changes the historical transcript only at an
allowed compression boundary. It does not evict the conversation client. The
changed wire bytes implicitly establish the new provider-cache prefix on the
next request, matching Python.

## Helper lanes and disposition

Work was split by dependency and file ownership.

- AGY mapped `agent/usage_pricing.py` and drafted a standalone parser. The
  source map was useful, but the draft was replaced because it rejected
  Python's `true -> 1` coercion, added unsupported fallback keys, and exposed
  unnecessary API surface. See `provider-usage-agy.md`.
- One Claude lane implemented only the pure, no-I/O count-based pruning module.
  It did not touch ingress, SQLite, provider code, or shared configuration. See
  `tool-result-prune-claude.md`.
- A second Claude lane mapped only the prune persistence contract. It showed
  that a prune must clone the wide row set and merge the rearm key in the same
  transaction. See `tool-prune-persistence-claude.md`.
- A later Claude lane traced only Python's incremental tool-history ordering.
  It identified the required flush-before-side-effect and
  result-before-next-request guarantees. See `native-tool-history-claude.md`.
- The primary lane owned all shared types, schema migrations, transactions,
  runtime integration, source review, end-to-end tests, and final validation.

AGY was never run concurrently with another AGY process. Its wrapper holds an
exclusive lock for the full invocation because the underlying rotating auth
state is not safe to race. Claude lanes may run concurrently only when their
outputs and decisions are independent.

## Usage accounting

`provider_usage.rs` implements the narrow JSON boundary required by the native
Chat Completions client while retaining tested Anthropic Messages and Codex
Responses normalization for later transports. It follows Python's provider and
API-mode precedence, nonnegative coercion, cache subtraction, and reasoning
detail selection. Arithmetic saturates at `u64::MAX`, the only representational
difference from Python's unbounded integers.

Streaming requests ask for usage chunks except on the native Gemini host, which
matches Python's exclusion. Non-streaming tool-loop and compression requests
read root usage. Main-turn totals and the empty-task model row update in one
SQLite transaction. Compression usage writes only the `compression` model-task
row and does not inflate main session totals. An unconditional open-time healer
rebuilds the stale Python five-column usage primary key with `task` in the key.
The copy preserves legacy and orphan rows, runs in a foreign-key-off window,
restores enforcement, and leaves healthy schemas untouched.

## Prune publication

The live prune path reads the exact active snapshot, checks configured trigger
and rearm thresholds, runs the pure transformation, and refuses to write when
the candidate is unchanged or does not meet minimum reclaim. Its publication
transaction then:

- verifies the lineage-root turn lease, durable route, live session, and exact
  active snapshot;
- archives every original active row as `active=0, compacted=1`;
- clones all columns into a new active generation;
- rewrites only clean/API content and tool-call arguments selected by the pure
  candidate;
- merges `_proactive_prune_rearm_tokens` without discarding other model config;
- recomputes active message and tool-call counters;
- commits all changes together or rolls all of them back.

The active provider request is rebuilt from durable wide history. Assistant
tool calls, tool results, identifiers, names, reasoning fields, and API content
sidecars therefore survive ordinary turns and prune publication.

## Incremental tool persistence

For a live native tool round, the assistant tool-call row is validated and
committed before the tool executes. Each completed result is validated against
the pending call IDs and committed before the loop can execute another provider
request. The transaction rechecks the durable lineage lease when one is
available. Private empty-response scaffolding is never passed to the store.

An HTTP/SQLite/provider integration test proves both ordering points and then
starts another turn to prove the complete group is replayed. A separate prune
integration test proves the next provider request contains the compact summary
and excludes the archived large result.

## Explicit remaining work

This is not full compression parity. The live prune currently runs before the
next admitted turn, not immediately after each tool result. Token-budget tail
selection, protected-tail pressure demotion, exact token-estimator parity,
micro-compaction, aggressive deletion, auxiliary model routing/fallback,
memory checkpoints, extension notifications, and final request-savings
measurement remain. The broader native plugin, external-memory, tool, approval,
delegation, and provider-failover systems also remain part of the full port.
