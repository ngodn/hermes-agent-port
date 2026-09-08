# Native token-budget compression resolution

## Outcome

Native full compression now uses Python-compatible token budgets for both its
deterministic Phase 1 tool-result prune and its verbatim summary tail. The path
is live on HTTP and push ingress before the triggering user message is written.

The Phase 1 implementation preserves the Python pass order:

1. deduplicate exact tool output
2. summarize eligible output before the protected boundary
3. truncate oversized historical tool-call arguments
4. retire stale image results and clear only their stale `api_content`
5. apply the three-stage protected-tail pressure escape hatch

The pressure stages first reclaim older protected rows while sparing the newest
three messages, then reclaim all protected tools except the newest one, then
demote that final tool only when it alone still exceeds the soft ceiling.
Pressure deliberately overrides ordinary `skill_view` protection and retains
the reload marker.

## Estimator and boundary parity

The shared Rust estimator now accounts for:

- ASCII ceiling division and dense CJK/Hangul code points
- UTF-8 byte weight for other non-ASCII text
- multimodal text and the fixed 1,600-token image estimate
- the complete Python `str(tool_call)` envelope
- always-replayed Codex reasoning and message sidecars
- newest-only generic thinking on strict routes and all-turn thinking on
  echo-back routes
- textual reasoning details without signed or encrypted envelope weight

The pruning boundary uses strict `>` comparison and includes the row where the
walk stops. `protect_last_n` remains a hard minimum capped at eight messages.
The summary-tail walk uses the 1.5x soft ceiling, bounded count floor,
raw-budget retry, complete tool-group alignment, and latest nonblank,
non-summary user and visible assistant anchors. Lean mode retains 2.5% of the
context window clamped to 10,000 through 25,000 tokens. Legacy mode retains the
configured target ratio of the effective compression threshold.

Compression snapshots now load the reasoning and Codex replay columns in the
same SQLite read as clean content and tool metadata. The exact-snapshot publish
guard compares those fields before cloning, so estimator input and mutation
authority cannot come from different transcript generations.

## Runtime publication

An admitted over-threshold attempt computes the deterministic candidate while
the route, transcript, and lineage leases remain owned. A changed candidate is
published through the existing immediate SQLite transaction, reloaded, and
then passed to summary selection. The attempt remains admitted across that
reload, matching Python's single `compress()` call even if deterministic
reclamation alone drops the fresh request below the trigger. No provider call
occurs between the two phases.

The live push test builds a complete transcript with an oversized `read_file`
result inside the protected tail. It proves the active row becomes the
informative summary before the triggering turn is persisted, the original is
still searchable as compacted history, tool IDs remain paired, and the turn
still executes and persists normally.

## Differential proof

`rust/tools/token-budget-prune-oracle.py` executes the real Python
`ContextCompressor`. Its checked-in corpus contains 17 message-estimator cases,
14 complete pruning cases, and 3 summary-tail cases. Rust replays all cases
through one `include_str!` differential test. The generator's `--check` mode
detects source drift.

The helper work was divided by deliverable:

- Claude created only the executable Python oracle, golden corpus, and its
  source report.
- AGY ran once behind its exclusive auth lock and added only the push/SQLite
  integration test.
- The primary lane owned shared types, estimator and boundary code, production
  wiring, helper correction, and final validation.

This avoided asking either helper to duplicate implementation or review work.

## Remaining compression work

This checkpoint does not claim complete compressor parity. The largest open
pieces are micro-compaction and defragmentation, same-turn full LLM summary
compression inside active tool loops, exact synthetic-user and multi-user tail
anchors, structural no-op backoff, final request-savings measurement,
provider-overflow recovery, auxiliary model routing and fallback, pre-compress
memory checkpoints, extension notifications, and stale replay-item stripping.

## Validation

- Full Rust workspace: 1,633 passed, two ignored (1,632 gateway plus one core).
- Selected Python compressor suites: 200 passed.
- Source-executed oracle drift check: 17 estimator, 14 prune, and 3 tail-cut
  cases passed.
- Formatting, Clippy with warnings denied, and diff hygiene passed.
