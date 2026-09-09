# Native interactive terminal approval resolution

## Production scope

Native Unix-local terminal conversations now support manual approval on the
Telegram, Discord, and Slack push adapters when Tirith is explicitly disabled.
Approval mode off remains native with the same unconditional hardline, sudo
stdin, and user deny checks. Smart approval, Tirith-enabled manual approval,
synchronous HTTP and CLI surfaces, remote backends, and non-Unix execution stay
on the Python path.

The provider-facing terminal and process schemas remain frozen for the life of
the conversation. Approval prompts and replies use a gateway control plane and
never enter the model transcript. Only the deterministic terminal result note
records that an approval occurred.

## Ownership and flow

One gateway-owned `ApprovalBroker` holds bounded, route-scoped pending requests
and session grants. The current admitted route and sender travel in the
turn-local tool context, so a cached client resumed onto another route cannot
retain stale authorization identity. A terminal call registers a redacted immutable request,
publishes `StreamEvent::ApprovalRequest`, and waits without holding a broker
lock. Push ingress resolves approval text before session admission and before
the transcript lease, so the reply can wake the waiting tool call even while
the original turn owns that lease.

The request carries the current turn's sender identity. The gateway requires
both an exact sender match and normal `/approve` authorization. Unauthorized or
malformed replies do not consume a request. Resolution is FIFO and exactly
once, including reply-timeout races. Dropping a waiting turn unlinks its request
immediately. Reset, resume, freshness rotation, and shutdown cancel pending
requests and clear route session grants.

This boundary follows the codebase-design skill's deep-module guidance. The
broker owns routing and decision lifecycle, the terminal owns execution policy,
and each stream consumer owns presentation. Exact-ID button callbacks and
reconnect acknowledgements were not added without a current consumer.

## Policy compatibility and security order

Every call reloads the live approval policy with last-known-good fallback. The
order is:

1. unconditional hardline blocks
2. unconfigured `sudo -S` protection
3. user `approvals.deny` rules
4. mode-off bypass
5. permanent command allowlist
6. native dangerous-command classification
7. route session grant or human decision

The classifier consumes a checked-in, source-executed Python corpus with 251
cases. It covers the ordered 99-pattern table, nested shell carriers, wrapper
and interpreter option ownership, read-tool execution flags, quoted PCRE
patterns, simple echo substitution, historical approval-key aliases, and
bounded fail-closed parsing.

Manual decisions support once, session, permanent, deny with a bounded reason,
timeout, cancellation, and broker overload. Session grants survive frozen
client eviction because they belong to the broker. Permanent grants serialize
in-process writes, re-read the latest YAML while holding the shared lock, union
the new key, and preserve comments through lossless atomic replacement.

Cross-process configuration writers can still race, matching the Python path's
current limitation. Permanent allowlist entries are normalized into sorted
order. Native manual approval does not claim PTY prompts, notification watchers,
remote execution, smart guardian decisions, Tirith findings, approval buttons,
or reconnect replay.

## Behavioral proof

- The 76-case interactive oracle pins reply parsing, prompt text, scope rules,
  sender authorization, timeout, cancellation, overload, and batch behavior.
- The 251-case dangerous-command oracle regenerates byte-for-byte and is read
  by the Rust classifier test.
- A dispatcher integration test holds the transcript lease, delivers an
  approval prompt, resolves the reply through pre-lease ingress, resumes the
  original turn, and proves neither control message entered SQLite history.
- A real local provider and SQLite test performs a manual approval, reuses its
  session grant, persists cwd, and proves byte-identical tool schemas across
  five provider requests.
- Focused tests cover current-sender authorization, session-grant survival
  across terminal reconstruction, dropped waiter cleanup, timeout races,
  permanent allowlist merging, lossless YAML, and runtime policy changes.

Validation completed with 1,795 Rust tests passed and two ignored. A relevant
Python approval and gateway set passed 305 tests across isolated commands. The
optional Slack Python adapter test was not collected because this checkout's
virtual environment lacks `aiohttp`; native Slack routing is covered by Rust.
Both Python oracles reproduce their checked-in goldens exactly. Formatting,
Ruff, Clippy with warnings denied, and diff hygiene pass.

## Helper disposition

AGY owned the independent Python contract work in two serialized runs: the
interactive approval oracle and the exhaustive dangerous-command oracle.
Claude first drafted the isolated broker and later performed a separate
security and concurrency review. The primary lane verified every result against
the source, corrected oracle and review mistakes, integrated the production
route, removed speculative APIs, and fixed the review findings before commit.
A separate Claude classifier draft timed out and produced no files, so it did
not duplicate or displace the AGY classifier contract.
