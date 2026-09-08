# Native same-turn proactive pruning resolution

## Result

The count-based proactive prune now runs at Python's active-loop boundary: a
complete tool-result batch is durable first, pruning may atomically rewrite the
active generation next, and the following provider request uses the rewritten
transcript. The existing next-turn preflight remains as restart and non-tool
maintenance coverage.

## Source-verified ordering

Claude owned only the runtime source trace in `same-turn-prune-claude.md`. It
confirmed this order in `agent/conversation_loop.py`:

1. Persist the assistant tool-call row before tool side effects.
2. Persist each tool result before advancing.
3. When full compression did not run, attempt proactive pruning.
4. Adopt a committed replacement transcript without re-persisting its rows.
5. Issue the next provider request.

The prune is fail-open and does not consume the full-compression attempt budget.
It performs no explicit client or cache reset. Changed historical wire bytes
implicitly create a new provider-cache prefix, and durable hysteresis prevents
repeated small cache breaks.

## Native integration

The per-conversation client freezes `AutomaticCompressionPolicy` alongside its
system prompt and toolset. `native_tools.rs` calls one maintenance hook only
after every actual tool result in the batch has been appended. `TranscriptModel`
owns the database, session identity, and admitted lineage-lease holder needed
to implement that hook without giving the generic loop gateway internals.

The hook sizes a projected non-streaming request including frozen tools and
provider extras. It applies the configured trigger, head/tail availability,
durable rearm, full-threshold bypass, and minimum-reclaim gates. It resolves a
durable route, then the SQLite transaction rechecks that route, the exact active
snapshot, the live session, and the unexpired lineage lease before rewriting.

The transcript validator accepts a complete turn or an in-progress turn whose
latest assistant tool-call group has every result. It rejects a lone user,
dangling calls, orphan results, duplicate call IDs, and reordered groups. After
commit, durable replay replaces the loop-owned messages while retaining the one
immutable system row. Pre-commit errors return the original list. A rare
post-commit replay failure is logged and the turn continues, matching the
maintenance path's fail-open contract.

## Helper separation and disposition

- Claude mapped only runtime ordering, identity-based adoption, persistence
  markers, gates, and cache behavior. Its insertion point was accepted.
- AGY independently mapped only the token-budget pruning algorithm in
  `token-budget-prune-agy.md`. No part of that unimplemented algorithm was mixed
  into this runtime checkpoint.
- The primary lane owned interface design, SQLite safety, client construction,
  the HTTP integration test, source verification, and validation.

AGY ran once behind the wrapper's exclusive auth lock. The Claude lane ran in
parallel because it touched a different question and output file.

## Validation and remaining work

The HTTP integration test executes a real large-output tool and observes the
next provider request in the same turn. The request contains the deterministic
summary and excludes the archived marker text. The final reply succeeds, the
active database generation is pruned, and full-text search still finds the
archived original.

Token-budget boundary selection and protected-tail pressure demotion remain the
next pure pruning slice. Same-turn LLM summary compression, micro-compaction,
exact Python estimator parity, auxiliary model routing, memory checkpoints, and
extension notifications also remain in the compression cluster.
