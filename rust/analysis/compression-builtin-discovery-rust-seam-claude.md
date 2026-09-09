# Rust seam: built-in auxiliary provider discovery for auto-mode compression

Scope: design the narrowest production seam for the built-in provider discovery
tier that Python consults after the two configured compression fallback tiers
are exhausted (`_get_provider_chain` / `_resolve_auto_route` /
`_try_payment_fallback` in `agent/auxiliary_client.py`). This is a Rust-only
ownership and interface lane. The authoritative Python contract and its goldens
are owned by the parallel agy lane
(`rust/analysis/compression-builtin-discovery-contract-agy.md` and
`rust/tools/compression-builtin-discovery-goldens.json`, not yet landed). This
document does not re-derive that contract; it cites only the parts that pin a
Rust interface and it anchors every symbol to code you can read in the working
tree today.

Baseline: the working tree already ships the two frozen configured tiers. The
task chain (`auxiliary.compression.fallback_chain`) and the top-level main chain
(`fallback_providers` / `fallback_model`) are both resolved to clients at
startup and stored in `CompressionRoutes` (`native_agent.rs:450`). The sibling
seam report `compression-main-fallback-rust-seam-claude.md` covers those two
tiers and explicitly listed this discovery tier as step 4, "not ported". This
report picks up exactly there.

## 1. Why this tier is not another frozen `Vec<NativeAgentClient>`

The task and main tiers are pure config. Their entries are parsed once
(`compression_auxiliary::Config::fallback_chain`, `main_fallback_chain`), built
once through `build_native_compression_client` (`main.rs:499`), and stored in an
immutable `Arc<CompressionRoutes>` (`native_agent.rs:494`). Nothing about them
changes after `build_agent_client_for_home` returns, which is why the whole plan
is frozen and shared by clone across every conversation turn.

Built-in discovery breaks that assumption in three ways that the contract lane
will pin precisely but that are already visible in the Rust modules:

- Unhealthy TTL. Python marks a discovered provider unhealthy for a bounded
  window after a failed request and skips it until the window expires. That is
  mutable state whose lifetime is the process, not one conversation. It must be
  observable by, and mutated from, every conversation client that shares the
  discovery surface.
- Credential refresh. Discovery selects providers whose credentials are live.
  For OAuth / device-code providers that means refreshing tokens as they near
  expiry. `credential_pool.rs` states plainly at its head that "Store hydration,
  refresh and lease ownership are still being ported", and `auth_store.rs`
  states "Reading rows does not select, refresh or lease a usable credential."
  So refresh is process-shared mutable state that does not exist natively yet
  (see section 8).
- Client eviction. When a refresh rotates a token or a provider goes unhealthy,
  the cached client for that provider must be dropped and rebuilt on next use.
  A frozen `Arc<CompressionRoutes>` cannot evict anything.

Conclusion for the interface: discovery cannot be a fourth field of the shape
`discovery_fallbacks: Vec<NativeAgentClient>` next to `task_fallbacks` and
`main_fallbacks`. A `Vec<NativeAgentClient>` is frozen at build time and cannot
carry TTL, refresh, or eviction. Discovery must be a process-shared source
behind interior mutability that the frozen conversation client only holds a
handle to.

## 2. The shape the frozen client should hold

Add one field to `CompressionRoutes` (`native_agent.rs:450`):

```
discovery: Option<std::sync::Arc<dyn CompressionDiscovery>>,
```

where the trait is small and request-shaped:

```
pub(crate) trait CompressionDiscovery: Send + Sync {
    /// Return at most one ready built-in route, honoring health TTL and
    /// skipping the already-failed and main providers. `None` means the
    /// discovery surface has nothing eligible right now.
    fn next_ready(
        &self,
        skip: &DiscoverySkip,
        now: std::time::Instant,
    ) -> Option<NativeAgentClient>;

    /// Record the outcome of the one request that ran so TTL and eviction
    /// advance. Called only when a discovery route actually fired.
    fn record_outcome(&self, identity: &BackendIdentity, outcome: DiscoveryOutcome, now: std::time::Instant);
}
```

