# Rust seam: credential-pool recovery for auxiliary compression discovery

Scope: design the narrowest production seam that connects the already-ported
`credential_pool.rs` selection core and `auth_store.rs` store reads to native
auxiliary-compression discovery and request-time credential recovery. This is a
Rust ownership and concurrency lane. The Python contract and its goldens are
owned by the parallel agy lane
(`rust/analysis/compression-credential-recovery-contract-agy.md`,
`rust/tools/compression-credential-recovery-goldens.json`, not yet landed); this
document does not re-derive that contract. It cites only what pins a Rust
interface and anchors every symbol to code readable in the working tree today.

It builds on the two sibling seam reports that already landed:
`compression-main-fallback-rust-seam-claude.md` (the two configured tiers) and
`compression-builtin-discovery-rust-seam-claude.md` (the built-in discovery
tier). Those established the frozen `CompressionRoutes` plan, the shared
`configured_fallback_attempted` gate, and the process-shared `Health` TTL. This
report covers the one thing neither did: what happens to the *credential* when a
discovery route returns 401/402/429, and how a key that was missing or exhausted
becomes usable again without rebuilding the conversation.

## 0. What exists today, precisely

- Discovery candidates are frozen clients. `build_native_compression_discovery`
  (`main.rs:510`) resolves each provider's key once from `dotenv`/`environment`
  (the `secret` helper at `main.rs:522`) and builds a `NativeAgentClient` whose
  `api_key` field is immutable (`native_agent.rs:556`). The resulting
  `Vec<NativeAgentClient>` is stored in `CompressionDiscovery.candidates`
  (`native_agent.rs:465`). A key is frozen at conversation-build time and never
  re-resolved.
- The only live shared state is `Health` (`compression_discovery.rs:16`), keyed
  by `(profile_home, provider)`, TTL 600s (`UNHEALTHY_TTL`), shared by
  `Arc<Health>` across conversations (`main.rs:1146`). It carries no secrets.
- The traversal already reacts to failures at `native_agent.rs:1626`: on a
  discovery 401 it calls `discovery.mark_unhealthy(provider)`
  (`native_agent.rs:1632`), bounded to two discovery attempts
  (`discovery_attempts >= 2`, `native_agent.rs:1486`). On 402/429 against the
  main route it marks the provider unhealthy (`native_agent.rs:1671`). There is
  no key rotation anywhere; a 401 quarantines the whole provider for 600s.
- The credential pool is fully ported but unused by compression.
  `load_pool_from_store` (`credential_pool.rs:754`), `CredentialPool::select` /
  `peek` / `next_available_at` / `available_entries` (`credential_pool.rs:930`
  onward), per-entry cooldown/exhaustion (`exhausted_ttl`,
  `credential_pool.rs:546`; `exhausted_until`, `:588`), and the `PersistSink`
  (`credential_pool.rs:841`) are all live and golden-tested. The only consumers
  are STT/TTS, which read through `store_pool_callback` (`credential_pool.rs:821`)
  and take a read-only `peek_runtime_key` (`credential_pool.rs:1152`), as wired
  in `transcription_http.rs:161`.

So the gap is exact: compression freezes one key per provider and has no path to
rotate to a second key or to record a per-key exhaustion in the store. The pool
machinery that does both already exists and is proven; it is simply not called
from the compression path.

## 1. The load-bearing constraint: secrets must not enter shared state

`CompressionDiscovery` is `Clone` (`native_agent.rs:462`) and every conversation
client clones it, so anything it holds is process-shared across profiles.
`Health` is safe to share because it holds only timestamps. A live
`CredentialPool` is not: `PooledCredential` retains the raw `access_token` in
memory (`credential_pool.rs:54`), and the pool is per-provider, not per-profile.
If a `CredentialPool` (or an `Arc<Mutex<CredentialPool>>`) were embedded in the
shared discovery surface, conversation A under profile `red` and conversation B
under profile `blue` would share one pool and one set of keys for provider `p`.
That is exactly the cross-profile leak `secret_scope.rs` exists to prevent
(`secret_scope.rs:8`), and it would defeat the fail-closed guard at `main.rs:871`.

