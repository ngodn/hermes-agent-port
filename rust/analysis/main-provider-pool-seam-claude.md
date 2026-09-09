# Rust seam: credential-pool selection and request-time recovery for the main provider

Scope: design the narrowest production seam that gives the native main-provider
chat-completions path what the auxiliary compression path already has, that is,
store-backed credential-pool selection at build and per-key recovery on a request
failure, for the static API-key subset. This is a Rust ownership and concurrency
lane. The Python behavior contract and its goldens are owned by the parallel agy
lane (`rust/analysis/main-provider-pool-contract-agy.md`,
`rust/tools/gen_main_provider_pool_goldens.py`,
`rust/tools/main-provider-pool-goldens.json`, not yet landed); this document does
not re-derive that contract. It cites only what pins a Rust interface and anchors
every symbol to code readable in the working tree at commit 1b3b4173c3.

It builds directly on the compression recovery seam that already landed as code:
`CompressionPoolCredential`, `PoolLocator`, and `RuntimeCredential`
(`native_agent.rs:474`, `credential_pool.rs:913`, `credential_pool.rs:1014`), and
on the analysis in `compression-credential-recovery-rust-seam-claude.md`. Where
that report proposed a design, the code now exists and I cite the code. This
report covers the one path that machinery was never wired into: the main turn,
both the streaming completion and the tool-calling loop.

## 0. What exists today, precisely

- The main client freezes exactly one key at build. In
  `build_agent_client_for_home_with_discovery` the main key resolves once to a
  single `Option<String>` from `config.llm_api_key`, then
  `config_file::resolve_profile_api_key` or
  `resolve_provider_api_key_with_env` (`main.rs:1040`). That string is passed
  straight into `NativeAgentClient::new(model, &key, base_url)` (`main.rs:1065`).
  `api_key` is a plain immutable `String` field (`native_agent.rs:883`). No
  `PoolLocator` is ever constructed for the main provider, at build or anywhere
  else. When the main provider appears as a compression discovery candidate it is
  pushed with `main_key` and a `None` binding (`main.rs:660`), so even the
  compression lane does not pool the main key.
- The pool machinery is fully live and used by compression only.
  `PoolLocator::new` / `select_runtime` / `mark_exhausted_and_rotate`
  (`credential_pool.rs:920`, `:977`, `:985`) reload the store per call and persist
  through `auth_store::write_pool` before returning (`credential_pool.rs:947`,
  the `check_write` after `drop(pool)` at `:1001`). `RuntimeCredential` carries
  `id`, `api_key`, `base_url` and deliberately has no `Debug`
  (`credential_pool.rs:1011`). `CompressionPoolCredential` wraps a
  `CompressionCredentialSource::{ApiKey(PoolLocator), Nous(...)}`, an
  `Arc<Mutex<Option<RuntimeCredential>>>` cursor, and a `fallback_base_url`
  (`native_agent.rs:474`). Its `rotate_after_failure` reads the dispatched
  credential, classifies the error, and calls `mark_exhausted_and_rotate` with the
  key hint and id (`native_agent.rs:517`).
- The main request path uses `self` directly and has no recovery. Two dispatch
  sites read the frozen credential fields:
  - streaming completion in `run_model_turn` (tools empty),
    `self.client.post(...).bearer_auth(&self.api_key).headers(self.provider_headers.clone())`
    (`native_agent.rs:2720`), status checked at `:2730`, then `forward_sse` at
    `:2739`;
  - tool round in `ChatModel::step`, same post shape (`native_agent.rs:3260`),
    status checked at `:3269`, then JSON decode.
  Both turn a non-success status into `Error::Other("native agent HTTP {status}:
  ...")` (`native_agent.rs:2733`, `:3272`) and return. No rotation, no store
  write, no retry. A 401 on the main key fails the whole turn.
- The turn client is a per-turn clone of a per-conversation cached client.
  `run_native_turn` does `let mut turn_client = self.clone()` (`native_agent.rs:2751`),
  sets `cache_scope`, then calls `run_model_turn`. The conversation client itself
  is cached by `ConversationAgent` under `CacheKey = (home, session_id)`
  (`conversation_agent.rs:63`, key built at `:120`), reused across turns until
  idle/pressure eviction. Turns within one session are serialized by the turn
  lease (`turn_lease.rs:1`, "serializes the load history -> run -> flush region").

