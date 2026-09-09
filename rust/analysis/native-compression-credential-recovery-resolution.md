# Native compression credential recovery

## Shipped boundary

Native auxiliary auto-discovery now resolves store-backed API-key pools before
environment credentials for OpenRouter and registered chat-completions
providers. Selection is profile scoped. A locator carries only the active and
root `auth.json` paths, provider name, and strategy. It reloads the mutable pool
for each selection or recovery instead of sharing raw credentials between
conversations.

The selected key and optional per-entry base URL are attached to the frozen
conversation route. A 401, payment-like failure, or ordinary 429 is attributed
to the exact dispatched key. Stale IDs cannot punish another entry, and every
row with the same runtime key receives the same cooldown. The failure is
persisted before any replacement request starts.

Recovery follows the current Python request path:

- 401 and payment failures rotate immediately.
- An ordinary 429 retries the dispatched key once, then rotates if the retry
  is still an auth, payment, or rate-limit failure.
- A rotated key receives one request. A second qualifying failure is persisted
  immediately, but no additional rotated-key request is made during that
  compression attempt.
- A replacement uses a newly built `reqwest::Client`, so the failed connection
  pool and authorization state are not reused.
- Provider health is marked only after recovery is exhausted according to the
  existing discovery failure rules.

Every attempt rebuilds the same tool-free summary JSON from the same prompt.
Credential changes do not rebuild the conversation system prompt, tools, or
plugin snapshot.

## Durable store semantics

Pool writes take a process mutex and a cross-process `auth.lock` flock, reread
the current store while locked, preserve concurrent additions, honor explicit
removals, and keep a newer live disk cooldown over stale memory. A changed
token is treated as a re-auth and is not given the old token's cooldown.

Writes use the existing atomic private-file helper. On Unix the target remains
mode `0600`, and an `auth.json` symlink remains a symlink while its target is
updated. A persistence error stops recovery before provider I/O.

## Proof

The source-executed Python corpus contains 64 cases across selection,
eligibility, identity, cooldowns, recovery, health, eviction, retry budgets,
profile shadowing, and transport boundaries. Rust tests consume its recovery
expectations and separately cover exact cooldown timestamps, duplicate-key
quarantine, stale identity attribution, unmatched rotation bounds, concurrent
store merge behavior, token replacement, permissions, and symlinks.

A real local HTTP plus filesystem integration sends the same summary body to
two credentials. It observes `key-one` fail with 401, independently reads the
persisted exhausted status before accepting `key-two`, verifies byte-identical
request bodies with no tools, and proves a later pool load skips `key-one`.
Startup integration also proves a stored GMI key wins over the environment key
and reaches the existing frozen discovery route. A second local HTTP test pins
the distinct retry counts: 402 goes directly to `key-two`, while ordinary 429
calls `key-one` once more before rotating to `key-two`.

## Deliberate limits

This checkpoint covers static API-key pools on the native chat-completions
discovery subset. OAuth refresh and single-use grant write-through for
Anthropic, OpenAI Codex, xAI OAuth, and Nous remain on the Python path.
Non-chat transports, main-provider failover, dynamic provider plugins, and the
general auxiliary client cache remain open. The fresh-client replacement here
solves credential-bound poisoning for this native route but is not a port of
Python's process-wide SDK client cache.

AGY owned the Python contract and executable corpus. Claude first mapped Rust
ownership and concurrency, then reviewed the implemented diff. The primary
lane checked both against the source, corrected the helper report's default
health TTL and retry-count summaries, and integrated the production path.