`CredentialPool` also cannot be embedded directly even ignoring isolation: it is
not `Clone`, `select` takes `&mut self` (`credential_pool.rs:1135`), and it owns
`Box<dyn Fn>` / `Box<dyn FnMut>` clock, chooser, and persist closures that are
`Send` but not `Sync` (`credential_pool.rs:851`). It does not fit inside a
`Clone` frozen client.

Conclusion that drives the whole design: **the pool must be request-scoped, not
shared.** Load it from the profile store inside the request, mutate it, persist
it, and drop it before the next request. The store on disk (the profile
`auth.json`, read through `auth_store::read_pool`, `auth_store.rs:80`) is the
shared source of truth, and it is already profile-scoped by path. This is the
same shape STT already uses; recovery reuses it rather than inventing shared
mutable credential state.

## 2. Proposed data model

Do not freeze a bare key on a recoverable candidate. Instead give the candidate
a locator that can rebuild the pool per request, plus the frozen client for the
happy path. A locator is cheap, `Clone + Send + Sync`, and holds no secret:

```
#[derive(Clone)]
pub(crate) struct PoolLocator {
    profile_store: std::path::PathBuf,        // <home>/auth.json
    root_store: Option<std::path::PathBuf>,   // root fallback, as read_pool takes
    provider: String,                         // pool key, lowercased
    strategy: String,                         // from pool_strategy(provider, config)
}
```

`CompressionDiscovery` gains one field beside `candidates`:

```
pub(crate) struct CompressionDiscovery {
    profile_home: std::path::PathBuf,
    candidates: Vec<NativeAgentClient>,
    locators: Vec<Option<PoolLocator>>,   // parallel to candidates; None = env-only key
    health: std::sync::Arc<Health>,
}
```

`locators[i]` is `Some` only when `candidate[i]`'s provider resolves through the
store-backed pool path, that is, a plain api-key provider that
`load_pool_from_store` will not reject (`credential_pool.rs:764`: not `anthropic`,
`openai-codex`, `xai-oauth`, `nous`, or `custom:`). For an env-only single key
(the common case for `openrouter`/`custom` in `build_native_compression_discovery`)
the locator is `None` and the candidate behaves exactly as today: one key, no
rotation, provider-level Health on failure.

The locator carries no `Arc<Mutex<CredentialPool>>` and no closures. The pool is
constructed fresh from it per recovery via the existing entry point:

```
let mut pool = credential_pool::load_pool_from_store(
    &loc.profile_store, loc.root_store.as_deref(),
    &loc.provider, &loc.strategy, Some(persist_sink))?;
```

`persist_sink` is a `Box<dyn FnMut(&str, Vec<Value>, Vec<String>) + Send>`
constructed at the call site that writes the profile store atomically (the same
sink shape `credential_sources.rs` and the pool goldens already exercise). It is
created per recovery, not stored, so nothing `Send`-but-not-`Sync` ever lives in
the shared surface.

Why a locator and not a live pool handle: a locator is immutable data that is
safe to clone across profiles because it is just paths and a provider name; two
conversations under different profiles get different `profile_store` paths and
therefore never touch the same store. A live pool handle would be shared secret
state and is rejected in section 8.

## 3. Call flow

Happy path is unchanged. The traversal (`native_agent.rs:1462`) walks the frozen
`candidates` in order, honoring `Health` (`native_agent.rs:1493`), the two-attempt
budget (`native_agent.rs:1486`), and the failed-chain-slot skip
(`native_agent.rs:1503`). A candidate runs through `summarize_history_on`
(`native_agent.rs:1324`) with its frozen key. If it returns a usable summary,
done. No pool is loaded, no store is read. Recovery is strictly a failure path,
so the store cost is paid only when a key actually fails.