So the gap is exact and symmetric to the compression gap that was already closed:
the main provider selects no pool at build and cannot rotate a key on 401/402/429.
Everything needed to close it, `PoolLocator` plus a shared cursor plus the
`with_runtime_credential` rebuild, already exists and is golden-tested for
compression. The work is wiring, not new mechanism, plus one genuinely new
concern the compression path never had: the failure point is inside a streaming
response and inside a multi-round tool loop, not a single one-shot summary call.

## 1. The load-bearing constraints

Restating the task's invariants as concrete properties this seam must hold, each
tied to where it is enforced.

1. One seam for both transports. Streaming (`run_model_turn`, `native_agent.rs:2714`)
   and the tool loop (`ChatModel::step`, `native_agent.rs:3238`) must recover
   through the same code. They already share the post-and-status-check shape;
   the seam factors exactly that shape out.
2. Per-entry endpoint change without leaking old headers or TLS. A rotated entry
   may carry its own `base_url` (`RuntimeCredential::base_url`,
   `credential_pool.rs:1041`). Swapping it must not carry provider headers or a
   TLS root set that belonged to a different endpoint. Enforced at the client
   rebuild (section 4).
3. Reload durable pool state before every mutation. `PoolLocator::load`
   re-reads the store on each call (`credential_pool.rs:955`), so a rotation always
   sees cooldowns other conversations or processes wrote. Reuse the locator; never
   cache a live `CredentialPool`.
4. Persist failure before retry. `mark_exhausted_and_rotate` persists through the
   sink and `PoolLocator` calls `check_write` before handing back the replacement
   (`credential_pool.rs:993`). The retry request cannot be issued until the failed
   key's exhaustion is on disk.
5. Bound request attempts. One rotation per dispatch, inside a fixed per-request
   budget, never an inner loop that drains the pool in a single request.
6. Exact request, prompt, and tool bytes stay stable across a retry. The retried
   request must be byte-identical: same `messages`, same `tools`, same assembled
   body. Enforced by retrying at the dispatch boundary with the same inputs
   (section 3), not by rebuilding the prompt.
7. No raw credential crosses a profile or conversation boundary. The cursor lives
   in a per-conversation `Arc<Mutex<...>>`; the locator holds only paths. Two
   conversations never share a cursor and are isolated on disk by
   `profile` path, exactly as compression already is (`secret_scope.rs:8` is the
   invariant, the request-local pool is how it is kept).

## 2. Proposed data model

Reuse the landed types. Do not invent a parallel credential holder. The only
question is whether the main client stores a `CompressionPoolCredential` as-is or
a lifted neutral type; section 8 recommends lifting the ApiKey arm. For the
signatures below I name the lifted type `MainPoolCredential` but note it is the
same shape as today's `CompressionPoolCredential` with the Nous arm removed for
this checkpoint.

`NativeAgentClient` gains one field, shared across clones so a rotation in one
tool round or turn is visible to the next:

```rust
pub struct NativeAgentClient {
    // ... existing fields ...
    /// Store-backed credential cursor for the main provider. `None` keeps the
    /// frozen `api_key`/`base_url` behavior of an env-only key unchanged.
    main_credential: Option<MainPoolCredential>,
}
```

`MainPoolCredential` (lifted from `CompressionPoolCredential`, ApiKey-only):

```rust
#[derive(Clone)]
pub(crate) struct MainPoolCredential {
    locator: crate::credential_pool::PoolLocator,
    current: std::sync::Arc<std::sync::Mutex<Option<crate::credential_pool::RuntimeCredential>>>,
    fallback_base_url: String,
}

impl MainPoolCredential {
    pub(crate) fn new(
        locator: PoolLocator,
        current: RuntimeCredential,
        fallback_base_url: impl Into<String>,
    ) -> Self { /* Arc::new(Mutex::new(Some(current))) */ }

    fn current(&self) -> Option<RuntimeCredential>;          // clone under the lock
    fn can_recover(error: &Error) -> bool;                   // 401 || 402 || 429
    fn rotate_after_failure(&self, error: &Error)
        -> anyhow::Result<Option<RuntimeCredential>>;        // sync; call under spawn_blocking
}
```

The `current` `Arc<Mutex<Option<RuntimeCredential>>>` is the single point of
credential identity. `NativeAgentClient::clone` clones the `Arc`, so the per-turn
`turn_client` clone (`native_agent.rs:2751`) and the per-conversation cached
client share one cursor. A rotation writes the cursor once; every later round and
every later turn on that conversation reads the new key. This is exactly the
propagation `CompressionPoolCredential.current` already relies on
(`native_agent.rs:476`), which is why lifting it rather than reimplementing it is
correct.

