# Rust seam: native Nous auxiliary discovery plus single-use OAuth refresh for compression

> Primary audit disposition: this is pre-implementation design input, not the
> final contract. The integration audit corrected several source readings in
> the body below. `("nous", "device_code")` is explicitly exempt from borrowed
> sanitization, so its OAuth material is persisted in the owned pool row. A
> trusted Portal environment override can be persisted by the direct runtime
> resolver, while `NOUS_INFERENCE_BASE_URL` remains runtime-only. Network-provided
> inference URLs allow only HTTPS on `inference-api.nousresearch.com`, not
> loopback. Nous auxiliary requests carry Portal tags but do not automatically
> inherit OpenRouter's `HTTP-Referer` header. A missing refresh token requires
> reauthentication but is not one of Python's three terminal quarantine codes.
> The shared lock is `nous_auth.lock`, from `nous_auth.json.with_suffix(".lock")`.
> The final implementation and disposition are recorded in
> `native-nous-oauth-recovery-resolution.md`.

Scope: design the narrowest production seam that lifts the `nous` deferral in
`load_pool_from_store` (`credential_pool.rs:828`) far enough for native
auxiliary-compression discovery to select a Nous credential and to recover a
single-use OAuth refresh at request time, safely across processes and profiles.
This is a Rust ownership, concurrency, and security lane. The behavioral
contract and its Python golden corpus are owned by the parallel agy lane
(`rust/analysis/nous-oauth-recovery-contract-agy.md` plus the generator and JSON
under `rust/tools`, not yet landed here). This document does not re-derive that
contract; it cites Python only where a value pins a Rust interface, and anchors
every Rust symbol to code readable in the tree today.

It builds on the two sibling seams that already landed and that this checkpoint
explicitly excluded:
`rust/analysis/compression-credential-recovery-rust-seam-claude.md` (per-key
rotation for static API-key pools) and
`rust/analysis/compression-builtin-discovery-rust-seam-claude.md` (the built-in
discovery tier and shared `Health`). Those established the frozen
`CompressionDiscovery`, the request-local pool, the `PoolLocator`, the
`CompressionPoolCredential` binding, and the two-attempt discovery budget. Every
one of those left Nous out on purpose because token refresh was unported. This
document is the one that says exactly what refresh needs, and what still stays
out.

## 0. What exists today, precisely

The API-key half of this machinery is already implemented and golden-tested, so
this seam extends a working system rather than inventing one.

- `load_pool_from_store` bails for `nous` (and `anthropic`, `openai-codex`,
  `xai-oauth`, `custom:`) via `SEEDING_REQUIRED` (`credential_pool.rs:828`),
  returning an error rather than a silently wrong pool. That error is what keeps
  Nous out of discovery today.
- Nous is intentionally absent from `build_native_compression_discovery`
  (`main.rs:607`), with a comment naming the exact missing capability: "live
  device-code token validation and refresh, which the native credential manager
  does not own yet."
- The pool already models Nous data. `PooledCredential::runtime_key`
  (`credential_pool.rs:152`) has the Nous branch that prefers `agent_key`
  (validated by `agent_key_expires_at`) then `access_token` (validated by
  `expires_at`), gated through an injected `nous_usable` validator, and
  `runtime_base_url` (`credential_pool.rs:194`) prefers `inference_base_url` for
  Nous. `ENTRY_FIELDS`/`EXTRA_FIELDS` (`credential_pool.rs:6`) already carry
  `access_token`, `refresh_token`, `expires_at`, `agent_key`,
  `agent_key_expires_at`, `scope`, `client_id`, `portal_base_url`,
  `inference_base_url`, `obtained_at`, `expires_in`, `token_type`.
- The validator seam is stubbed. `CredentialPool::runtime_api_key`
  (`credential_pool.rs:1117`) passes `|_, _, _| false` to `runtime_key`, so a
  Nous entry always resolves to an empty key inside the pool. This is the single
  most important stub to replace: it is the NAS invoke-JWT validator.
- `with_failure` already classifies the OAuth terminal reasons
  (`credential_pool.rs:290`): `invalid_grant`, `unauthorized_client`,
  `invalid_token`, `token_revoked`, `token_invalidated`, `refresh_token_reused`
  under status 401 promote an entry to `dead` rather than `exhausted`. So the
  store transition for a terminal refresh already has a home.
- Disk hygiene for Nous deliberately treats `("nous", "device_code")` as an
  owned source. The pair is an explicit exception in
  `credential_persistence.rs:is_borrowed`, so `sanitize` preserves its OAuth
  fields in the pool row.
- `auth_store.rs` already has the Nous-specific `normalize` branches: the
  `portal_base_url` heal from `api.nousresearch.com` to
  `https://portal.nousresearch.com` (`auth_store.rs:37`) and the legacy
  `systems.nous_portal` migration into `providers.nous` (`auth_store.rs:63`).