Recovery path, entered only in the `Err(error)` arm for a
`BuiltinDiscovery` candidate (`native_agent.rs:1629`) whose `locators[index]` is
`Some`:

1. Classify. `auth_failure = compression_auth_failure(&error)`
   (`native_agent.rs:1627`, HTTP 401); `payment_failure =
   compression_payment_failure(&error)` (`native_agent.rs:1628`, 402 and the
   billing/quota markers). These already exist; do not add a parallel classifier.
2. Load the pool for this provider from the locator (section 2). If load fails
   (I/O or a deferred-seeding provider), fall back to today's behavior: mark
   provider Health unhealthy and continue within budget. `load_pool_from_store`
   returning `Err` is the documented signal that this provider is not a static
   api-key pool (`credential_pool.rs:766`), so this is the OAuth/device-code
   deferral (section 7) surfacing naturally.
3. Record the outcome on the used key. Identify the entry by the key that just
   failed with `entry_id_for_api_key(Some(used_key))` (`credential_pool.rs:959`),
   then apply the store transition the agy contract pins: 401 -> `last_status =
   exhausted` with `last_error_code = 401` (300s TTL via `exhausted_ttl`,
   `credential_pool.rs:547`); 402 -> exhausted with billing reason (3600s); 429
   -> exhausted with the vendor retry delay parsed by `retry_delay`
   (`credential_pool.rs:607`) folded through `normalize_error_context`
   (`credential_pool.rs:644`). The mutation is applied in memory and flushed by
   the persist sink, so the next request in this or any conversation reads the
   cooldown from disk.
4. Select a replacement. Call `pool.select()` (`credential_pool.rs:1135`). It
   runs `available_entries(true)`, which clears expired entries, prunes aged-out
   dead manual entries, applies the strategy side effects, and persists once
   (`credential_pool.rs:1071`). If it returns a new id whose runtime key differs
   from the failed one, rebuild the candidate with that key (section 4) and let
   the traversal retry it, consuming one unit of the existing
   `discovery_attempts` budget. If it returns `None` or the same key, there is
   nothing to rotate to: mark provider Health unhealthy
   (`discovery.mark_unhealthy`, as today at `native_agent.rs:1632`) and continue.
5. Provider-level Health is marked unhealthy only when the pool has no other
   available entry. This is the central correctness change: `pool.has_available()`
   (`credential_pool.rs:913`) false, or `select` yielding the same/last key, is
   the condition for quarantining the whole provider. A single exhausted key must
   not blacklist a provider that still has good keys.

The pool is a local `let mut pool` inside this arm. It is dropped at the end of
the arm, before the next `.await`. No pool ever crosses an await point.

## 4. Rebuilding the client on rotation

`NativeAgentClient` freezes `api_key` and is otherwise immutable. Recovery needs
a client identical to the failed candidate except for the key. The candidate
already carries everything else (model, base_url, headers, profile, identity,
timeout, output cap). Add one narrow, secret-safe method beside the existing
`with_*` builders (`native_agent.rs:736` onward):

```
pub(crate) fn with_rotated_api_key(mut self, api_key: String) -> Self {
    self.api_key = api_key;
    self.compression_routes = Default::default(); // never inherits a plan
    self
}
```

This is the entire "rebuild client" surface. It does not re-resolve base_url,
headers, or profile, because those are provider config, not credential state, and
they did not change. It clears `compression_routes` so a rebuilt route can never
recurse into discovery, preserving the invariant that route clients carry no plan
(`native_agent.rs:568`, and `summarize_history_on` clears it again at
`native_agent.rs:1331`). Rejected alternative: re-running
`build_native_compression_client` per recovery (section 8) would re-resolve the
whole provider surface under a scope that may no longer be installed, and is far
more than a key swap needs.

## 5. Ownership table

