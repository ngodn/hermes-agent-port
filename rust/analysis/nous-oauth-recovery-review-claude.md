# Native Nous OAuth compression discovery and recovery: post-implementation review

Scope: security, concurrency, and prompt-cache review of the uncommitted Nous
slice only. No production code, tests, or the AGY oracle were changed. This
does not re-derive the Python behavior corpus that AGY owns; where I cite
Python it is to verify a shared filename, an allowlist, or an active-provider
rule the task asked me to confirm rather than assume.

## Primary disposition

All five actionable findings were independently reproduced or checked against
Python before commit. Four were fixed and one was rejected.

- M1: fixed by returning valid local tokens before acquiring the shared lock.
  Refresh-due paths still hold that lock through peer adoption, POST, and
  durable publication.
- M2: rejected. Python uses a reuse phrase only to improve the actionable error
  message. `_is_terminal_nous_refresh_error` still requires `invalid_grant`,
  `invalid_token`, or `refresh_token_reused` before the resolver quarantines the
  state. Rust keeps that same three-code terminal boundary and omits the
  untrusted description from logs.
- L1: fixed by rejecting a shared record unless both access and refresh tokens
  are present.
- L2: fixed with a private atomic mirror writer that replaces rather than
  follows a symlink, matching Python's `os.replace` behavior.
- L3: fixed by asserting in the real refresh and inference integration that an
  out-of-allowlist response URL is healed before persistence.

N1 and N2 remain explicitly scoped future work. No unresolved verified
correctness, security, concurrency, or prompt-cache finding from this review
remains in the implemented checkpoint.

Files reviewed in the working tree (not the stale git-status snapshot):

- `rust/crates/hermes-gateway/src/nous_credentials.rs` (new, the resolver)
- `rust/crates/hermes-gateway/src/auth_store.rs` (lock transaction)
- `rust/crates/hermes-gateway/src/native_agent.rs` (discovery + recovery)
- `rust/crates/hermes-gateway/src/main.rs` (discovery wiring)
- `rust/crates/hermes-gateway/src/atomic_file.rs` (write contract)

Corrected starting fact used throughout: owned Nous `device_code` secrets are
persisted into the pool (`sync_pool`, `nous_credentials.rs:641-735` writes
`access_token`/`refresh_token` into the `credential_pool.nous` row). The live
`is_borrowed` exemption for `(nous, device_code)` is real, so my earlier seam
note that called that pair borrowed/sanitized was wrong.

## Correctness and concurrency findings (implemented slice)

### M1 (Medium, concurrency): shared-store lock is taken on the valid-token read path, which Python avoids

`resolve_sync` acquires the cross-profile shared flock unconditionally at
`nous_credentials.rs:156-165` and holds it for the rest of the call, then reads
and merges the shared file at `166-169` on every resolve, including the
valid-token fast path that returns at `186-198`.

Python does the opposite. `resolve_nous_runtime_credentials`
(`hermes_cli/auth.py:7065`) only enters `_nous_shared_store_lock` and only calls
`_merge_shared_nous_oauth_state` when the access token is missing or when
`force_refresh`/`invoke_jwt_status` says a refresh is due. When the local token
is usable it never touches the shared store or its lock.

The shared lock file is one path for every profile
(`shared/nous_auth.lock`), while the profile auth lock is per profile. So the
Rust version serializes all profiles through a single lock on every compression
resolve, even pure reads. That is exactly the cross-profile contention the
shared-store design and the cited "120 subagents, ~540 401s in eight minutes"
incident were meant to remove. This is not a token-loss bug: on lock timeout
`acquire_for` returns `TimedOut`, `resolve` maps it to `FailureKind::Transient`,
`prepare` returns `Ok(None)` (`native_agent.rs:558-586`), and the Nous
candidate is skipped for that attempt. The cost is added latency and occasional
skips under fan-out. Side effect worth noting: because the valid path also
merges shared unconditionally, a still-usable local token can be swapped for a
peer's fresher one even when no refresh was needed, which Python never does.

Smallest faithful fix: gate the shared read/lock on the same condition Python
uses (missing token or refresh due), leaving the valid-token return at
`186-198` lock-free.

### M2 (Medium, correctness): terminal reuse detection ignores the error-description heuristic

Rust decides terminality purely from the response `error` code:
`nous_credentials.rs:238-242` reads `payload["error"]` (defaulting to
`invalid_grant`) and marks terminal only for
`invalid_grant | invalid_token | refresh_token_reused`.