The `Arc<dyn CompressionDiscovery>` is the only new thing the frozen client
owns, and it owns it immutably. All mutation lives behind the trait object's own
interior lock (a `Mutex` over the health map plus the small cached-client map).
Cloning a `NativeAgentClient` clones the `Arc`, so every conversation shares one
discovery surface, which is exactly the "state must remain live across frozen
conversation clients" requirement. The trait returns an owned
`NativeAgentClient` built on demand, so the returned route is not itself stored
in the frozen plan and cannot mutate it.

This is deliberately not a second executor. `next_ready` selects and builds one
candidate; it does not run the request. The existing traversal runs it (section
4).

## 3. Reusable resolution and discovery inputs

Almost the entire "turn a provider name into a working native client" surface
already exists and must be reused unchanged so discovery does not fork a second
resolver:

- `build_native_compression_client` (`main.rs:499`) already does profile lookup
  (`profiles.get`), the `inherits_main` path (`main.rs:520`), custom-provider
  resolution through `custom_provider_config::named` (`main.rs:536`), base-URL
  and key precedence (`main.rs:568` and `main.rs:632`), `api_mode` gating to
  `chat_completions` (`main.rs:617`), `models_dev` context-window lookup
  (`main.rs:659`), and extra-header projection (`main.rs:667`). A built-in
  discovery candidate is just a provider name plus a resolved credential, which
  is a strict subset of what a `MainFallback` route already resolves. The
  discovery source should call `build_native_compression_client` with a new
  route-config variant rather than resolving anything itself.
- The provider catalog is `ProviderRegistry` (`provider_registry.rs:494`),
  populated by `register_bundled_base_profiles` (`provider_registry.rs:502`)
  plus the three hook profiles (`register_upstage`, `register_nebius`,
  `register_vercel`). `list()` (`provider_registry.rs:583`) returns profiles in
  stable insertion order, which is the natural basis for a deterministic
  discovery order once the agy lane pins the exact ordering.
- Static credential discovery is `credential_sources.rs`
  (`seed_from_env`, `seed_custom_pool`, `ProfileEnvSource`) reading through the
  same env/dotenv surface. This is the only credential path that resolves today
  without refresh, so it is the only one native discovery can use in the first
  checkpoint.

New code that discovery genuinely needs, and nothing more: the health/TTL map,
the small cached-client map with eviction, and a thin route-config variant. The
variant belongs next to the existing ones in `main.rs:373`:

```
CompressionRouteConfig::BuiltinDiscovery { provider: &str, task: &compression_auxiliary::Config }
```

Its accessors mirror `MainFallback` exactly (timeout returns `task.timeout`,
`reasoning_config` and `certified_output_cap` return `None`,
`request_extra_body` returns the task-level body with the `claims_fast_lane`
reasoning strip, `direct_api_key` resolves through the shared credential path).
A discovered provider is never a certified fast lane, so reusing the
`MainFallback` accessor behavior verbatim is correct, not speculative.

## 4. Selecting one discovery request without a parallel executor

The existing traversal in `summarize_history_with_memory` (`native_agent.rs:1275`)
builds a `Vec<PlannedRoute>` and walks it with a single shared one-shot gate
`configured_fallback_attempted` (`native_agent.rs:1366`). The task and main
tiers already share that gate so at most one configured fallback ever fires
(`native_agent.rs:1380`). Discovery joins that same gate as a terminal step; it
does not get its own loop.

Concretely, after the `main_fallbacks` are appended (`native_agent.rs:1350`) and
only when `main_first` is true, append one terminal planned route:

```
PlannedRoute::Discovery
```

In the traversal body, `PlannedRoute::Discovery` is reached only if
`configured_fallback_attempted` is still false, which already encodes the Python
mutual-exclusion contract from the sibling report: if any task or main candidate
was eligible it ran and set the flag, so discovery is never consulted; if none
was eligible, discovery is the last resort. When reached, the step calls
`self.compression_routes.discovery.as_ref()?.next_ready(&skip, Instant::now())`,
where `skip` is built from `first_failure` (the failed identity and scope) plus
the main provider. If `next_ready` returns a client, run it through the
identical request path already used for every other route:

```
match discovered.summarize_history_on(database, session_id, &prompt).await { ... }
```

Set `configured_fallback_attempted = true` immediately before that call so the
one-shot invariant holds across all three fallback surfaces combined. After the
request completes, call `discovery.record_outcome(...)` so TTL and eviction
advance. `next_ready` returning `None` must not set the gate, matching how an
ineligible configured entry does not consume the one shot (`native_agent.rs:1392`
and `:1417`).