- Recovery wiring for compression is fully landed. `CompressionPoolCredential`
  (`native_agent.rs:474`) holds a `PoolLocator` plus an
  `Arc<Mutex<Option<RuntimeCredential>>>`; `rotate_after_failure_async`
  (`native_agent.rs:598`) runs the store read-modify-write inside
  `tokio::task::spawn_blocking`; `recover_summary` (`native_agent.rs:631`) does
  the 429-cheap-retry then rotate ladder inside the two-attempt budget.

So the gap for Nous is narrow and specific: the pool cannot be built (the bail),
the runtime key cannot be validated (the stub), and there is no code that turns
a `refresh_token` into a fresh `access_token`. Everything downstream of a
selected, valid Nous credential already works.

## 1. Load-bearing constraints

Two constraints drive the whole design. The first is shared with the API-key
seam; the second is new and is what makes Nous different from every provider
recovered so far.

1. Secrets must not enter process-shared state. `CompressionDiscovery` is
   `Clone` (`native_agent.rs:462`) and cloned per conversation, so anything it
   holds is shared across profiles. `CompressionPoolCredential` already respects
   this: it holds a `PoolLocator` (paths, provider, strategy, no secret,
   `credential_pool.rs:912`) plus a per-conversation `Arc<Mutex<Option<...>>>`
   for the last selected credential, and reloads the durable pool from the
   profile store for every mutation. The Nous seam must keep the refresh token
   and the rotated pair on the same footing: they live only in the request-local
   `CredentialPool` and on disk, never in the `Clone` shared surface.

2. A single-use refresh token is a destructive shared resource. This is the
   crux. Nous rotates the refresh token on every successful refresh, and the
   portal treats replay of a spent token as theft: it revokes the whole session
   chain and returns `refresh_token_reused` (`auth.py:6621`). So two callers
   that both POST the same on-disk refresh token do not merely waste work, they
   destroy each other's credential. The real-world failure this guards is
   documented in `resolve_nous_runtime_credentials` (`auth.py:7083`): 120
   subagents, 81 concurrent refreshes, ~540 401s in eight minutes. The Rust seam
   must make "at most one process POSTs a given refresh token, and losers adopt
   the winner's rotated token" a hard invariant, not a best effort. This is
   stronger than the API-key case, where a duplicate select only loses a
   `request_count` hint.

## 2. Modules and functions to extend

The seam is a new refresh capability threaded through the existing pool load and
rotate path. It does not add a new discovery surface. Exact touch points:

- `credential_pool.rs:828` `load_pool_from_store`: remove `nous` from
  `SEEDING_REQUIRED` only once a Nous seeder exists, and gate the Nous branch on
  a caller-supplied refresh hook. The cleanest shape is a new sibling entry
  point, `load_nous_pool_from_store`, that reads `providers.nous` (the singleton
  OAuth state) plus the `credential_pool.nous` rows, seeds one `device_code`
  entry from the singleton if the pool has none, and constructs the pool. Do not
  widen the generic `load_pool_from_store` signature for the common case;
  Nous seeding is provider-specific and belongs behind its own door, matching
  how Python splits `load_pool` from the Nous resync helpers.
- `credential_pool.rs:1117` `runtime_api_key`: replace the `|_, _, _| false`
  stub with a real NAS invoke-JWT validator for Nous entries. That validator is
  the port of `_nous_invoke_jwt_status` (`auth.py:3218`): decode the JWT claims,
  require the `inference:invoke` scope
  (`NOUS_INFERENCE_INVOKE_SCOPE`, `auth.py:116`) in `scope`/`scp`/state `scope`,
  and require `exp > now + 120s` (`ACCESS_TOKEN_REFRESH_SKEW_SECONDS`,
  `auth.py:121`, mirrored by `NOUS_INVOKE_JWT_MIN_TTL_SECONDS`). Non-usable
  yields an empty runtime key, which is what makes `has_available` and `select`
  treat an expiring Nous entry as needing refresh. This validator holds no
  secret and does no I/O, so it lives in the pure pool layer.
- `credential_pool.rs` new method on `CredentialPool` alongside
  `mark_exhausted_and_rotate` (`:1374`): a `refresh_nous_entry` step that, when
  the selected entry is a Nous `oauth`/`device_code` entry whose runtime key is
  empty (expired) or that just failed 401, invokes an injected refresh callback,
  merges the result into the entry, and persists. The refresh network call must
  not run inside the pool; the pool takes a `Box<dyn FnMut(...) -> Result<...>>`
  refresh hook the way it already takes `PersistSink` and `choose_random`. This
  keeps the pool free of `httpx`/`reqwest` and of the shared-store lock.