Build site. In `build_agent_client_for_home_with_discovery`, replace the single
`key` resolution (`main.rs:1040`) with a pool-first resolution that mirrors the
compression lane's `pool_runtime` closure (`main.rs:543`):

```rust
let locator = credential_pool::PoolLocator::new(
    home.join("auth.json"),
    root_auth_for(home),                          // Some only under root/profiles
    provider_identity,                            // the resolved main provider id
    credential_pool::pool_strategy(provider_identity, user_config),
);
let (api_key, main_credential) = match locator.select_runtime() {
    Ok(Some(runtime)) => (
        runtime.api_key().to_owned(),
        Some(MainPoolCredential::new(locator, runtime, base_url.clone())),
    ),
    _ => (frozen_env_key, None),                  // today's behavior, byte-for-byte
};
```

`select_runtime` returns `Ok(None)` for an empty or fully exhausted pool and
`Err` for a seeding-required provider (`load_pool_from_store` bails on `anthropic`,
`openai-codex`, `xai-oauth`, `nous`, `custom:`, `credential_pool.rs:828`). In both
non-`Some` cases the client falls back to the frozen env key with
`main_credential = None`, so a provider without a store-backed pool behaves
exactly as it does today. This is the strict-subset boundary made structural:
recovery exists only where a pool exists.

## 3. The dispatch seam

The narrowest deep seam is a single private method both transports call for the
post-and-status-check region. Name it `dispatch_chat`. It owns credential
resolution, the recoverable-status classification, one bounded rotation, and the
retry. It returns a `reqwest::Response` whose status is already success, so the
caller streams or decodes without knowing recovery happened.

```rust
impl NativeAgentClient {
    /// Post an already-built chat-completions body, recovering a pooled key once
    /// on a classified auth/billing/rate failure. Returns a success-status
    /// response; the caller owns the body (stream or decode). The body is passed
    /// by reference and re-sent byte-identical on the retry.
    async fn dispatch_chat(&self, url_path: &str, body: &Value) -> Result<reqwest::Response>;
}
```

Flow inside `dispatch_chat`:

1. Resolve the effective credential. If `main_credential` is `Some`, read
   `current()`; its `api_key` and (`base_url` or `fallback_base_url`) drive this
   request. If `None`, use the frozen `self.api_key` / `self.base_url` /
   `self.client`. Build the outgoing request with the resolved bearer, the
   resolved URL, `self.provider_headers.clone()` (provider config, unchanged by a
   key swap), and a client valid for the resolved endpoint (section 4).
2. Send. On transport error, return it (not a credential failure).
3. Check status. On success, return the response. This is the only path the
   happy case takes, and it reads the store zero times.
4. On a non-success status, classify with the existing predicates already used by
   compression: `compression_auth_failure` (HTTP 401, `native_agent.rs:828`),
   `compression_payment_failure` (402 and billing markers, `:842`),
   `compression_rate_limit_failure` (429 non-payment, `:837`). If the status is
   not recoverable, or `main_credential` is `None`, return the
   `Error::Other("native agent HTTP {status}: ...")` exactly as today.
5. Recover once, inside the budget. If this dispatch has not yet spent its single
   rotation, call `rotate_after_failure` on a blocking worker (it is synchronous
   file I/O plus an flock; run it under `tokio::task::spawn_blocking`, the same
   discipline `rotate_after_failure_async` uses at `native_agent.rs:678`). It
   marks the dispatched key exhausted with the classified status and reason,
   persists, and returns the next `RuntimeCredential` or `None`. Update the shared
   cursor to the returned value.
6. If a replacement was returned, rebuild the request against it (step 1 with the
   new credential) and send once more. Return that response regardless of its
   status; the second status is surfaced to the caller as the final outcome. Do
   not rotate a second time in one dispatch.
7. If `None` was returned (pool drained), return the original classified error.

Where the two callers change:

- `run_model_turn` streaming completion (`native_agent.rs:2714`): build `body`
  as today, then `let resp = self.dispatch_chat("/chat/completions", &body).await?;`
  replacing the inline `self.client.post(...).send()` and the status check. The
  existing `forward_sse(resp.bytes_stream(), ...)` at `:2739` is unchanged.