Why this is the narrowest wiring: it reuses `summarize_history_on`
(`native_agent.rs:1246`) unchanged, which already resets
`compression_routes = Default::default()` on the clone (`native_agent.rs:1253`)
and flips the usage bucket to `Auxiliary`. So a discovered route inherits both
invariants for free: it cannot recursively own a plan, and its usage is
attributed as auxiliary. No new request executor, no duplicated retry loop.

## 5. Preserving the four invariants

Route clients cannot recursively own plans. Satisfied structurally.
`next_ready` returns a plain `NativeAgentClient` built through
`build_native_compression_client`, whose `compression_routes` defaults to empty
(`native_agent.rs:551`), and `summarize_history_on` clears it again on the clone
(`native_agent.rs:1253`). A discovered client therefore has no discovery handle
of its own and cannot re-enter discovery.

Auxiliary usage attribution. `summarize_history_on` sets
`usage_bucket = Auxiliary` and calls `begin_auxiliary_usage` /
`take_auxiliary_usage` / `record_compression_usage` (`native_agent.rs:1254`).
`record_compression_usage` (`native_agent.rs:941`) persists to the session DB
keyed by `session_id` and by the client's own `provider_name()` / `model` /
`base_url`. So attribution is correct as long as `next_ready` builds the
discovered client with the real discovered provider identity via
`with_provider_identity` (as `build_native_compression_client` already does at
`main.rs:653`). This is a hard requirement on the discovery source: a discovered
client whose `provider_name()` is empty or wrong would misattribute compression
usage. The section 7 test pins it.

Profile secret isolation. This is the sharpest tension in the whole seam. The
configured tiers resolve credentials inside `build_agent_client_for_home` under
the active secret scope: `secret_scope::current_secret_scope()` with the
fail-closed guard at `main.rs:724`, the profile `dotenv` snapshot at
`main.rs:730`, and the `environment` closure `secret_scope::get_secret(name,
None)` at `main.rs:734`. A process-shared discovery source outlives any one
profile scope, so it must not cache resolved secrets or resolved clients keyed
only by provider. If it did, conversation A's key for provider `p` could be
handed to conversation B under a different profile. Two rules keep isolation
intact:

- The shared discovery source stores only health/TTL state keyed by
  `BackendIdentity`, never a resolved credential.
- Credential resolution and client construction happen per call inside
  `next_ready` using a scope-bound closure passed in from the requesting
  conversation, not a closure captured at source-construction time. In practice
  `next_ready` takes the same `dotenv` snapshot and `environment` closure that
  `build_native_compression_client` already threads, so the discovered client is
  built in the caller's scope exactly like every configured route. Any cached
  client must be keyed by `(scope-hash, identity)` or simply not cached across
  scopes at all in the first checkpoint.

Prompt byte stability. Untouched. The prompt is assembled once by
`compression_prompt::build_with_memory` at the top of
`summarize_history_with_memory` (`native_agent.rs:1283`) and the same `&prompt`
is handed to every route including discovery. Discovery adds a route, not a
prompt variant, so the prompt bytes and the prompt-cache prefix are identical
across all tiers. The seam must not touch prompt assembly.

## 6. Where the shared state is created, and the one real structural cost

Today `ProviderRegistry::default()` is constructed fresh inside
`build_agent_client_for_home` (`main.rs:735`) on every conversation build, and
there is no process-lifetime mutable state in `main.rs` at all (no `OnceLock`,
no lazy static). `build_agent_client_for_home` is called per conversation from
`build_agent_client` (`main.rs:360`) and from the conversation rebuild path
(`main.rs:1299`), and conversation clients are frozen behind
`OnceCell<Arc<dyn AgentClient>>` (`conversation_agent.rs:33`).

So the discovery source is the first piece of genuinely process-shared mutable
state this layer would own. That is the real cost of this seam and it must be
introduced deliberately, not smuggled in. Recommendation: create one
`Arc<dyn CompressionDiscovery>` at server/process setup and thread it into
`build_agent_client_for_home` as a parameter (or store it in a single
`OnceLock` initialized at startup), then clone the `Arc` into
`with_compression_routes`. Do not construct a new discovery source per build;
that would reset health TTL every conversation and defeat the point. This is the
one place where the "everything is frozen per conversation" model must bend, and
the design keeps the bend to a single `Arc` handle.