- `credential_pool.rs:938` `PoolLocator::load` and `:985`
  `mark_exhausted_and_rotate` / `:977` `select_runtime`: add a Nous-aware
  `select_runtime`/`mark_exhausted_and_rotate` path that carries the refresh hook
  into the request-local pool. The locator still owns "load, mutate, persist,
  drop"; the refresh hook is constructed at the call site.
- New module `nous_oauth.rs` (or a Nous section in a credentials module): the
  actual refresh, the port of `_refresh_access_token` (`auth.py:6580`) plus the
  peer-rotation adoption logic from `resolve_nous_runtime_credentials`
  (`auth.py:7065`). This is the only new code that touches the network and the
  shared Nous store. It owns the HTTP client, the portal-host allowlist, the
  lock ordering, and the error classification.
- `main.rs:607` `build_native_compression_discovery`: add the Nous entry once
  the above exists. It resolves through the same `pool_runtime` helper
  (`main.rs:542`) that already produces `(PoolLocator, RuntimeCredential)` for
  API-key providers, so the discovery-build shape does not change; only the
  provider set grows by one, and the locator carries the refresh hook.
- `native_agent.rs:500` `CompressionPoolCredential::rotate_after_failure`: no
  structural change. It already calls `mark_exhausted_and_rotate` and swaps the
  current credential under the mutex. For Nous the same call must route through
  the refresh-aware select so a 401 triggers a refresh-and-adopt rather than a
  key rotation (there is only ever one Nous key). This is the smallest possible
  behavioral fork: same method, refresh hook present for Nous.

## 3. The two-token model and the HTTP refresh

Nous uses two tokens, and getting this right is load-bearing for the seam.

- The OAuth `access_token` returned by the portal is itself the NAS inference
  invoke JWT. Python sets `state["agent_key"] = access_token`
  (`_set_nous_agent_key_from_invoke_jwt`, `auth.py:3341`). There is no second
  token-exchange HTTP call. So the runtime bearer for a Nous inference request is
  the `access_token` (mirrored into `agent_key`), and the base URL is
  `inference_base_url`. This is why `runtime_key` (`credential_pool.rs:152`)
  tries `agent_key` then `access_token`: they are the same string after a
  successful refresh.
- The `refresh_token` is single-use and only ever POSTed to the portal.

The refresh call, ported from `_refresh_access_token` (`auth.py:6587`):

- Method and URL: `POST {portal_base_url}/api/oauth/token`.
- Headers: `x-nous-refresh-token: <refresh_token>` plus `Accept: application/json`
  (the shared client header). The refresh token travels in a header, not the
  body.
- Body: form-encoded `grant_type=refresh_token`, `client_id=<client_id>`.
  `client_id` defaults to `DEFAULT_NOUS_CLIENT_ID` (`auth.py:7168`).
- Success (200): payload must contain `access_token` or it is a terminal
  `invalid_token` (`auth.py:6598`). The seam reads `access_token`,
  `refresh_token` (the rotated one; fall back to the old one if the server omits
  it, `auth.py:7336`), `token_type` (default `Bearer`), `scope`, `expires_in`,
  and `inference_base_url`. It recomputes `expires_at` as
  `now + expires_in` (`auth.py:7358`) and stamps `obtained_at`.
- `portal_base_url` is a persisted, attacker-influenced value, so it is gated
  by the host allowlist `_NOUS_PORTAL_ALLOWED_HOSTS` (`auth.py:3072`, referenced
  at `7145`) before any POST. A value outside the allowlist heals to
  `DEFAULT_NOUS_PORTAL_URL`. An operator env override
  (`HERMES_PORTAL_BASE_URL`/`NOUS_PORTAL_BASE_URL`) bypasses the gate. The direct
  runtime resolver can persist that effective Portal URL. The Rust port must
  keep the network-value gate on the refresh path: the
  refresh token is the bearer, and a poisoned portal host would exfiltrate it.
  Reuse `local_probe::urlparse_hostname` (already used in
  `auth_store.rs:50`) for host extraction.
- `inference_base_url` from the refresh response is re-validated for network
  provenance before it is persisted (`_validate_nous_inference_url_from_network`,
  `auth.py:7346`); the `NOUS_INFERENCE_BASE_URL` env override is layered onto the
  returned/used value only and never persisted. The seam persists the validated
  network value and returns the override-layered value. The two allowlists are
  concrete: portal hosts are `portal.nousresearch.com` plus loopback
  (`auth.py:3072`), while network-provided inference URLs require HTTPS on
  `inference-api.nousresearch.com` (`auth.py:3102`). Both must ship in the Rust port; they are the
  security boundary that keeps the bearer and the invoke JWT on Nous hosts.