- `ChatModel::step` (`native_agent.rs:3238`): build `body` as today, then
  `let resp = self.dispatch_chat("/chat/completions", &body).await?;` replacing the
  post and status check at `:3260`. Decode and name-repair are unchanged.

Byte stability. `dispatch_chat` takes `body` by reference and re-sends the same
serialized bytes on the retry. `body` is built once by the caller from
`build_request_body_from_messages` plus `apply_provider_extras`
(`native_agent.rs:2715`, `:3241`), so the prompt, tools, and provider extras are
identical on both attempts. The only wire difference between attempt one and two
is the bearer and possibly the endpoint, which is the whole point.

## 4. Rebuilding the client on rotation, without leaking headers or TLS

The landed `with_runtime_credential` (`native_agent.rs:976`) swaps `api_key` and
`base_url`, rebuilds `self.client` with a bare `reqwest::Client::builder().build()`,
and clears `compression_routes`. Two properties matter for the main path:

- Provider headers are preserved, and that is correct. `provider_headers`
  (`native_agent.rs:886`) is provider configuration (for example a version header
  or referer), not credential state, and a rotated key is the same provider. The
  seam must keep sending `self.provider_headers` after a rotation. The pitfall is
  the inverse: a rotation must never be used to cross to a different provider
  identity. A main-provider pool holds alternate keys for one provider; a
  per-entry `base_url` is a regional or mirror endpoint of that same provider.
  The contract lane must confirm that assumption; if a pooled entry could name a
  foreign host, the headers would leak and the design must gate the base_url swap
  on same-host. State this as an explicit open item (section 9).
- TLS is currently uncustomized on the main request client, so nothing leaks
  today, but the rebuild is a latent trap. `NativeAgentClient::new` builds a bare
  client (`native_agent.rs:938`) and so does `with_runtime_credential`
  (`native_agent.rs:987`); neither installs a CA bundle. Provider CA handling
  lives only in `provider_registry::profile_http_client` (`provider_registry.rs:346`,
  `:426`), which is used for model-list fetches, not for turn requests. So a key
  swap does not drop any TLS config that turns rely on right now. The trap: if a
  later change gives the main turn client a provider CA or a custom TLS builder,
  the bare rebuild in `with_runtime_credential` will silently discard it on every
  rotation. The seam should route the rebuild through one shared client-builder
  helper (the same one `new` uses), so TLS policy has a single source and a
  rotation can never diverge from a fresh build. Flag it now; do not add TLS to
  the main client in this checkpoint.

Because `dispatch_chat` resolves the effective client per request, the cleanest
implementation does not mutate `self` at all: it derives a request client for the
resolved endpoint locally and posts through it. That sidesteps the fact that
`turn_client` is borrowed immutably through the whole tool loop (`&self` in
`step`, and `TranscriptModel { inner: self }` at `native_agent.rs:2691`) and so
cannot have its `api_key` field reassigned mid-loop. The credential lives in the
shared `Arc<Mutex>` cursor; the frozen `self.api_key`/`self.client` fields are
only the `None`-binding fallback. This is the key ownership decision: recovery
state is the cursor, not the struct fields.

## 5. Ownership table

| State | Owner | Lifetime | Sharing | Mutability |
| --- | --- | --- | --- | --- |
| Frozen `api_key` / `base_url` / `client` fields | `NativeAgentClient` | conversation build to eviction | cloned per turn (shared bytes) | immutable; used only when `main_credential` is `None` |
| `main_credential` locator (paths, provider, strategy) | `NativeAgentClient` | conversation build to eviction | cloned per turn | immutable, no secret |
| `main_credential.current` cursor | `Arc<Mutex<Option<RuntimeCredential>>>` | conversation build to eviction | shared across all clones of one conversation client | interior mutability; written by rotation, read per request |
| `CredentialPool` (holds live keys) | request-local inside `PoolLocator::load` | one selection or rotation | never shared | `&mut`, request-scoped |
| Persist sink | request-local closure in `PoolLocator::load` | one rotation | never shared | writes profile store atomically |
| Profile `auth.json` | filesystem | durable | shared truth, profile-path scoped | read/written per rotation |
| Per-request outgoing client | `dispatch_chat` local | one HTTP attempt | never shared | built for the resolved endpoint |

