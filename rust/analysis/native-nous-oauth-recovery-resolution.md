# Native Nous OAuth compression recovery resolution

## Outcome

The canonical `providers.nous` device-code grant is now a live native
compression discovery candidate. It occupies Python's reserved slot after
OpenRouter and before custom or registered static providers. Credential
resolution remains lazy, so conversation construction freezes the route and
prompt without performing OAuth network I/O.

The request path validates the stored NAS invoke JWT with Python's scope union
and strict 120-second minimum lifetime. It prefers `agent_key`, then
`access_token`, maintains the compatible derived key metadata, uses the trusted
operator inference override only at runtime, and persists the effective Portal
override just as the direct Python resolver does.

## Single-use refresh ownership

`nous_credentials::Locator` owns no secret. Each resolution runs on one
blocking worker and holds the complete transaction from disk re-read through
refresh and durable publication:

1. Lock the active profile `auth.json`.
2. If the provider is borrowed, lock and re-read the root `auth.json` without
   creating a shadow provider in the profile.
3. If the local token needs refresh, lock `shared/nous_auth.lock`. The common
   valid-token path never takes this cross-profile lock.
4. Merge a peer's rotated OAuth state and adopt its different usable access
   token before considering another POST.
5. If still required, POST the single-use refresh token to the validated Portal
   route with redirects disabled.
6. Atomically persist the rotated singleton and owned device-code pool row
   before the credential can reach an inference retry.
7. Mirror the minimal OAuth pair to `shared/nous_auth.json` on a best-effort
   basis, omitting runtime `agent_key` fields.

The lock wait is `max(15 seconds, refresh timeout + 5 seconds)`. No async await
point exists inside the transaction, and dropping the caller future cannot
cancel the blocking worker after the Portal has consumed the token.

Two sibling profiles racing the same refresh grant produce one Portal POST.
The winner publishes the rotated pair, and the waiter adopts it from the shared
store. The adoption check reads `access_token` directly before local
`agent_key` precedence, preventing an older but still decodable key from
causing a second rotation.

## Failure and security behavior

Stored Portal routes accept HTTPS only on `portal.nousresearch.com`, plus HTTP
loopback for local tests. Network-provided inference routes accept HTTPS only
on `inference-api.nousresearch.com`. Trusted operator overrides remain an
explicit escape hatch. A refresh response cannot redirect the inference bearer
to another host, and refresh redirects are disabled.

`invalid_grant`, `invalid_token`, and `refresh_token_reused` quarantine the
singleton, remove canonical and legacy device-code pool rows, retain unrelated
manual credentials, and delete the shared copy. A
partial shared record without both tokens is ignored. Shared mirror replacement
detaches a symlink to match Python. Missing refresh material asks
for reauthentication without entering that terminal quarantine path. A
successful refresh whose auth-store commit fails returns no runtime credential,
so an unpersisted rotated token cannot escape to inference. Untrusted Portal
error descriptions are not logged.

The refreshed `reqwest::Client` is rebuilt before retry. A failed inference
retry does not consume a second single-use refresh token. Static API-key pools
retain their existing behavior of recording the replacement failure without a
third provider request.

## Prompt and request invariants

The summary prompt is built once. The failed Nous inference attempt and its
single retry reuse identical JSON bytes and remain tool-free. OAuth affects
only the authorization and validated endpoint. Nous attribution uses the
lineage-root conversation ID and the product version parsed at compile time
from `hermes_cli.__version__`, rather than the placeholder Rust workspace
package version.

## Verification

The production tests exercise:

- native auto-discovery before a configured static GMI provider;
- profile-to-root write-through without profile pollution;
- peer adoption when a stale local `agent_key` remains;
- one refresh POST across two concurrently resolving sibling profiles;
- terminal singleton, pool, and shared-store quarantine;
- Portal persistence versus inference runtime-only override behavior;
- same-token cooldown preservation during metadata healing;
- a real post-refresh filesystem commit failure that fails closed;
- persistence before inference retry, fresh authorization, identical request
  bytes, tags, and absence of tools;
- exactly one refresh when the retried inference request also returns 401.

AGY owned the source-executed Python contract lane. Its initial 86-case corpus
contained broken shared-store filenames, a wrong fixed singleton ID and label,
and several false booleans. The primary audit repaired those checks and added a
Portal override persistence case. The final corpus has 87 cases across 12
sections and regenerates byte for byte.

Claude owned the separate Rust ownership and post-implementation security lane.
The primary audit corrected its borrowed-secret, Portal persistence, inference
allowlist, header, missing-refresh, and lock-file claims. Its cancellation
warning directly shaped the final blocking transaction. The primary lane fixed
four verified review findings: valid-token shared-lock contention, partial
shared adoption, mirror symlink handling, and the missing end-to-end
persisted-route assertion. It rejected the claimed reuse-description quarantine
gap because Python's terminal helper still requires one of the three terminal
error codes.

## Honest boundary

This checkpoint covers the standard singleton-backed Nous device-code grant on
the native chat-completions compression path. Pool-only independent Nous rows,
interactive device-code login, Portal free-tier and rate-guard state, dynamic
recommended-model selection, stale-model 404 refresh, main-agent Nous recovery,
and the other OAuth providers remain separate work. A newly added Nous grant
also cannot change an already frozen conversation's provider set; it appears on
the next conversation initialization.