Model and base-URL selection is largely the contract lane's to pin, but two
values touch the Rust interface. The inference base URL defaults to
`https://inference-api.nousresearch.com/v1` and flows through the credential's
`runtime_base_url` (`credential_pool.rs:194`). The auxiliary model default is
`google/gemini-3.6-flash`, overridable by the portal recommendation
(`get_nous_recommended_aux_model`); model resolution stays in the discovery-build
step in `main.rs`, not in the recovery path, so the recovery seam never chooses a
model. Nous requests carry a `tags` extra body. The OpenRouter-specific
`HTTP-Referer` header is not automatically applied to Nous. These request
properties are frozen candidate state, not credential state, so recovery leaves
them untouched.

## 4. Lock ordering and single-use safety

This is the heart of the concurrency lane. Python coordinates three locks; the
Rust port must reproduce the ordering exactly or it reintroduces the reuse
storm.

Python's locks:

1. `_auth_store_lock()` (`auth.py:1374`): the profile `auth.json` cross-process
   flock, entered by `_provider_state_transaction("nous")` (`auth.py:7090`) for
   the whole resolve.
2. `_nous_shared_store_lock()` (`auth.py:6167`): a separate cross-profile flock
   over the shared root Nous store, entered around the merge-and-POST
   (`auth.py:7236`, `7270`).
3. The documented ordering invariant (`auth.py:6170`): acquire the profile
   `_auth_store_lock` FIRST, then the shared Nous store lock. All runtime refresh
   paths follow this order.

The Rust store already has the profile-lock half. `auth_store::write_pool`
(`auth_store.rs:265`) takes a process-wide `WRITE_LOCK` mutex (`:135`) plus a
per-file `AuthFileLock` flock (`:137`, `LOCK_EX|LOCK_NB` with a 15s spin and
50ms sleeps). That is the profile authority. The seam adds the second, shared
lock and the ordering:

- Introduce a `nous_shared_store` lock keyed to the shared root Nous store path
  (the Rust analogue of `_nous_shared_store_path`), using the same
  `WRITE_LOCK`-plus-flock shape already proven in `auth_store.rs`. Acquire the
  profile lock first, the shared lock second, matching `auth.py:6170`. Never
  acquire them in the opposite order anywhere.
- The refresh network call must happen inside the shared lock but must not be a
  tokio `.await` inside a held `std::sync` guard. The existing recovery path
  already solves this: `rotate_after_failure_async` (`native_agent.rs:598`) runs
  the entire load-mutate-persist on `spawn_blocking`. The Nous refresh HTTP call
  therefore runs on that same blocking worker using a blocking HTTP client
  (`reqwest::blocking` or the ureq-style client the crate already uses for other
  blocking store work), synchronously, while the flock is held. No async lock is
  needed, and no `.await` crosses a lock. This matches Python, where the POST
  runs synchronously inside the flock so that a waiter blocks until the winner
  has persisted the rotated token.