Python is broader. `_refresh_access_token` (`hermes_cli/auth.py:6621`) also
promotes to terminal when the `error_description` contains `reuse` or
`reuse detected`, regardless of the machine code. A Nous response that carries a
non-terminal `error` code but a reuse description is therefore treated as
`Transient` in Rust: the spent refresh token is not quarantined
(`quarantine`, `244`), the shared copy is not removed (`246`), and the same
single-use token is replayed on the next compression. That produces repeated
4xx until the server happens to return a properly coded reuse error. The path
is narrow (needs an explicit non-terminal code plus a reuse phrase), which is
why this is Medium and not High, but it defeats the single-use safety the whole
module is built around.

### L1 (Low, divergence): shared record without an access token is still adopted

`merge_shared_state` only requires a non-empty `refresh_token` to proceed
(`nous_credentials.rs:811-818`) and then copies whatever fields are truthy.
Python's `_read_shared_nous_state` (`hermes_cli/auth.py:6297`) returns `None`
unless BOTH `refresh_token` and `access_token` are present and non-empty, so a
partial shared record is ignored. The Rust version will merge a refresh token
from a partial shared file and drive a refresh off it. Benign in practice
(a fresh refresh still happens under the lock), but it is a real behavioral
drift from the corpus and could adopt a shared refresh token the Python side
would have skipped.

### L2 (Low, divergence): shared token file is written through a symlink

`write_shared` calls `write_private_preserving_symlink`
(`nous_credentials.rs:886`), which resolves and writes through an existing
symlink (`atomic_file.rs:17-37`). Python's `_write_shared_nous_state`
(`hermes_cli/auth.py:6229`) uses `os.replace(tmp, path)`, which detaches the
symlink and leaves a regular file. This is the same detach-vs-write-through
divergence already flagged for the API-key path, now also applying to the
shared Nous token file. Output is still 0600 and same trust boundary, so impact
is low, but a symlinked `nous_auth.json` is followed on write rather than
replaced.

## Missing production behaviors (not defects in the implemented slice)

### N1: Nous compaction model is hardcoded, not Portal-resolved

`nous_credentials.rs:16` pins `DEFAULT_MODEL = "google/gemini-3.6-flash"` and
`main.rs` uses it as the Nous discovery model. Python resolves the compaction
model dynamically from the Portal recommended-models payload
(`get_nous_recommended_aux_model`, `hermes_cli/models.py:1270`, reached via the
nous model-provider plugin) with a fallback default. The in-tree note at
`agent/auxiliary_client.py:1012` records that a hardcoded Nous model already
404'd once when the Portal dropped it upstream, so the static pin will drift.
This is a deliberately narrow slice, so this is a gap to schedule, not a bug.

### N2: interactive and device-code login remain unported

The resolver is refresh-only. It fails `Unavailable` when there is no refresh
token (`nous_credentials.rs:201-212`) and never runs the device-code or
interactive login flow. Consistent with the seam plan; listed here so it is not
mistaken for covered.

## Test sensitivity

The real HTTP and filesystem tests are sensitive to the core invariants:

- Persist-before-retry and fail-closed on commit failure: the async test asserts
  `persisted_before_retry` is true after the retry
  (`native_agent.rs:4972`), and `rotated_token_is_not_returned_when_auth_commit_fails`
  (`nous_credentials.rs:1062-1144`) asserts a `Persistence` failure and that the
  shared file was not written.
- Byte-stable summary and tool-free body: the compression test asserts
  `bodies[0] == bodies[1]`, `body.get("tools").is_none()`, and the tags prefix
  (`native_agent.rs:4975-4978`); the startup test asserts the Nous request body
  equals the main request body and carries no tools (`main.rs:2660-2664`).
- Refresh token on the wire in the header plus the form grant: asserted at
  `native_agent.rs` endpoint (`headers["x-nous-refresh-token"]` and the
  `grant_type=refresh_token` body check). This matches Python
  (`hermes_cli/auth.py:6587-6594`).
- Peer adoption without re-POST: `forced_refresh_adopts_peer_rotation_from_root_without_posting`
  (`nous_credentials.rs:1146-1221`) asserts zero POSTs and that the borrowed
  state was rotated in the root store, not a new profile-local copy.
- Terminal quarantine: `terminal_refresh_quarantines_singleton_pool_and_shared_copy`
  (`nous_credentials.rs:1223-1311`) asserts token removal, `last_auth_error.code`,
  singleton pool collapse to the surviving manual entry, and shared-file removal.

Gaps:

- L3 (Low, test coverage): the compression test injects an out-of-allowlist
  `inference_base_url` in the refresh response
  (`native_agent.rs:4802`, `https://attacker.invalid/v1`) but never asserts the
  persisted value was healed to the default. The retry succeeds regardless
  because the operator override wins as the effective base URL, so the
  network-provenance allowlist wiring on the persist path
  (`nous_credentials.rs:308-314`) is exercised only by the helper unit test
  `network_urls_are_allowlisted_but_operator_overrides_are_cleaned_only`
  (`nous_credentials.rs:918-936`), not end to end. Add an assertion that the
  persisted `providers.nous.inference_base_url` is the default after that
  refresh.