The only cross-turn shared mutable state is the per-conversation cursor, and it
is per conversation, never cross-profile. No live `CredentialPool` and no raw key
ever sits in a field cloned across conversations. This matches the compression
ownership model exactly (`compression-credential-recovery-rust-seam-claude.md`
section 5), which is the point of reusing it.

## 6. Race, cancellation, and concurrency analysis

- Concurrent turns in one conversation. The turn lease serializes the
  load/run/flush region per session (`turn_lease.rs:1`), so two turns of one
  conversation never run their dispatches concurrently. The cursor is therefore
  written by at most one turn at a time. The `Mutex` around it is for the
  cross-clone share and for lock-poison safety, not for turn concurrency.
- Concurrent turns across conversations sharing a provider. Two sessions under the
  same profile hold two different cached clients and two different cursors, but
  one on-disk pool for the provider. Each rotation loads its own request-local
  pool, mutates, and writes atomically through `auth_store::write_pool`. This is
  the identical read-modify-write race compression already tolerates: last writer
  wins on the row set, an exhaustion mark is idempotent (both writers set the same
  status), and the only lossy field is a `least_used` counter bump, which is a
  load-spread hint, not correctness. No global pool file lock is needed and none
  should be added (`compression-credential-recovery-rust-seam-claude.md` section
  6 reached the same conclusion against the same store).
- No lock is held across an await. `rotate_after_failure` is synchronous; it runs
  under `spawn_blocking`, and the `Mutex` guard on `current` is taken only to read
  or write the cursor, never across the HTTP `.await`. The request-local pool is
  dropped inside the blocking worker before its result crosses back. This is the
  same discipline as `rotate_after_failure_async` (`native_agent.rs:664`).
- Cancellation. A turn future can be dropped at any await (client disconnect,
  shutdown). Three points to reason about:
  1. Dropped during the first HTTP send, before any status: no store write has
     happened, nothing to undo. The cursor is unchanged.
  2. Dropped after the rotation persisted but before the retry send: the store
     correctly records the first key exhausted, and the cursor points at the
     replacement. The next turn on this conversation, or the next conversation,
     reads that state and proceeds. Persist-before-retry means a cancellation
     between the two leaves durable state consistent, never a key that failed but
     was not recorded.
  3. Dropped mid-stream in `run_model_turn` after a 200: this is not a credential
     failure and dispatch already returned; no rotation is in flight. The partial
     stream is simply abandoned, exactly as today.
  There is no background task owning a pool or a sink, so there is nothing to join
  at shutdown, and a persist mid-write is safe because `write_pool` renames
  atomically. This matches the compression shutdown analysis.
- Streaming must only recover pre-body. The status check in `run_model_turn` is at
  `native_agent.rs:2730`, before `forward_sse` consumes the body at `:2739`. A
  401/402/429 is delivered as an HTTP status before the SSE stream starts, so
  `dispatch_chat` can rotate and retry without any bytes having been emitted to
  the caller. A failure that surfaces mid-stream (after a 200) is not a status
  error, is not classified as recoverable, and must not rotate; retrying it would
  double-emit `MessageChunk` text to the user. This is the single correctness rule
  unique to the streaming transport, and it falls out naturally because
  `dispatch_chat` only ever sees the pre-body status.
- Current-credential identity. The failed key is identified precisely, not
  guessed. `rotate_after_failure` reads `current()` and passes both
  `credential.api_key()` and `credential.id()` into `mark_exhausted_and_rotate`
  (`native_agent.rs:531`), which prefers the id, falls back to a unique key match,
  and marks duplicate rows carrying the same key (`credential_pool.rs:1399` to
  `:1459`). So the exact dispatched entry is attributed even when two pool rows
  share a key.
- Bounded attempts. One rotation per `dispatch_chat`, tracked by a local flag, not
  a loop. A turn with N tool rounds can rotate at most once per round, but each
  rotation reloads the store and sees prior exhaustions, so a dead key is skipped
  by selection rather than re-tried. The per-request cap prevents a single round
  from draining the pool. The contract lane pins whether the main path, like
  compression's `recover_summary`, also grants an ordinary 429 one cheap retry on
  the same key before rotating (`native_agent.rs:740`); if so, that retry lives
  inside the same budget as one extra send, still bounded.

## 7. Deferred scope, stated plainly

