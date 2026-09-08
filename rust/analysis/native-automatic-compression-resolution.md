# Native automatic compression resolution

Date: 2026-09-08

## Landed behavior

Native HTTP and push turns now run automatic compression after stable-route,
transcript, and cross-process lineage admission, but before the inbound user
message is persisted. The triggering message is included in pressure sizing
and excluded from the history summarized as older context.

The native client sizes its final provider-visible request after adding the
immutable system prompt, full active transcript, inbound structured content,
frozen tool schemas, provider request hooks, prompt-cache fields, and output
limit. The resolved models.dev context window and output reservation feed the
Python-compatible ratio, small-window floor, absolute cap, and per-model
override policy. The estimate is serialized request bytes divided by four
until provider usage capture lands.

The old Rust-only 40-message history truncation was removed. Native turns now
load the full active transcript, matching Python and making compression the
explicit context-bounding mechanism.

## Boundaries and publication

The first compression preserves the configured initial head, aligned down to a
complete turn when the raw count would split alternation or a tool group.
Later passes detect a durable checkpoint and decay head protection to zero.
The configured recent message count is aligned backward to a complete-turn
boundary. The middle alone is sent to the existing bounded, redacted,
tool-aware summarizer.

Both publication modes work under the same admitted locks:

- default `compression.in_place: true` keeps the session ID and cached client
- explicit false creates a child, updates the stable route, rebinds the process
  transcript lease, and releases the old physical client without ending the
  logical conversation

SQLite can now clone a byte-exact protected prefix before the summary pair and
a byte-exact protected or concurrently appended tail after it. Tool metadata,
API content, timestamps, display fields, and wider Python columns survive.

## Durable retry guards

The sessions schema now carries Python-compatible summary cooldown and
ineffective-compression state. A live 600-second summary-failure cooldown and
an armed two-strike breaker block automatic retries. An expired recovery
deadline permits one probe. A non-shrinking summary records a strike and arms a
300-second recovery window on the second strike. Successful publication clears
both guards.

## Concurrency proof

Admission retains its route lease only through pre-turn maintenance. Automatic
publication therefore cannot race reset, resume, or another compressor. The
existing transcript and durable lineage leases remain held throughout summary
I/O and publication. The route lease is dropped before the ordinary provider
turn, so unrelated route operations are not blocked for the full response.

HTTP integration proves same-ID in-place compaction happens before persistence
of the triggering user turn. Push integration proves explicit rotation, route
publication, lease rebinding, parent closure, and the triggering turn landing
only in the child.

## Helper disposition

The requested helpers were used as separate team lanes, not duplicate audits:

- AGY owned pure policy parsing and decisions only.
- Claude owned durable SQLite guard columns and APIs only.
- The main lane independently verified both against Python, corrected their
  mistakes, designed and implemented ingress, request sizing, locking,
  publication, and end-to-end tests.

AGY initially missed small-window/output-reservation threshold behavior and
several Python coercions. Claude initially made the nullable recovery deadline
non-null. Neither draft was accepted unchanged.

## Validation

- Full Rust workspace: 1,589 passed, two ignored.
- Focused Python threshold, attempt, cooldown, and persistence oracle: 26
  passed under Python 3.11.15.
- Clippy with warnings denied: passed.
- Formatting and `git diff --check`: passed.

## Still open

This is a production vertical slice, not full Python compressor parity. The
remaining compression work includes provider-reported usage capture and
recalibration, token-budget tail selection, exact role-collision preservation
for every raw head count, deterministic tool-result pruning, micro-compaction,
structural no-op backoff, final-request 10% savings measurement, overflow
recovery, auxiliary summary model routing and fallback, pre-compression memory
checkpoints, context-engine/plugin notifications, and aggressive deletion.
