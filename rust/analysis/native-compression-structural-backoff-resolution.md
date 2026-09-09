# Native compression structural backoff resolution

Date: 2026-09-09

## Outcome

Native full compression now distinguishes a structural no-op from a real but
ineffective summary attempt. When an over-threshold pass finds no complete
compressible region, it arms a 300 second monotonic, process-local guard for
that conversation. Automatic retries are deferred while the guard is active,
without incrementing or persisting the durable ineffective-compression
breaker.

The guard belongs to the immutable per-conversation native client and is
shared by its clones. The bounded conversation cache forwards guard reads,
arms, and clears to the already initialized client selected by profile home and
session ID. It is deliberately absent from SQLite, so process restart or
conversation-client eviction resets it just like Python's in-memory compressor
state.

Both automatic entry points use the guard. Pre-turn maintenance evaluates it
alongside the durable summary cooldown and ineffective breaker after sizing the
provider-visible request. Same-turn maintenance checks it after a durable tool
batch and before summary I/O. A successful full-compression publication clears
the guard. Manual `/compress` clears it before the forced attempt, then rearms
it if the transcript is still too short to compress.

## Evidence and corrections

Claude produced a deterministic, source-executed Python oracle with 24 cases.
It covers initialization and reset, absolute deadline replacement, exact
deadline expiry, all three Python structural caller reasons, forced and
successful clearing, cooldown and breaker independence, and overflow bypass
behavior. The golden corpus regenerates byte-for-byte through the real Python
`ContextCompressor` methods.

AGY worked on a separate dependency lane. It mapped synthetic user rows,
multi-user tail anchors, reference-only handoffs, todo snapshot insertion, role
alternation, and restart suppression for the next compression checkpoint. It
did not review or implement this backoff.

Source verification preserved these distinctions:

- The deadline uses monotonic time and expires exactly at zero remaining time.
- Rearming replaces the deadline with `now + 300 seconds`; it does not add to
  the old deadline.
- Structural state never writes the durable cooldown, strike count, or
  recovery deadline.
- Provider-overflow recovery may bypass a summary-failure cooldown, but it must
  not bypass structural backoff when that recovery loop is ported.
- A committed boundary and a manual forced attempt clear the guard. A
  non-shrinking generated summary remains a real ineffective strike.

The public HTTP and SQLite regression first drives an over-threshold transcript
with no compressible region. It then grows the transcript enough to summarize
and proves that a second request makes no summary call while the guard is live.
After an explicit clear, the next request performs exactly one summary call and
commits it. SQLite's ineffective counter stays zero throughout. Separate tests
cover exact timer semantics, clone sharing, conversation-cache forwarding, and
manual forced clearing.

## Deliberate remaining work

- The provider-overflow retry loop and its transient-versus-exhausted outcome
  classification are not native yet.
- Native compression still needs exact synthetic-user construction,
  multi-user tail anchoring, handoff stripping and reference-only suppression,
  todo snapshot coupling, and role-collision assembly. The AGY source map is
  the starting evidence for that work.
- Required pre-compression memory checkpoints, extension notifications,
  mid-turn rotation, configured auxiliary fallback chains, and non-chat
  auxiliary transports remain open.
- This guard does not attempt durable persistence or a recovery-probe ladder.
  Those belong only to the separate durable ineffective breaker.

## Validation

- Rust workspace: 1,659 passed, two ignored.
- Selected Python structural and anti-thrash suites: 46 passed.
- Claude source-executed oracle: 24 cases, regeneration check passed.
- Focused live HTTP, SQLite, cache-routing, timer, and manual command tests
  passed.
- Formatting, Clippy with warnings denied, and diff hygiene passed.