| State | Owner | Lifetime | Sharing | Mutability |
| --- | --- | --- | --- | --- |
| Frozen candidate `NativeAgentClient` (incl. `api_key`) | `CompressionDiscovery.candidates` | conversation build to drop | cloned per conversation (shared bytes) | immutable |
| `PoolLocator` (paths, provider, strategy) | `CompressionDiscovery.locators` | conversation build to drop | cloned per conversation | immutable, no secret |
| `Arc<Health>` (TTL map) | process, `main.rs:1146` | process | shared across all conversations/profiles | interior `Mutex` (`compression_discovery.rs:17`) |
| `CredentialPool` (holds live keys) | request-local `let mut pool` | one recovery step | never shared | `&mut`, request-scoped |
| Persist sink | request-local closure | one recovery step | never shared | writes profile store atomically |
| Profile `auth.json` store | filesystem | durable | shared truth, profile-path-scoped | read/written per recovery |
| Rotated `NativeAgentClient` | request-local | one retry | never shared | built by `with_rotated_api_key` |

The only cross-conversation shared mutable state remains `Arc<Health>`, exactly
as today. Credentials live only in the frozen candidate (immutable) and in the
request-local pool (never shared). No new shared secret state is introduced.

## 6. Race and concurrency analysis

- Lock lifetime across await. There is none to worry about, by construction. The
  pool is a stack local; its internal state is not behind an `Arc<Mutex>` that
  outlives the request. The only await in the arm is the HTTP retry through
  `summarize_history_on`, and by then the pool has been dropped (its selection
  and persistence completed synchronously). The failed-key string is copied out
  before the pool is dropped. This is the whole reason to reject a shared
  `Arc<Mutex<CredentialPool>>`: holding it across the retry `.await` would
  serialize every conversation on that provider and risk a poisoned lock taking
  down compression for the process.
- Two conversations recovering the same provider concurrently. Each loads its own
  request-local pool from the same profile store, mutates, and persists. This is
  a read-modify-write race on `auth.json`. It is the identical race the existing
  pool persistence already tolerates: `load_pool_from_store` re-reads current
  disk state, and the persist sink must write atomically (rename over temp, as
  `atomic_file.rs` provides). Last writer wins on the row set; the cost is a
  possibly-lost `request_count` bump under `least_used`, which is a load-spread
  hint, not correctness. An exhaustion mark is idempotent (both writers set the
  same status), so a 401/402/429 record cannot be lost by interleaving. No new
  lock is needed; do not add a global pool file lock for a hint field.
- Selection side effects. `select` under `round_robin` renumbers priorities and
  persists (`credential_pool.rs:1106`), and under `least_used` bumps
  `request_count` and persists via `available_entries` clearing
  (`credential_pool.rs:1086`). These writes happen inside recovery, which is a
  failure path, so the extra store write is rare. The design must not call
  `select` on the happy path, which would turn every compression into a store
  write. Recovery calls `select` only after a failure.
- Health vs pool double cooldown. `Health` (600s, in-memory, provider-level) and
  pool exhaustion (300/3600/60s, persisted, per-key) are two layers with
  different granularity. They must not both fire when a rotation target exists
  (section 3 step 5), or one bad key would quarantine a good provider for 600s
  across all conversations. Rule: per-key exhaustion always records; provider
  Health marks unhealthy only when `has_available()` is false after recording.
- Cache ownership and eviction. There is no client cache. "Eviction" on 401 is
  just not reusing the failed frozen candidate and building a fresh one with
  `with_rotated_api_key`. Provider-level eviction across conversations is the
  existing `Health` TTL. Nothing to invalidate, nothing to reference-count.
- Shutdown. No background task owns a pool or a persist sink; both are
  request-scoped. A persist mid-write is safe because the sink renames
  atomically, so a torn `auth.json` is impossible. There is no flush/join to
  sequence at shutdown. `Arc<Health>` drops when the last conversation client
  drops, carrying no durable state, so it needs no shutdown handling.