One clarification the contract lane pins: Nous does not use the borrowed-root
pool-fork machinery that `anthropic`/`openai-codex`/`xai-oauth` use (Nous is not
in Python's `SINGLE_USE_REFRESH_POOL_PROVIDERS`, `auth.py:1732`). Its
single-use safety comes entirely from the shared-store flock, the provider-state
transaction, the peer-rotation adopt check, and a short per-process resolve memo.
So the Rust seam does not port the `_fail_closed_unpersisted_rotation` sidecar
for Nous; it ports the flock-plus-adopt path below. The persist-failure
fail-closed rule (end of this section) still applies as the local backstop.

A per-process resolve memo collapses the startup burst. Python memoizes the
resolve result for 5s (`_RESOLVE_TOKEN_CACHE_TTL_S = 5.0`, `auth.py:6714`) so a
fleet of subagents starting together does not each take the flock. The Rust seam
should carry an equivalent short-TTL memo keyed by profile so a burst of
conversations building at once coalesces onto one refresh. This is an
optimization, not a correctness requirement; the flock is the correctness layer.

- Single-use safety is a re-read-under-lock plus adopt. Before POSTing, under
  the shared lock, re-read the store and check whether a peer already rotated:
  the on-disk `access_token` differs from the failed/stale token and is a usable
  invoke JWT (`_already_rotated_by_peer`, `auth.py:7100`; the merge helper
  `_merge_shared_nous_oauth_state`, `auth.py:6194`). If so, adopt it and skip the
  POST entirely (`force_refresh = False`, `auth.py:7268`). Only when no usable
  peer token exists does the winner POST. This is the exact port target for the
  Rust `refresh_nous_entry` hook, and it is what turns N concurrent 401s into one
  POST and N-1 adoptions.
- Persist-before-return, and persist-before-retry. Python persists the rotated
  pair immediately after a successful POST (`_persist_state("post_refresh...")`,
  `auth.py:7371`) so a later validation failure cannot drop a rotated refresh
  token. The Rust seam must write the rotated pair to the store (profile row plus
  the shared store mirror, `_write_shared_nous_state`, `auth.py:7221`) before the
  refreshed credential is returned to the caller for the retry request. The
  existing `PoolLocator` contract already enforces "persist completed, pool
  dropped, then retry `.await`" (`credential_pool.rs:977`), so the refreshed
  credential reaches the retry only after it is durable.

Losers that cannot adopt must not silently succeed. Python's
`_fail_closed_unpersisted_rotation` (`credential_pool.rs` Python side,
`auth.py`) marks the entry terminally when a rotated pair could not be committed.
The Rust seam should treat a persist failure after a successful POST as a
terminal `credential_persist_failed` (already a terminal reason in
`with_failure`, `credential_pool.rs:304`) so a restart cannot replay the spent
token.

## 5. Profile and root authority, store merge rules

Three stores, three roles. The seam must not blur them.

- Profile `auth.json` (`<home>/auth.json`): the write authority for this
  profile's pool rows and its `providers.nous` singleton. `auth_store::write_pool`
  writes only here (`auth_store.rs:265` takes one path). Reads shadow root under
  it: `merge_pool` (`auth_store.rs:92`) returns the profile's provider rows when
  non-empty, else the root rows. This is unchanged.
- Root `auth.json` (`<hermes_root>/auth.json`): read-only fallback for pool rows
  when the profile slice is empty, wired via the `root_auth` argument in
  `build_native_compression_discovery` (`main.rs:540`, gated on the profile being
  a child of `<root>/profiles`). The seam never writes pool rows to root.
- Shared Nous OAuth store (`nous_auth.json` under `${HERMES_SHARED_AUTH_DIR}`,
  default `<hermes-root>/shared/nous_auth.json`, `auth.py:6117`): the seam reads it under
  the shared lock to adopt peer rotations, and mirrors the rotated pair to it
  after a successful refresh (`_write_shared_nous_state`, `auth.py:7221`) so
  sibling profiles do not replay a spent refresh token. This is the one new write
  target, and it is guarded by the shared lock, not the profile lock.

Merge rules that already hold and must be preserved:

- `merge_newer_disk_status` (`auth_store.rs:199`) keeps a newer on-disk cooldown
  and, critically, will not clobber a status when the incoming and disk
  `access_token` differ (`auth_store.rs:230`): a peer that rotated the token owns
  its own status. For Nous this is exactly right; the rotated pair is not a
  status regression to be merged away.
- `write_pool` merges concurrent rows by id and preserves rows this writer did
  not touch (`auth_store.rs:331`). A Nous pool with one `device_code` entry is a
  degenerate case of this, so no new merge logic is needed on the pool side.
- The singleton `providers.nous` state is separate from the `credential_pool`
  rows. The Nous seeder reads `providers.nous`, seeds a `device_code` pool entry,
  and after refresh writes the rotated tokens back to both the singleton and the
  pool row (the Python `_sync_nous_entry_from_auth_store` /
  `_sync_device_code_entry_to_auth_store` pair, `credential_pool.py:1446`/`1518`).
  The Rust seam owes both directions of that sync, under the profile lock.

## 6. Refresh error classification

The classification is small and already half-present in `with_failure`.

- Terminal, requires relogin, promote entry to `dead` and quarantine: portal
  `error` code in `{invalid_grant, invalid_token, refresh_token_reused}` with
  `relogin_required` (`_is_terminal_nous_refresh_error`, `auth.py:6341`), plus
  the reuse-detected text signal (`auth.py:6621`). These already map to
  `TERMINAL_AUTH_REASONS` in `with_failure` (`credential_pool.rs:290`), so the
  seam feeds the parsed `error` code as the failure reason and lets the existing
  code produce the `dead` transition. A terminal refresh also quarantines the
  singleton OAuth material (`_quarantine_nous_oauth_state`, `auth.py:6391`) so it
  is not replayed; the Rust seam clears the dead token material from
  `providers.nous` under the profile lock, keeping routing metadata.
- Transient, retryable, do not quarantine: everything else (429, 5xx, network
  timeout, a malformed non-terminal error body). These map to an `exhausted`
  transition with the standard cooldown ladder from `exhausted_ttl`
  (`credential_pool.rs:610`): 401 without a terminal reason is 300s, billing is
  60s/3600s, else 3600s. For a single-entry Nous pool the sole-key short cooldown
  applies so the one credential can recover.
- Missing refresh token entirely requires relogin (`auth.py:7295`) but does not
  satisfy `_is_terminal_nous_refresh_error`, because its code is not one of
  `invalid_grant`, `invalid_token`, or `refresh_token_reused`. It must not run
  the terminal quarantine path.
- Lock-contention timeout is deliberately not terminal. When a waiter times out
  on the flock, Python leaves the entry untouched rather than benching it
  (`credential_pool.py:2228`); this was the fix for the "pool size 0" mass-401
  incident. The Rust seam must treat a flock-acquire timeout as a transient
  no-op on the store, never a `dead`/`exhausted` transition, or a slow portal
  under contention would quarantine a healthy credential.

The compression-layer classifiers (`compression_auth_failure` for 401,
`compression_payment_failure` for 402, `compression_rate_limit_failure` for 429,
`native_agent.rs:715`) are unchanged. They decide when recovery runs; the Nous
refresh classifier above decides what the refresh outcome does to the store.

## 7. Bounded request behavior

- One discovery budget, unchanged. Recovery runs inside the existing two-attempt
  discovery budget (`discovery_attempts >= 2`, `native_agent.rs:1733`). A Nous
  refresh-and-retry consumes one attempt exactly as an API-key rotation does. The
  golden `maximum_discovery_candidates_io == 2` (`compression_discovery.rs:230`)
  still holds: at most two store loads and two wire attempts per compression.
- One POST per refresh token, enforced by the lock plus the adopt check
  (section 4), not by a counter. Concurrency is bounded by the shared flock: the
  winner POSTs once, waiters block then adopt.
- No inner refresh loop. The seam refreshes once, retries the request once, and
  on a second failure records the outcome and stops (the exact shape
  `recover_summary` already uses at `native_agent.rs:679`: "do not spend a third
  rotated-key request in this compression attempt"). A Nous 401 that survives one
  refresh is either terminal (relogin) or a transient the cooldown will handle on
  the next compression.
- The refresh HTTP call has its own timeout, independent of the compression
  timeout. Python uses a 15s default with a `HERMES_*_REFRESH_TIMEOUT_SECONDS`
  override and sizes the lock timeout to the POST timeout plus 5s
  (`auth.py:1834`/`7237`). The Rust flock timeout for the shared Nous lock must be
  `max(AUTH_LOCK_TIMEOUT default, refresh_timeout + 5s)` so a slow portal cannot
  make a waiter give up before the winner finishes. The existing 15s
  `AuthFileLock` deadline (`auth_store.rs:154`) is too short for this path and
  must be parameterized for the shared Nous lock.

## 8. Client rebuild and eviction

- Client rebuild is the existing one-field swap. After a refresh yields a new
  `access_token`/`agent_key` and possibly a new `inference_base_url`, the
  candidate is rebuilt with `with_runtime_credential` (`native_agent.rs:864`),
  which sets `api_key` and `base_url` and rebuilds the `reqwest::Client`, clearing
  `compression_routes` so a rebuilt route cannot recurse into discovery. Nous
  needs no new rebuild surface; `RuntimeCredential` already carries
  `id`/`api_key`/`base_url` (`credential_pool.rs:1013`) and the Nous
  `runtime_base_url` (`inference_base_url`) flows through it.
- Eviction is store cooldown plus provider `Health`, unchanged. A terminal
  refresh marks the entry `dead` (never re-selected until relogin) and, because a
  dead single-entry pool has no available entry, marks provider `Health`
  unhealthy for 600s (`compression_discovery.rs:13`). A transient marks the entry
  `exhausted` with the cooldown ladder and marks `Health` only when
  `has_available()` is false, so a single Nous credential in cooldown quarantines
  the provider until the earliest of the store cooldown and the 600s `Health`
  TTL. There is no client cache to invalidate; the failed frozen candidate is
  simply not reused.
- The per-conversation `CompressionPoolCredential.current`
  (`native_agent.rs:476`) is updated to the refreshed credential under its mutex,
  so subsequent requests in the same conversation use the rotated token without
  re-reading the store. This is already how rotation updates `current`
  (`native_agent.rs:518`).

## 9. Prompt-cache invariants

- Compression prompt bytes are untouched by credential recovery. The prompt is
  built once (`build_with_memory` path) and the recovery arm only swaps the
  credential and base URL, never the request body. Rotating or refreshing a Nous
  key changes the `Authorization` header and the host, not the prompt, so the
  Anthropic/Nous prompt-cache prefix is byte-identical across the failed attempt
  and the retried attempt. This is the same invariant the API-key seam preserves.
- The refresh POST is a separate request to a different endpoint
  (`/api/oauth/token`) with its own tiny body; it carries no prompt and does not
  interact with the compression prompt cache at all.
- The Nous mtime-keyed auth-status cache (`auth.py:7186` skips writes where only
  derived TTL countdowns changed) is a Python read-path optimization. The Rust
  seam should mirror the "skip persist when only `expires_in`/
  `agent_key_expires_in` changed" rule (`_NOUS_EFFECTIVE_STATE_IGNORED_KEYS`,
  `auth.py:3364`) so that a no-op resolve does not churn `auth.json` mtime and
  invalidate that cache. This is a persistence-suppression invariant, not a
  prompt invariant, but it belongs here because it is the one place a refresh can
  cause a spurious write.

## 10. Ownership table

| State | Owner | Lifetime | Sharing | Mutability |
| --- | --- | --- | --- | --- |
| Frozen candidate `NativeAgentClient` | `CompressionDiscovery.candidates` | conversation build to drop | cloned per conversation | immutable |
| `PoolLocator` (paths, provider, strategy) | `CompressionDiscovery` binding | conversation build to drop | cloned per conversation | immutable, no secret |
| Nous refresh hook (closure over HTTP client + shared-lock path) | request-local, constructed at rotate site | one recovery step | never shared | called on blocking worker |
| `CompressionPoolCredential.current` (last selected `RuntimeCredential`) | per conversation | conversation | `Arc<Mutex>` within one conversation | swapped under mutex |
| Request-local `CredentialPool` (holds live Nous tokens) | `let mut pool` inside `spawn_blocking` | one recovery step | never shared | `&mut`, request-scoped |
| Profile `auth.json` (pool rows + `providers.nous`) | filesystem | durable | profile-path-scoped | written under profile flock |
| Shared root Nous OAuth store | filesystem | durable | cross-profile | written under shared Nous flock |
| `Arc<Health>` (600s TTL map) | process, `main.rs` | process | shared across conversations/profiles | interior `Mutex` |

The only cross-conversation shared mutable state remains `Arc<Health>` (no
secrets). The refresh token and rotated pair live only in the request-local pool
and on disk behind the two flocks.

## 11. Concurrency and race analysis

- No lock across await. The refresh POST and the store read-modify-write run
  synchronously on a `spawn_blocking` worker (section 4), so no tokio `.await`
  crosses either flock or the `std::sync` guards. The retry `.await`
  (`summarize_history_on`) runs only after the pool is dropped and the write
  errors are checked (`credential_pool.rs:977`).
- The single-use reuse race is closed by lock-plus-adopt. Concurrent callers
  serialize on the shared Nous flock; the loser, on acquiring the lock, re-reads
  the store, finds the winner's rotated usable token, and adopts it without
  POSTing (section 4). This is the whole reason the shared lock exists and why the
  profile-then-shared ordering is mandatory: adopting under only the profile lock
  would still let two profiles POST the same shared token.
- Profile isolation holds. Two conversations under different profiles get
  different `profile` paths in their locators and different profile flocks, and
  the shared Nous store is the one intentional cross-profile channel, guarded by
  its own flock. A red-profile refresh cannot corrupt a blue-profile pool row; it
  can only publish a rotated shared token that blue then adopts, which is the
  desired behavior.
- Deadlock freedom. Two flocks, one global ordering (profile first, shared
  second), no path that takes them in the other order. The one Python exception
  (`_try_import_shared_nous_state` holds the shared lock alone, `auth.py:6172`) is
  an import path, not a runtime refresh path, and is out of this checkpoint.
- Persist failure is fail-closed. A successful POST whose rotated pair fails to
  persist marks the entry terminal (`credential_persist_failed`,
  `credential_pool.rs:304`) rather than returning a token a restart would not
  see. This prevents replay of a spent refresh token.
- Shutdown. No background task owns the refresh hook or a pool; both are
  request-scoped. Writes are atomic (`write_private_preserving_symlink`,
  `auth_store.rs:353`), so a torn store is impossible. Nothing to flush or join.

## 12. Deferred scope

State plainly what this checkpoint does not cover so the boundary is honest.

- Interactive login and device-code acquisition stay deferred. This seam
  refreshes an existing Nous grant; it never runs the device-code flow that
  first obtains a refresh token. A profile with no `providers.nous` singleton
  produces no Nous candidate, exactly as an absent env key produces no API-key
  candidate. Relogin (`hermes auth add nous`) is a human action outside the
  gateway.
- The other single-use OAuth providers stay deferred. `anthropic`,
  `openai-codex`, and `xai-oauth` remain in `SEEDING_REQUIRED`
  (`credential_pool.rs:828`). They have their own sync helpers, their own token
  endpoints, and, for `anthropic` `claude_code`, a shared credentials file with a
  distinct lock (`auth.py:1736`). Porting Nous does not port them; each is a
  separate checkpoint. The Nous seam should be written so the shared-lock and
  refresh-hook shapes generalize, but this checkpoint lands Nous only.
- The frozen candidate SET is still frozen; only the KEY refreshes. A Nous
  credential that becomes available mid-conversation (a fresh login in another
  process) does not appear in a live conversation whose candidate list was built
  without Nous; it appears on the next conversation build. What this checkpoint
  restores mid-conversation is a refreshed token within an already-selected Nous
  entry, not a newly-appeared Nous provider.
- Non-`chat_completions` transports stay excluded, matching the built-in
  discovery seam. Nous inference over `chat_completions` is in scope; anything
  else is not.
- The `agent_key_id` / minted-invoke-key path is not needed. Python sets
  `agent_key_id = None` and uses the access token directly as the invoke JWT
  (`auth.py:3342`); there is no separate key-minting HTTP call to port for this
  checkpoint.

## 13. Production integration tests

All extend the existing axum-per-route compression harness (`native_agent.rs`
around the discovery tests near `:4356`) and the end-to-end
`build_native_compression_discovery` path in `main.rs`. Drive profiles through
`secret_scope::with_secret_scope`. Fixtures use synthetic JWTs (a header,
`{"scope":"inference:invoke","exp":<future>}`, a dummy signature) and a fake
portal endpoint; never a real credential.

- Refresh on expiry. A Nous singleton whose `access_token` JWT `exp` is inside
  the 120s skew. Assert the pool treats the runtime key as unusable, the fake
  `/api/oauth/token` is called once with header `x-nous-refresh-token` and body
  `grant_type=refresh_token`, the rotated pair is persisted to the profile row and
  the singleton before the retry, and the inference request carries the new
  bearer.
- Refresh on 401. A usable-looking token that the fake inference endpoint 401s.
  Assert exactly one refresh POST, one retry, and the new token on the retry.
- Single-use reuse safety. Two concurrent recoveries on one profile store, both
  starting from the same stale token. Assert the fake portal receives exactly one
  refresh POST (the winner) and the loser adopts the winner's rotated token from
  disk without POSTing. This is the reuse-storm regression test; it must not flake.
- Peer-rotated adoption across profiles. Profile A refreshes and mirrors the
  rotated pair to the shared store; profile B, starting stale, adopts from the
  shared store under the shared lock and does not POST. Assert one POST total.
- Terminal `refresh_token_reused`. Fake portal returns 200-less error body with
  `error: refresh_token_reused`. Assert the entry goes `dead`, the singleton OAuth
  material is quarantined, provider `Health` is marked unhealthy, and no retry is
  attempted beyond the terminal classification.
- Terminal `invalid_grant` vs transient 429. `invalid_grant` -> `dead` and
  relogin; 429/5xx -> `exhausted` with the sole-key short cooldown and the
  credential eligible again after the cooldown and the 600s `Health` TTL.
- Portal-host allowlist. A poisoned `portal_base_url` outside
  `_NOUS_PORTAL_ALLOWED_HOSTS`. Assert the refresh POSTs to the default portal,
  not the poisoned host, and the refresh token never reaches the poisoned host.
- Persist-before-retry and fail-closed. Inject a store-write failure after a
  successful POST. Assert the entry is marked terminal
  (`credential_persist_failed`) and the rotated token is not returned to the
  retry, so a restart cannot replay it.
- Lock ordering and no-await-across-lock (structural). A test that two concurrent
  recoveries on one store do not deadlock and that the refresh runs on a blocking
  worker (the recovery compiles with the retry `.await` outside any held guard).
- Disk hygiene. After a refresh, assert the owned Nous `device_code` pool row
  carries the rotated `access_token`, `refresh_token`, and `agent_key`, matching
  the explicit non-borrowed exception in `credential_persistence.rs`.
- Prompt byte stability. Same compression prompt across the pre-refresh failed
  attempt and the post-refresh retry; assert the request body bytes are identical
  and only the `Authorization` header and host changed.

## 14. Open items for the contract oracle (agy lane)

- The exact peer-adoption predicate: Python's `_already_rotated_by_peer`
  (`auth.py:7100`) combines "token differs from stale" and "on-disk token is a
  usable invoke JWT." Confirm the precise ordering of the two shared-store merge
  points (`auth.py:7236` for missing access token, `7270` for
  force/expiring) and whether both re-run `_resolve_effective_routing_metadata`.
- Whether a Nous 401 at the compression layer forces `force_refresh=True` (the
  `stale_access_token` path) or relies on the invoke-JWT-expiring check, and how
  that maps to the compression `recover_summary` 429-vs-auth branches.
- The shared Nous store lock file naming (Python locks `nous_auth.lock`,
  from `with_suffix(".lock")` at `auth.py:6179`) and whether `${HERMES_SHARED_AUTH_DIR}` resolution has any
  edge cases the Rust `hermes_root`/profile layout does not already cover.
- The exact resolve-memo key and TTL semantics (`_RESOLVE_TOKEN_CACHE`, 5s,
  `auth.py:6714`): whether it keys on profile alone or on the requested scope,
  since the Rust memo must not serve one profile's token to another.
- The exact singleton write-back conditions (`_sync_device_code_entry_to_auth_store`,
  `credential_pool.py:1518`, and the `set_active=False` rule) so the Rust seam
  does not flip `active_provider` on a refresh side effect.
- Whether `expires_in` / `agent_key_expires_in`-only changes must suppress the
  persist (`_NOUS_EFFECTIVE_STATE_IGNORED_KEYS`, `auth.py:3364`) exactly, to keep
  the auth-status cache warm, and the precise field set that counts as an
  effective change.