Interface-growth flags:

- `with_compression_routes` (`native_agent.rs:699`) already takes five
  positional arguments. Adding a sixth `Option<Arc<dyn CompressionDiscovery>>`
  is acceptable for one production call site (`main.rs:978`), but this is the
  threshold where a small `CompressionRoutesBuilder` or a `CompressionRoutes`
  struct-literal at the call site becomes cleaner than more positional
  arguments. Flag for review; do not grow to seven.
- Do not add discovery accessors to `CompressionRouteConfig` beyond the single
  `BuiltinDiscovery` variant that mirrors `MainFallback`. Any per-provider
  discovery tuning surface (per-provider timeout, per-provider reasoning) is
  speculative until the contract lane shows Python reads it, and Python's
  discovery path does not read per-entry request tuning.
- Resist a `DiscoveryPlan` type stored in the frozen client. The plan is
  inherently live; freezing a snapshot of it would be a shallow abstraction that
  re-introduces the staleness problem section 1 rules out.

## 7. Local HTTP and stateful tests

These prove the five behaviors the task names. They extend the existing
axum-per-route harness used by the route-order tests in `native_agent.rs`
(around `native_agent.rs:3336`, `with_compression_routes` fixtures) and the
end-to-end `build_agent_client_for_home` test in `main.rs`
(`compression_routes_preserve_explicit_and_auto_fallback_order`, `main.rs:1893`).

- Provider order. Register a fake discovery source over three local axum
  servers in a known order. With no configured fallbacks and both main and (if
  present) primary returning unusable summaries, assert discovery is consulted
  and hits the servers in the source's declared order until one returns a usable
  summary. Assert the earlier servers were hit before the later ones.
- Health expiry (TTL). Drive the discovery source with an injected clock. Mark
  provider one unhealthy, assert `next_ready` skips it and selects provider two.
  Advance the injected `now` past the TTL, assert provider one becomes eligible
  again. This is why `next_ready` / `record_outcome` take `now:
  std::time::Instant` rather than reading the clock internally: the TTL test
  must be deterministic, not wall-clock dependent.
- One-request execution. Configure an eligible task fallback that returns an
  unusable response, plus a live discovery source. Assert that no discovery
  server is ever hit, proving the shared `configured_fallback_attempted` gate
  spans discovery too. Then remove the task fallback and assert exactly one
  discovery server is hit even when several are ready.
- Credential recovery. Start with the discovery provider's credential absent
  (env unset) so `next_ready` cannot build it and returns `None`; assert the
  ladder falls through to whatever remains. Then set the credential and, on the
  next compression, assert discovery now builds and fires that provider. This
  proves the source resolves credentials per call in the caller's scope rather
  than caching a stale absence. Full OAuth-refresh recovery is deferred to the
  later checkpoint (section 8); this test covers the static api_key recovery
  that is native today.
- Conversation isolation. Build two conversation clients under two secret
  scopes with different keys for the same discovery provider (drive through
  `secret_scope::with_secret_scope`, as the existing multiplex tests do around
  `main.rs:3211`). Run compression on both. Assert each conversation's discovery
  request carried its own scope's `Authorization` bearer, proving the shared
  source never leaked one scope's secret into the other and never cached a
  cross-scope client.

Parser-level parity: once the agy lane lands
`compression-builtin-discovery-goldens.json`, bind the discovery order and the
health-skip predicate to it the same way the task and main parsers bind to their
goldens (the `include_str!` pattern already used across
`compression_auxiliary.rs` tests).

## 8. Behavior that must split to a later checkpoint

The task asks explicitly to flag behavior whose transport is not native. Three
groups must be deferred and the first checkpoint must fail closed on them rather
than silently skip them wrong:

- OAuth and device-code credential refresh. `credential_pool.rs` and
  `auth_store.rs` both state refresh/lease ownership is unported.
  `credential_persistence.rs::is_borrowed` (`credential_persistence.rs:23`)
  enumerates the refresh-backed source types (`minimax-oauth` / `oauth`,
  `nous` / `openai-codex` / `xai-oauth` / `device_code`). Any discovered
  provider whose only credential is one of these cannot be refreshed natively
  yet, so checkpoint 1 discovery must include only providers resolvable through
  the static api_key path in `credential_sources.rs`. Providers gated on refresh
  are deferred until credential lease/refresh is ported. This is the single
  biggest reason native discovery is a strict subset of Python discovery in
  checkpoint 1.