- No test drives the M2 path (non-terminal code plus a reuse description) or the
  M1 contention path, so neither is caught by the current suite.

## Verified correct (spot-checked against Python and the live diff)

- Lock ordering is profile then root then shared, matching Python's documented
  auth-before-shared order. `lock_provider_state` takes the profile lock, then
  the distinct root lock only when the profile does not own the provider
  (`auth_store.rs` transaction), and `resolve_sync` takes the shared lock after
  (`nous_credentials.rs:128-165`). Python: `_provider_state_transaction`
  (`hermes_cli/auth.py:1557`) plus inner `_nous_shared_store_lock`.
- No lock is held across an `.await`. `resolve_sync` is fully synchronous on a
  `spawn_blocking` worker and drops both flocks when it returns
  (`nous_credentials.rs:104-121`). The recovery arm awaits `resolve`, which owns
  the blocking hop internally (`native_agent.rs:690-724`).
- Cancellation after the single-use POST does not lose the token. The blocking
  task persists the rotated pair (`nous_credentials.rs:321`) before returning,
  and tokio does not cancel an in-flight `spawn_blocking` body, so a dropped
  caller future still commits the rotation; the next `prepare` re-reads it.
- Persist happens before JWT validation and before the token is returned
  (`nous_credentials.rs:321-339`); a post-refresh validation failure cannot drop
  the rotated refresh token.
- Peer-adopt predicate and skew match: adopt only a usable token different from
  the stale one (`176-184`), with `invoke_jwt_status` requiring
  `inference:invoke` and a 120s skew (`19`, `476-497`), matching
  `_already_rotated_by_peer` and `NOUS_INVOKE_JWT_MIN_TTL_SECONDS = 120`
  (`hermes_cli/auth.py:121`).
- SSRF and redirect posture: the refresh client disables redirects
  (`refresh_client`, `nous_credentials.rs:372`) so the bearer and refresh token
  cannot follow a redirect off a Nous host. Portal is allowlisted to
  `https://portal.nousresearch.com` or http loopback
  (`validate_stored_portal_url`, `418-424`), and network-provided
  `inference_base_url` is allowlisted to `inference-api.nousresearch.com`
  (`validate_network_inference_url`, `426-435`), matching
  `_NOUS_PORTAL_ALLOWED_HOSTS` (`hermes_cli/auth.py:3072`). Operator env
  overrides are `clean_url`-only by design, matching Python, and the operator
  inference override is used for the return value but never persisted
  (`308-314`, `329-334`).
- active_provider parity: `persist_state` sets `active_provider = "nous"`
  (`nous_credentials.rs:631`), which matches Python's
  `_save_provider_state_to_source` in both the same-store and borrowed-root
  branches (`hermes_cli/auth.py:1611-1651`, `set_active=True`). The no-op skip
  (`nous_credentials.rs:189`, gated on `shared_changed || routing_changed ||
  changed`, where `changed` excludes TTL countdown fields via `effective_state`,
  `590-599`) mirrors Python's `_persist_state` mtime-cache-preserving skip, so
  an unchanged resolve neither rewrites the file nor flips active_provider.
- Shared filename parity: shared store is `shared/nous_auth.json` and its lock is
  `nous_auth.lock` via `with_extension("lock")` (`auth_store.rs` +
  `main.rs` wiring), matching `NOUS_SHARED_STORE_FILENAME` and
  `_nous_shared_store_lock` (`hermes_cli/auth.py:6117`, `6180`).
- Terminal shared-clear parity: Rust removes the shared file on terminal
  (`nous_credentials.rs:246`, `266`); Python does the same via
  `_quarantine_nous_oauth_state -> _clear_shared_nous_state`
  (`hermes_cli/auth.py:6472`).
- Credential hygiene: all writes are 0600 with owner preserved and no symlink
  privilege escalation on create (`atomic_file.rs:50-66`, `99-103`); no raw
  token reaches logs (recovery warns through `compression_redact::redact`,
  `native_agent.rs:566-569`, `716-719`, and quarantine records only a code and
  reason, `nous_credentials.rs:737-763`).
- Recovery gating is auth-only for Nous: `can_recover` returns true for Nous
  only on an auth failure (`native_agent.rs:544-553`), so 402/429 do not trigger
  a refresh, and the recovery budget stays bounded by the existing two-candidate
  discovery attempts.
