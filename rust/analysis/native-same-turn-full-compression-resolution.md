# Native same-turn full compression resolution

## Result

Native full LLM compression now runs after a complete tool-result batch is
durable and before the next provider request in that same turn. This closes the
live ordering gap that remained after pre-turn compression, token-budget
pruning, and micro-compaction were ported.

The implementation is intentionally limited to the Python-default in-place
mode. Mid-turn rotation would require atomically rebinding the immutable client,
session identity, route lease, and transcript lease while a tool loop is live.
That larger lifecycle change remains deferred. `checkpoint_required` also
fails closed because the native pre-compression memory hook is not connected.

## Runtime behavior

The native tool loop now awaits transcript maintenance after it persists every
tool result. The compression gate then:

1. Prefers the latest real provider prompt-token count, excluding completion
   and reasoning tokens.
2. Falls back to full provider-visible request sizing, including tool schemas,
   when usage is absent.
3. Uses a post-compaction sentinel until real provider usage arrives, avoiding
   immediate recompression from a schema-heavy rough estimate.
4. Rearms the per-turn attempt budget only when the next real prompt count
   proves the compacted request is below threshold.
5. Applies the existing durable cooldown and ineffective-compression breaker.
6. Runs deterministic token-budget pruning, selects a complete middle region,
   and makes one tool-free auxiliary summary request outside SQLite.
7. Rejects empty, reasoning-only, tool-calling, length-truncated, and
   non-shrinking summaries.
8. Publishes through the exact-snapshot, current-route, live-session, and
   lineage-turn-lease transaction, then adopts the durable active transcript.

The system prompt stays outside the database rewrite and is copied unchanged
back into the live request vector. Only dynamic conversation history changes,
so the required prompt-cache invalidation is confined to the compressed
history prefix.

The in-place publication validator now accepts a retained partial turn ending
in a fully answered tool-call group. It still rejects dangling or mismatched
tool results. This is the exact shape present between a completed tool batch
and its follow-up model request.

## Transaction and failure policy

Provider I/O never runs inside a SQLite transaction. Phase 1 and final summary
publication each use their existing immediate transaction and compare the
durable route, snapshot, and lease at commit time. A stale publication refunds
the attempt because it is a contention outcome, not evidence that the history
cannot shrink.

Summary failures arm the durable 600-second cooldown. A non-shrinking summary
adds an ineffective strike, with the existing 300-second recovery window after
the second strike. Deterministic Phase 1 pruning may remain committed when the
later summary call fails, matching the Python pipeline.

## Proof

The public HTTP and SQLite integration test starts a routed conversation,
injects old durable turns, receives a high provider prompt count and tool call,
executes the real tool, and verifies its result is searchable before the
summary endpoint is called. The next request in the same turn contains the
summary checkpoint and live tool result, omits the archived early detail, and
preserves the exact frozen system-prompt bytes. The final response is durable,
there is one active summary marker, and the archived source remains searchable.

Focused unit tests cover Python-compatible `in_place` and
`checkpoint_required` coercion, completed-tool-batch validation, prompt-usage
precedence, the post-compaction sentinel, provider-confirmed attempt rearming,
and rejection of truncated, reasoning-only, and tool-calling summaries.

Validation at this checkpoint:

- Full Rust workspace: 1,650 passed, two ignored.
- Selected Python compression suites: 251 passed.
- Source-executed same-turn oracle: 25 cases, regeneration check passed.
- Formatting, Clippy with warnings denied, and diff hygiene passed.

## Helper disposition

The helper lanes were divided by independent deliverable. AGY ran once behind
its exclusive auth lock and produced only the Python runtime contract map.
Claude produced only the deterministic source-executed Python oracle and golden
corpus. The primary lane wrote and integrated all Rust production code, added
the live database test, independently ran the oracle and Python suites, fixed
the summary-response and attempt-lifecycle gaps, and owns this resolution.

## Remaining compression work

- Rotation-mode compression inside a live tool loop.
- Synthetic-user, multi-user, and reference-only handoff edge cases.
- Structural no-op backoff and remaining timeout/overflow recovery behavior.
- Configurable auxiliary caps, auxiliary route selection, and fallback.
- Required memory checkpoints and extension notifications.

These are explicit gaps. This checkpoint does not claim full compression or
full Hermes parity.