- Bounded retries. Recovery must live inside the existing `discovery_attempts >=
  2` budget (`native_agent.rs:1486`), not add an inner rotation loop. One
  rotation consumes one attempt. This caps store I/O per compression at two pool
  loads, matching the goldens' `maximum_discovery_candidates_io: 2` already
  asserted in `compression_discovery.rs` tests (`compression_discovery.rs:230`).
  Do not loop `select` until the pool is exhausted within a single compression.

## 7. Checkpoint boundary: what recovery can and cannot do natively

- Static api-key rotation is in scope. Providers whose pool is manual/env
  api-key entries (`openai-api`, `gmi`, and the rest of
  `API_KEY_PROVIDER_ORDER`, `compression_discovery.rs:76`) rotate through
  `load_pool_from_store` + `select`, which is exactly the path
  `load_pool_from_store` supports.
- OAuth and device-code refresh is out of scope. `load_pool_from_store` bails for
  `anthropic`, `openai-codex`, `xai-oauth`, `nous`, and `custom:`
  (`credential_pool.rs:764`) precisely because seeding/refresh is unported
  (`credential_pool.rs:1` and `auth_store.rs:1` both say so). For those providers
  the locator is `None`, recovery does not run, and a 401 marks provider Health
  unhealthy as today. This is the honest strict-subset boundary: native recovery
  covers key rotation, not token refresh, until credential lease/refresh lands.
  `nous` is already excluded from discovery for the same reason
  (`main.rs:555`).
- The candidate SET is frozen; only the KEY recovers. A provider whose env key
  was absent at conversation-build time never becomes a discovery candidate
  mid-conversation, because `build_native_compression_discovery` runs once at
  build (`main.rs:1131`). What recovery restores is a *key within an already-known
  pool*: a manual key added to the store, or an exhausted key whose cooldown
  elapsed, becomes usable on the next compression because the pool is re-read from
  disk. A provider that had no candidate at all requires a new conversation build
  to appear. Making the whole candidate list lazy per request is rejected in
  section 8. The task's "missing credentials become available to an existing
  conversation" is satisfied for the pool-key case and explicitly not for the
  whole-new-provider case; that distinction is the single most important thing to
  get right and to state plainly.

## 8. Recommendations to reject as unsafe or speculative

- Reject a shared `Arc<Mutex<CredentialPool>>` in `CompressionDiscovery`. It puts
  live secrets in state cloned across profiles (section 1 leak), holds a lock
  across the retry await (section 6 serialization/poison), and embeds
  `Send`-not-`Sync` closures in a `Clone` client. The request-local pool avoids
  all three.
- Reject caching resolved keys or built clients keyed by provider on the shared
  surface. Same cross-profile leak. If any cache is ever added it must key on
  `(profile_store_path, provider)` and never outlive a scope, but for this
  checkpoint no cache is needed at all.
- Reject calling `pool.select()` on the happy path to "keep the pool warm." It
  turns every compression into a store read-modify-write and a possible
  round-robin renumber. Selection is a failure-path action.
- Reject re-running `build_native_compression_discovery` or
  `build_native_compression_client` per request to pick up newly-available
  providers. It re-resolves the entire provider surface under a secret scope that
  may no longer be installed (`with_rotated_api_key` needs no scope; a full
  rebuild does), and contradicts the frozen-plan model the sibling reports
  established. New-provider availability is a new-conversation concern.
- Reject an unbounded inner rotation loop. Recovery must consume the existing
  two-attempt discovery budget, not spin through every key in one compression.
- Reject marking provider Health unhealthy on every 401. Do it only when the pool
  has no remaining available entry; otherwise rotation is defeated.
- Treat as speculative until the agy contract lands: the exact 429 store TTL
  source (vendor `Retry-After` vs `retry_delay` regex vs the 60/300/3600 ladder),
  and whether a 402 on a pooled key marks that one key or the whole provider.
  `exhausted_ttl` (`credential_pool.rs:546`) already encodes a specific ladder;
  bind it to the oracle rather than guessing.