- Non-`chat_completions` transports. `NativeAgentClient::with_provider_profile`
  (`native_agent.rs:628`) and `build_native_compression_client` (`main.rs:617`)
  both hard-reject any `api_mode` other than `chat_completions`. Any built-in
  provider whose profile is `anthropic_messages`, `codex_responses`, or
  `bedrock_converse` must be skipped by discovery in checkpoint 1 and ported
  when those native transports land. The discovery source should filter on
  `profile.api_mode == "chat_completions"` before offering a candidate, so it
  never offers a route that `build_native_compression_client` would then reject.
- `_try_payment_fallback` specifics. HTTP 402 already classifies as
  `FailureScope::Credential` via `compression_failure_scope`
  (`native_agent.rs:461`) and `classify_failure_reason`
  (`compression_auxiliary.rs:43`), so the failure-scope plumbing exists. But
  whether the payment path selects a different provider subset than ordinary
  discovery, and in what order, is a contract question for the agy lane. Until
  that lands, treat payment-triggered discovery as the same `next_ready`
  surface with a credential-scoped skip, and flag any divergence the oracle
  reports.

## 9. Recommended implementation sequence

1. Trait and outcome types. Add `CompressionDiscovery`, `DiscoverySkip`,
   `DiscoveryOutcome` in a new small module (or in `compression_auxiliary.rs`
   beside `BackendIdentity` / `FailureScope`). No behavior yet.
2. Route config. Add `CompressionRouteConfig::BuiltinDiscovery { provider, task }`
   in `main.rs:373` with accessors copied from `MainFallback`. Unit-test that its
   timeout is `task.timeout`, reasoning and output cap are `None`.
3. Shared source. Implement one `CompressionDiscovery` whose state is a
   `Mutex<{ health: HashMap<BackendIdentity, Instant>, clients: HashMap<key,
   NativeAgentClient> }>`, an injected clock for tests, a scope-bound
   `next_ready` that filters to `chat_completions` + static-credential providers,
   builds through `build_native_compression_client`, and honors TTL and the skip
   set. Deterministic order from `ProviderRegistry::list()`
   (`provider_registry.rs:583`) narrowed by the agy order once it lands.
4. Plan field. Add `discovery: Option<Arc<dyn CompressionDiscovery>>` to
   `CompressionRoutes` (`native_agent.rs:450`) and a parameter to
   `with_compression_routes` (`native_agent.rs:699`). Flag the argument count
   (section 6).
5. Traversal. Append the terminal `PlannedRoute::Discovery` after
   `main_fallbacks` when `main_first`; reach it only if
   `configured_fallback_attempted` is false; run through `summarize_history_on`;
   set the gate before firing; call `record_outcome` after.
6. Startup. Create the discovery source once at process setup, thread its `Arc`
   into `build_agent_client_for_home`, and pass it to `with_compression_routes`
   (`main.rs:978`) only when `compression_auto` is true (`main.rs:922`). Include
   it in the "install routes only if any exist" guard (`main.rs:967`).
7. Tests. Add the five tests from section 7.
8. Parity and checkpoint boundary. Bind order and health-skip to the agy
   goldens; document at the source and at `next_ready` that OAuth-refresh
   providers and non-`chat_completions` transports are deliberately excluded
   until their checkpoints land.

## 10. Open items for the contract oracle

- Exact discovery provider set and order, and whether it derives from
  `ProviderRegistry` insertion order or a separate hard-coded chain in
  `_get_provider_chain`.
- Unhealthy TTL duration and whether the window is per-provider or per-identity
  (provider + model + base_url). The Rust health map keys on `BackendIdentity`;
  confirm that matches Python granularity.
- Whether `_resolve_auto_route` can execute more than one discovery candidate in
  the request layer, or exactly one like the configured tiers. Section 4 assumes
  exactly one, consistent with the request-layer mutual-exclusion the sibling
  report established for the configured tiers. If Python instead loops discovery
  until success within a single compression, the terminal single-step design in
  section 4 becomes a bounded loop over `next_ready`, still behind the same gate
  and still not a separate executor, but the gate semantics would change.
- Whether payment-fallback (HTTP 402) discovery uses a different provider subset
  or order than ordinary exhaustion discovery (section 8).