- OAuth and device-code refresh is out. `load_pool_from_store` bails on
  `anthropic`, `openai-codex`, `xai-oauth`, `nous`, and `custom:`
  (`credential_pool.rs:828`). For those main providers `select_runtime` returns
  `Err`, `main_credential` is `None`, and the turn uses the frozen key with no
  recovery, exactly as today. The Nous OAuth arm of `CompressionPoolCredential`
  (`CompressionCredentialSource::Nous`, `native_agent.rs:483`, and its async
  refresh at `:687`) is deliberately not lifted into `MainPoolCredential` for this
  checkpoint. Wiring native OAuth refresh into the main turn is a separate lane;
  the boundary is the `SEEDING_REQUIRED` list.
- The candidate is one provider; only the key recovers. Recovery rotates among
  keys of the already-resolved main provider. It never switches to a different
  provider or activates a configured fallback provider; that is the fallback-chain
  lane, out of scope here. A key added to the store out of band becomes usable on
  the next turn because each rotation reloads the store, but a wholly new provider
  requires a rebuild.
- New providers or keys appearing mid-conversation. The conversation client is
  cached under `(home, session_id)` (`conversation_agent.rs:120`) and rebuilt only
  on eviction. A store change is picked up by the next rotation (which reloads) or
  the next rebuild, not proactively. This is the same frozen-set-with-live-key
  boundary the compression seam documented.
- Non chat-completions transports. This seam is `/chat/completions` only, the URL
  both callers already use (`native_agent.rs:2714`, `:3240`). A Responses-API or
  other transport is out of scope.
- TLS customization of the main turn client. Not added here; section 4 only flags
  the rebuild trap for whoever adds it later.

## 8. Generalize `CompressionPoolCredential`, or keep it separate

Recommendation: lift the ApiKey arm into a neutral, transport-agnostic binding
that both paths share; keep the Nous arm where it is for now.

Why lift. Today's `CompressionPoolCredential` already is the shape the main path
needs: a `PoolLocator`, an `Arc<Mutex<Option<RuntimeCredential>>>` cursor, a
`fallback_base_url`, and a `rotate_after_failure` that reads `current()` and calls
`mark_exhausted_and_rotate` with the id and key hint (`native_agent.rs:474` to
`:543`). Reimplementing that for the main path would duplicate the exact
persist-before-retry, identity, and cursor-propagation logic that is already
golden-tested, and would invite the two copies to drift. The classifiers it uses
(`compression_auth_failure`, `compression_payment_failure`,
`compression_rate_limit_failure`) are provider-neutral string checks on the error,
not compression-specific, so they carry over unchanged.

Why not lift wholesale. The type is named for compression, carries a
`CompressionCredentialSource` enum whose `Nous` variant does OAuth refresh
(deferred for main), and its recovery is redacted through `compression_redact`
(`native_agent.rs:528`). Dragging the whole enum into the main path pulls in the
OAuth arm this checkpoint explicitly excludes.

Concrete shape. Extract an ApiKey-only `PoolCredentialBinding` into a small module
(for example `credential_binding.rs`) with the `new` / `current` /
`rotate_after_failure` / `can_recover` surface from section 2. Have
`CompressionCredentialSource::ApiKey` hold, and the main client hold, the same
`PoolCredentialBinding`. `CompressionPoolCredential` keeps its Nous arm and
delegates its ApiKey arm to the shared type. The main path never sees Nous. This
is a mechanical extraction: no behavior change for compression, one shared code
path for the ApiKey recovery that both transports and both lanes exercise.

## 9. Open items for the contract oracle (agy lane)

- Whether the main path rotates within the pool on a single request at all, or
  only records the failure and surfaces the error to the caller. Section 3 assumes
  one rotation-and-retry per dispatch; if Python's main loop does not rotate
  in-request, the seam shrinks to record-and-fail and `dispatch_chat`'s retry arm
  is dropped.
- Whether an ordinary 429 on the main key gets one cheap same-key retry before
  rotation, mirroring compression's `recover_summary` (`native_agent.rs:740`), and
  whether that retry counts against the same per-request budget.
- The exact per-key store transition and cooldown for 401 vs 402 vs 429 on the
  main path, and whether a 402 marks one key or the whole provider. Bind to
  `mark_exhausted_and_rotate` and the `exhausted_ttl` ladder rather than guessing.
- Whether a pooled main entry's `base_url` may name a different host than the
  configured provider. If yes, section 4's header-preservation is a leak and the
  base_url swap must be gated on same-host; if no, preservation is correct as
  written.
