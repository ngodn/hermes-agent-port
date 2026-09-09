# Native compression handoff framing and tail anchors

## Outcome

Native full compression now reads `compression.min_tail_user_messages` with
the same coercion and floor as Python. Values above one pull the token-selected
tail boundary backward until the last N real actionable user turns survive.
The default value remains one and keeps the earlier single-user causal anchor
unchanged.

The classifier used by the multi-user extension excludes persisted summary
carriers, both continuation placeholders, max-iteration and retry nudges,
background-process notices, todo snapshots, and blank text echoes. Structured
image input remains actionable. The final forward alignment still prevents a
tail boundary from splitting a tool-call/result group.

Native batch handoffs now use Python's exact current summary prefix, historical
task heading, end marker, and continuation strings. Re-compression recognizes
the legacy prefix, all five frozen Python prefix generations, the earlier
native prefix, and current merged carriers. It strips stale framing and the end
marker before a summary body is fed back to the summarizer. Rolling
micro-compaction uses the shared classifier and normalizer instead of checking
only the latest prefix.

## Team split and review

The work was split by independent output:

- AGY, under the existing exclusive auth lock, implemented only the
  multi-user tail draft and configuration path.
- Claude produced only the 60-case source-executed handoff oracle, golden
  corpus, and source map.
- The primary lane verified both against current Python, corrected the draft's
  default-anchor and test assumptions, simplified the Rust API, connected the
  same-turn caller, and ran final validation.

This avoided asking two helpers to solve or review the same problem.

## Proof

- Full Rust workspace: 1,669 passed, two ignored.
- Selected Python handoff and tail suites: 62 passed.
- The 60-case Python handoff corpus regenerates byte-for-byte with `--check`.
- Focused Rust tests pin current constants against that corpus and exercise all
  five historical prefixes, merged normalization, default-anchor stability,
  config coercion, synthetic exclusions, multimodal input, monotonic N-user
  anchoring, and tool-boundary behavior.
- Formatting, Clippy with warnings denied, and diff hygiene pass.

## Explicit remaining work

This checkpoint does not claim the full handoff assembly algorithm. Rust still
publishes its established two-row summary and acknowledgement pair. Python's
template-visible role selection, merge-into-tail layouts, zero-user
continuation or real-user insertion, and reference-only next-call suppression
remain to be wired through an exact-snapshot publication plan that can clone
and rewrite retained wide rows without losing tool or provider replay fields.

The source-executed oracle already pins those behaviors for the next
checkpoint. Mid-turn rotation, overflow recovery, required memory checkpoints,
and auxiliary fallback breadth also remain in the compression cluster.