## 9. Minimal test matrix

All extend the existing axum-per-route compression harness
(`native_agent.rs` around `native_agent.rs:3578`) and the end-to-end
`build_agent_client_for_home` discovery test (`main.rs` around the
`discovery_requests` fixture, `main.rs:2135`). Drive scopes through
`secret_scope::with_secret_scope` as the multiplex tests already do
(`main.rs:2510`).

- Rotate on 401. A pooled provider with two manual keys in the profile store.
  First key returns 401; assert the store marks that entry `exhausted` with code
  401, `select` yields the second key, the rebuilt candidate is retried once, and
  the second key's bearer reaches the wire. Assert provider Health is NOT marked
  unhealthy because a rotation target existed.
- No rotation target. Pooled provider with one key that returns 401. Assert the
  entry is exhausted in the store AND provider Health is marked unhealthy, and no
  retry beyond budget. Advance the injected clock past the 300s store TTL and the
  600s Health TTL and assert the key becomes eligible again.
- 402 billing. Pooled key returns 402 with a billing marker; assert the store
  records the billing exhaustion (3600s ladder) and, per the agy contract,
  either the key or the provider is quarantined. Pin against the oracle.
- 429 retry delay. Key returns 429 with a parseable retry hint; assert the store
  cooldown equals `retry_delay` folded through `normalize_error_context`, not the
  flat 3600s.
- Store recovery across conversations. Conversation 1 exhausts key A. Add key B
  to the profile store out of band. Conversation 2 (fresh build) and, for a
  key-within-known-pool case, a later compression in a still-live conversation
  reads the pool from disk and uses B. Assert the frozen-candidate-set limitation
  from section 7: a wholly new provider added out of band does not appear in the
  live conversation.
- Profile isolation under rotation. Two conversations under `red` and `blue`
  scopes, each with a different two-key pool for the same provider, both hit 401
  and rotate. Assert each rotates within its own profile store path and neither
  sees the other's key. This is the leak test for the request-local design.
- Lock/await safety (structural). A test that a recovery step drops the pool
  before the retry await (no `Mutex` guard is `Send`-held across `.await`);
  enforced by the pool being a local, verified by the code compiling with the
  retry inside the same arm and by the isolation test not deadlocking under two
  concurrent recoveries on one store.
- Budget. Several pooled keys all returning 401; assert at most two store loads
  and two wire attempts per compression, matching `maximum_discovery_candidates_io`.
- Deferred providers fail closed. A candidate whose provider is `anthropic` /
  `openai-codex` / `nous` (locator `None`): assert recovery does not run, the
  store is untouched, and behavior matches today (Health unhealthy on 401).

## 10. Open items for the contract oracle (agy lane)

- The exact per-key store transition for 401 vs 402 vs 429 in the auxiliary
  compression path: which HTTP status maps to which `last_status` / cooldown, and
  whether a 402 marks one key or the whole provider.
- Whether Python rotates within a provider's pool during a single compression at
  all, or only records the failure and moves to the next provider. Section 3
  assumes one rotation inside the two-attempt budget; if Python does not rotate
  mid-compression, the seam shrinks to "record exhaustion, then move on" and
  `with_rotated_api_key` is unneeded for checkpoint 1.
- Whether provider-level health (the 600s in-memory layer) and per-key store
  exhaustion are both meant to fire, and the precise condition for quarantining a
  provider that still has a good key. Section 3 step 5 proposes "Health only when
  no available entry"; confirm against source.
- Selection strategy source for compression pools:
  `pool_strategy(provider, config)` (`credential_pool.rs:718`) reads
  `credential_pool_strategies[provider]`; confirm the compression path uses the
  same config key and default (`fill_first`).
</content>
</invoke>