- The precedence at build among `config.llm_api_key`, `user_config.model.base_url`,
  a registered profile key, the profile/root pool entries, and the dotenv or
  environment key. Section 2 puts pool selection ahead of the env fallback and
  behind an explicit `config.llm_api_key`; the oracle must pin the full order,
  since it decides whether an explicit constructor key suppresses the pool.
- Whether `pool_strategy(provider, user_config)` (`credential_pool.rs:pool_strategy`,
  used at `main.rs:548`) is the right strategy source and default for the main
  provider, or whether the main path reads a different config key.

## 10. Red-first integration tests

All drive real seams: the `NativeAgentClient` public turn entry
(`run_native_turn` / `run_turn`) or `build_agent_client_for_home` end to end,
against an axum test server per behavior, with `secret_scope::with_secret_scope`
for profile isolation as the existing multiplex tests do (`main.rs:2510`). Each is
written to fail against the tree at 1b3b4173c3, where the main path has no
recovery.

- Streaming rotate on 401. Two manual keys in the profile store for the main
  provider. First key returns 401 on the streaming completion; assert the store
  marks that entry exhausted with code 401, the second key's bearer reaches the
  wire, the turn completes, and no partial `MessageChunk` was emitted before the
  retry. Fails today: the turn errors on the 401.
- Tool-loop rotate across rounds. A tool turn where round 1 succeeds, round 2
  returns 402 on the first key; assert the rotation persists between rounds, round
  2 retries on the second key, and rounds 3+ continue on the second key (cursor
  propagation across `step` calls). Fails today.
- No rotation target. One key returning 401; assert the entry is exhausted in the
  store and the turn fails with the 401 error, with no retry beyond budget. Then
  advance the injected clock past the store TTL and assert the key is eligible on
  the next turn.
- Byte stability across retry. Capture both request bodies at the test server;
  assert attempt one and attempt two are byte-identical except for the
  Authorization header (and base_url if the entry overrode it). Guards against a
  retry that rebuilds the prompt or reorders tools.
- Streaming mid-body failure does not rotate. First key returns 200 then a
  truncated or error SSE frame mid-stream; assert no rotation, no store write, and
  no duplicated emitted text. Guards the pre-body-only rule.
- Cancellation between persist and retry. Inject a rotation that persists, then
  drop the turn future before the retry send; assert the store shows the first key
  exhausted and the cursor points at the replacement, and a fresh turn resumes on
  the replacement. Guards persist-before-retry under cancellation.
- Cross-conversation store race. Two sessions under one profile, each rotating the
  same provider concurrently; assert both exhaustion marks survive (idempotent),
  the store is never torn, and neither deadlocks. Guards the request-local pool +
  atomic write claim.
- Profile isolation. Two sessions under `red` and `blue` scopes with different
  two-key pools for the same provider name; both hit 401 and rotate; assert each
  rotates within its own `auth.json` and neither sees the other's key. The leak
  test for the request-local design.
- Env-only key unchanged. A provider with no store-backed pool (`main_credential`
  is `None`); assert a 401 fails the turn exactly as today, the store is untouched,
  and no rotation is attempted. Guards the strict-subset fallback.
- Deferred provider fails closed. Main provider is `anthropic` or `nous`;
  `select_runtime` returns `Err`, `main_credential` is `None`, and behavior matches
  today with the store untouched. Guards the OAuth boundary.
- Bounded attempts. Several pooled keys all returning 401; assert exactly one
  rotation and two wire attempts per dispatch, not a drain.

## Recommendation

Wire the main provider through the credential pool by lifting the ApiKey arm of
`CompressionPoolCredential` into a shared `PoolCredentialBinding`, giving
`NativeAgentClient` an optional `main_credential` cursor resolved at build via
`PoolLocator::select_runtime`, and routing both the streaming completion in
`run_model_turn` and the tool-loop `ChatModel::step` through one new
`dispatch_chat` seam that classifies the pre-body status, rotates once inside a
per-request budget on the shared cursor, persists before retrying, and re-sends
byte-identical inputs. Keep OAuth refresh, fallback-provider activation, non
chat-completions transports, and any TLS customization of the main client out of
this checkpoint. The mechanism already exists and is proven for compression; the
work is the wiring plus the one new streaming rule that recovery only fires on the
pre-body HTTP status, never mid-stream. Do not implement until the contract lane
pins in-request rotation, the build precedence, and the base_url same-host
question in section 9.
