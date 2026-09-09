# Rust seam: cross-provider fallback for the ordinary main turn

Scope: design the narrowest production seam that activates the top-level
`fallback_providers` chain plus legacy `fallback_model` on the ordinary main
conversation turn, after the active provider and its credential pool cannot
recover. This is the main-turn analog of the compression main-fallback seam
(`compression-main-fallback-rust-seam-claude.md`), but for live user turns:
streaming completions and the multi-round tool loop, not one-shot summaries. It
builds directly on the credential-pool recovery that already landed at
`d87226dbc9` ("Port native main credential recovery").

This is a Rust ownership and interface lane. The Python behavior contract and
its goldens belong to the parallel agy lane
(`rust/analysis/main-provider-fallback-contract-agy.md`,
`rust/tools/gen_main_provider_fallback_goldens.py`,
`rust/tools/main-provider-fallback-goldens.json`, not yet landed). This document
does not re-derive that contract. It cites the live Python only where a fact
pins a Rust interface, and it anchors every Rust symbol to code readable in the
working tree at `d87226dbc9`. Anything that needs the oracle to be safe is
called out in section 12.

## 0. What exists today, precisely

The within-provider credential-pool recovery is live and is the only recovery
the main turn has.

- Both transports already funnel through one dispatch method. The streaming
  completion in `run_model_turn` calls `self.send_main_request(&body, "")`
  (`native_agent.rs:3240`) and then `forward_sse`; the tool round in
  `ChatModel::step` calls `self.send_main_request(&body, "step")`
  (`native_agent.rs:3762`) and then decodes JSON. `send_main_request`
  (`native_agent.rs:1570`) is therefore the single existing seam every main
  request passes through. That is the anchor this checkpoint extends.
- `send_main_request` rotates within the active provider's key pool and returns
  a terminal error in exactly these cases:
  - the client has no pool (`main_pool` is `None`) and the status is non-success
    (`native_agent.rs:1614`);
  - the failure classifies as `MainPoolFailure::Unrelated` or
    `UpstreamRateLimit` (`native_agent.rs:1618`), which never rotate;
  - `rotate_after_failure` returns `None`, that is the pool is drained
    (`native_agent.rs:1672`);
  - the replacement equals the just-failed entry (`native_agent.rs:1675`).
  In every terminal case it returns `main_http_error(label, status, &text)`
  (`native_agent.rs:1253`), an opaque `Error::Other("native agent ... HTTP
  {status}: ...")`.
- The credential cursor is a shared handle. `MainPoolCredential`
  (`native_agent.rs:881`) holds a `PoolLocator`, a `fallback_base_url`, an
  `Arc<Mutex<ActiveMainCredential>>` (`native_agent.rs:884`, the live key plus
  its `reqwest::Client`), and an `Arc<Vec<(route, HeaderMap)>>` of per-route
  headers. `route()` (`native_agent.rs:935`) clones out the active
  key/base_url/client; `install_replacement` (`native_agent.rs:953`) swaps under
  the lock guarded by a compare on `(id, api_key)`. Because it is all `Arc`, a
  rotation is visible to every clone of the conversation client.
- The pool cursor is built at startup only. In
  `build_agent_client_for_home_with_discovery` (`main.rs:957`) the pool is
  resolved once (`main.rs:1078`), gated to non-OAuth non-custom providers
  (`pool_supported`, `main.rs:1070`) and suppressed by an explicit runtime key or
  base URL (`explicit_runtime`, `main.rs:1068`), then installed via
  `with_main_pool` (`main.rs:1149`). No fallback-provider client is built for the
  main turn anywhere; `main_fallback_chain` is consulted only for compression
  (`main.rs:1272`).
- The parser is already landed and golden tested, for compression.
  `compression_auxiliary::main_fallback_chain` (`compression_auxiliary.rs:227`)
  merges `fallback_providers` then `fallback_model`, requires both provider and
  model, and dedups on `(provider, model, base_url)` lowercased.
  `should_skip_main_fallback_provider` (`compression_auxiliary.rs:295`) skips a
  literal `auto` entry and any entry equal to the failed or main provider.

So the gap is exact: the main turn recovers keys within one provider and then
fails the whole turn. It never switches providers. Everything on the parsing and
per-entry-credential side already exists; the missing piece is a frozen ordered
plan of alternate main-turn clients plus a sticky activation seam that both
transports enter, wrapping `send_main_request` rather than replacing it.

## 1. What the Python contract forces onto the interface

Restated as interface constraints, cited to the live Python for grounding only.
The agy lane owns the exact values.

1. Pool recovers first, then fallback. Cross-provider fallback is suppressed
   while the pool "may recover", that is `pool.has_available()` and more than one
   entry (`_pool_may_recover_from_rate_limit`, `run_agent.py:365`). A single-entry
   pool is allowed to skip straight to fallback. Upstream rate limit
   (`upstream_rate_limit`) bypasses the pool entirely because the model, not the
   credential, is throttled (`conversation_loop.py:6049`). In Rust this ordering
   is free: the fallback loop sits outside `send_main_request`, which already
   drains a multi-key pool before returning terminal.
2. Trigger classes. Immediate fallback on `rate_limit`, `billing`,
   `upstream_rate_limit` (`conversation_loop.py:6000`). Transport failures
   (`timeout`, `overloaded`) fall back only after two retries
   (`conversation_loop.py:6020`, `6036`). Auth 401/403 escalates after
   per-provider credential refresh is exhausted (`conversation_loop.py:6107`).
   A wrapped output-cap 429 is exempt.
3. Chain shape and order. `fallback_providers` then `fallback_model`, single dict
   or list, provider and model both required, dedup on
   `(provider, model, base_url)` lowercased (`fallback_config.py:80`). Already
   implemented by `main_fallback_chain`.
4. Bounds. Per-call retries default 3 (`api_max_retries`,
   `agent_init.py:2135`); each chain entry is attempted at most once per turn and
   `_fallback_index` advances once per candidate (`chat_completion_helpers.py:2757`,
   returns false at `>= len(chain)`, `:2739`); an outer error cap of 8
   (`conversation_loop.py:371`); a 5s exhausted cooldown to stop cross-turn
   replay storms (`chat_completion_helpers.py:67`).
5. Stickiness. Sticky within a turn and across tool rounds: once activated the
   swapped provider/model/client serves the rest of `run_conversation`, nothing
   restores mid-turn. Re-decided at the next turn: `restore_primary_runtime`
   (`agent_runtime_helpers.py:1641`) runs at the top of every turn from
   `turn_context.py:625`, resets `_fallback_index = 0`, and restores the primary
   snapshot unless a cooldown window `_rate_limited_until` is still active
   (`agent_runtime_helpers.py:1663`).
6. Per-entry credentials and endpoint. base_url and key come from the entry:
   inline `api_key`, else `key_env` / `api_key_env` through the profile secret
   scope (`fallback_config.py:14`), passed to the client build
   (`chat_completion_helpers.py:2817`). Provider default headers are preserved
   across the rebuild (`chat_completion_helpers.py:3010`).
7. Body and prompt. The system prompt's model-identity line is rewritten to the
   answering model (`chat_completion_helpers.py:3150`,
   `_sync_failover_system_message` at `conversation_loop.py:1743`); each
   provider's own `extra_body` and prompt-cache decoration are re-applied while
   the tool definitions stay stable.
8. Isolation. Same-backend entries are skipped by backend identity
   (`should_skip_candidate`, `backend_identity.py:189`); the fallback provider's
   own pool is loaded on switch (`chat_completion_helpers.py:2963`); there is no
   literal `"auto"` skip on the main turn, isolation is by identity.
9. Restoration. A new conversation constructs a fresh agent with
   `_fallback_index = 0` (`agent_init.py:1586`); an explicit model switch
   re-snapshots the primary and resets the fallback flags
   (`agent_runtime_helpers.py:3470`).
10. Usage. The call is attributed to the live swapped provider/model, with
    `call_role = "fallback"` once `_fallback_index > 0`
    (`conversation_loop.py:3643`).
11. Transports. Static chat-completions key providers are full participants now.
    Anthropic messages, Nous dual-wire, openai-codex Responses, Bedrock, and
    per-entry OAuth all participate in Python (`chat_completion_helpers.py:2828`
    onward) but are out of scope for a chat-completions static-key Rust
    checkpoint (section 13).

## 2. Two designs, compared

### Design A: turn-level re-drive

Wrap the whole turn. `run_native_turn` catches a classified fallback-eligible
terminal error from `run_model_turn`, rebuilds the turn on the next fallback
client, and re-runs it.

Rejected. The streaming transport has already emitted `MessageChunk` text to the
caller by the time most errors surface, and the tool loop has already executed
tool calls and appended them to the durable transcript. Re-running the turn
double-emits and double-executes. To be safe the catch must be pre-body only,
which is exactly the point `send_main_request` already isolates. Turn-level
re-drive also cannot express within-turn stickiness cleanly: a fallback that
activates on tool round 3 must keep serving rounds 4 and later without unwinding
rounds 1 and 2. Re-drive throws that state away.

### Design B: sticky active-route inside the shared dispatch seam (recommended)

Freeze an ordered vector of alternate main-turn clients per conversation, plus a
sticky active-route cursor shared across clones. Both transports call one seam
that picks the active serving client from the cursor, has that client build its
own request body and run its own pool dispatch (`send_main_request`), and on a
fallback-eligible terminal error advances the cursor to the next eligible route
and retries, bounded by the chain length. Usage is captured on the conversation
client regardless of which route served.

This is strictly more local and more correct than A. It reuses `send_main_request`
unchanged as the per-route inner dispatch, so per-route pool rotation, the 429
cheap retry, and persist-before-retry all keep working. It touches only the two
call sites in `run_model_turn` and `step`, which already share the seam. It
recovers only on the pre-body HTTP status, so streaming never double-emits and
the tool loop never re-executes a round. And it carries within-turn stickiness
in the cursor for free.

The one real cost of B: the request body depends on the serving client (its
`model`, provider profile, reasoning, base_url, `extra_body`), so the body must
be built per route, not once by the caller. That is why the seam takes a
body-builder closure rather than a finished body. This is the same shape the
compression executor already uses when it hands `&prompt` to each route's own
`summarize_history_on` (`native_agent.rs:2358` onward): the route client owns its
wire projection.

Recommendation: Design B.

## 3. What freezes per conversation, and what mutates

Frozen at build, immutable for the client's life:

- `main_fallbacks: Arc<Vec<NativeAgentClient>>`, the alternate main-turn clients
  in chain order (`fallback_providers` then `fallback_model`, deduped), each
  carrying an empty fallback plan and empty `compression_routes` so a fallback
  route can never recurse into another chain. This mirrors the existing
  "route clients never carry their own plan" invariant documented at
  `native_agent.rs:1281`.

Mutated at runtime, shared across all clones of one conversation client through
`Arc<Mutex<...>>`:

- `active_route: Arc<Mutex<usize>>`, the sticky cursor. `0` means the primary
  (`self`); `i >= 1` means `main_fallbacks[i - 1]`. Because the per-turn
  `turn_client = self.clone()` (`native_agent.rs:3254`) and the per-conversation
  cached client share the `Arc`, a switch on tool round 3 is visible to round 4
  and to `send_main_request`'s next call. This is the same propagation
  `MainPoolCredential.active` already relies on (`native_agent.rs:884`).
- `rate_limited_until: Arc<Mutex<Option<Instant>>>`, the cooldown window that
  decides whether the next turn re-attempts the primary or stays on the current
  fallback (section 8). This is the Rust analog of Python `_rate_limited_until`
  (`agent_runtime_helpers.py:1663`).

## 4. Data model and method sketches

Group the frozen plan and the runtime cursors in one struct, held by
`NativeAgentClient`:

```rust
#[derive(Clone, Default)]
struct MainFallbackRoutes {
    /// Chain order: fallback_providers then legacy fallback_model, deduped.
    /// Each client has empty MainFallbackRoutes and empty compression_routes.
    fallbacks: std::sync::Arc<Vec<NativeAgentClient>>,
    /// Sticky active route. 0 = primary(self); i>=1 = fallbacks[i-1].
    active: std::sync::Arc<std::sync::Mutex<usize>>,
    /// Stay on the active fallback until this instant; then the next turn may
    /// re-attempt the primary. None means "restore primary at the next turn".
    cooldown_until: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
}
```

`NativeAgentClient` gains one field:

```rust
pub struct NativeAgentClient {
    // ... existing fields ...
    /// Frozen cross-provider fallback plan plus the sticky activation cursor.
    /// Empty for a client that is itself a fallback route.
    main_fallback: MainFallbackRoutes,
}
```

Install at build, next to `with_main_pool`:

```rust
pub fn with_main_fallback_routes(mut self, fallbacks: Vec<NativeAgentClient>) -> Self {
    self.main_fallback.fallbacks = std::sync::Arc::new(fallbacks);
    self
}
```

The dispatch seam, called by both transports. It returns the serving provider
name so the caller labels usage and, for the streaming path, `forward_sse`,
correctly:

```rust
struct MainDispatch {
    resp: reqwest::Response,
    /// provider_name() of the client that actually served the request.
    served_provider: String,
}

impl NativeAgentClient {
    /// Send a main-turn chat request, activating cross-provider fallback once
    /// per route on a fallback-eligible terminal failure. `build_body` is given
    /// the serving client so the body is projected for that provider (model,
    /// reasoning, extra_body, base_url). The happy path builds one body and
    /// reads the store zero extra times.
    async fn dispatch_main_turn(
        &self,
        label: &str,
        build_body: impl Fn(&NativeAgentClient) -> Result<Value>,
    ) -> Result<MainDispatch> {
        let mut index = self.resolve_active_route(); // reads cursor, applies cooldown gate
        loop {
            let serving = self.route_at(index); // &self for 0, &fallbacks[i-1] otherwise
            let body = build_body(serving)?;
            match serving.send_main_request(&body, label).await {
                Ok(resp) => {
                    self.commit_active_route(index); // stick to the route that worked
                    return Ok(MainDispatch {
                        resp,
                        served_provider: serving.provider_name().to_owned(),
                    });
                }
                Err(terminal) => {
                    match self.next_fallback_route(index, &terminal) {
                        Some(next) => { index = next; continue; }
                        None => return Err(terminal), // final error shape unchanged
                    }
                }
            }
        }
    }
}
```

`route_at(0)` is `self`; `route_at(i)` is `&self.main_fallback.fallbacks[i - 1]`.
`next_fallback_route` returns the index of the next chain entry that (a) exists,
(b) is fallback-eligible for `terminal`'s class, and (c) is not skipped by the
identity predicate against the failed and main providers; else `None`.

Classification. `send_main_request` today computes `MainPoolFailure` and then
discards it into an opaque string via `main_http_error`. The seam needs the class
to decide eligibility. The minimal change is to have `send_main_request` return a
typed terminal error that carries the wire status and the `MainPoolFailure`, with
`From` back to today's `Error::Other` string so the final surfaced error is
byte-identical:

```rust
struct MainTerminal { status: reqwest::StatusCode, class: MainPoolFailure, body: String, label: String }
impl From<MainTerminal> for Error { /* == main_http_error(label, status, body) */ }
```

`next_fallback_route`'s eligibility check reads `MainTerminal::class`. Eligible
classes are pinned by the oracle (section 12); the current known-immediate set is
`RateLimit`, `Billing`, `BillingUnverified`, `Auth`, `UpstreamRateLimit`, with
transport/5xx gated on a retry count the Rust path does not yet track (section
12).

Both call sites change by one line each and add usage/label plumbing:

- Streaming (`native_agent.rs:3235`):
  ```rust
  let MainDispatch { resp, served_provider } = self
      .dispatch_main_turn("", |serving| {
          let mut body = build_request_body_from_messages(&serving.model, &history, content);
          serving.apply_provider_extras(&mut body)?;
          if supports_stream_usage(&serving.base_url) {
              body["stream_options"] = json!({"include_usage": true});
          }
          Ok(body)
      })
      .await?;
  let usage = forward_sse(resp.bytes_stream(), &events, &served_provider).await?;
  self.capture_usage(usage);
  ```
- Tool round (`native_agent.rs:3743`): the same closure builds the `stream:false`
  body with `serving.model` and the summary-strip when tools are empty, then
  `self.capture_usage(provider_usage::from_response(&v, ChatCompletions,
  Some(&served_provider)))`. Name repair still uses the `tools` argument, so it is
  unaffected by the switch.

The important ownership point: only the wire concerns (model, key, base_url,
client, provider headers, reasoning, extra_body, api_mode) come from the serving
route. Everything conversation-scoped stays on `self`: `usage_state`,
`system_prompt`, `tools`, `turn_limit`, `hooks`, `compression_routes`,
`micro_compaction_state`. That is why usage attributes to the conversation
client's Main bucket (`capture_usage`, `native_agent.rs:1847`) and only the
provider label follows the route.

## 5. Pool recovers fully before cross-provider fallback

This ordering is structural and needs no new code. `dispatch_main_turn` calls
`serving.send_main_request`, which for a pool-backed primary rotates through
every key, persisting each exhaustion, until the pool is drained or the failure
is non-rotating, and only then returns terminal (`native_agent.rs:1672`). The
fallback loop advances only on that terminal return. So a multi-key primary pool
is always fully rotated before any provider switch, matching
`_pool_may_recover_from_rate_limit`. For `Unrelated` and `UpstreamRateLimit` the
pool is never consulted (`native_agent.rs:1618`), which matches the Python
upstream-rate-limit bypass. The one place this differs from Python is the
single-entry-pool shortcut (Python allows skipping straight to fallback when the
pool has one entry): in Rust a single-entry pool still runs `send_main_request`,
which fails once and returns terminal, so the fallback still fires, just after one
doomed attempt rather than zero. Flag whether that extra attempt is acceptable
(section 12); it is one request, not a loop.

## 6. Route clients: reuse the parser, not the compression builder

Reuse for parsing and identity:

- `compression_auxiliary::main_fallback_chain(user_config)`
  (`compression_auxiliary.rs:227`) for the merged, deduped, validated chain.
- `compression_auxiliary::should_skip_main_fallback_provider`
  (`compression_auxiliary.rs:295`) for the build-time and runtime skip.
- `FallbackChainEntry::direct_api_key` (`compression_auxiliary.rs:204`) for the
  per-entry inline / key_env credential, resolved through the same `environment`
  closure and `dotenv` snapshot the main build already uses, so profile secret
  scope is preserved with no new surface.

Do not reuse `build_native_compression_client` (`main.rs:735`) to build a
main-turn route. It bakes in auxiliary-only semantics that are wrong for a live
turn: it installs `with_summary_request_policy` (a summary timeout and a
certified output cap, `main.rs:891`), never attaches tools, a turn limit, the
gateway output cap, `reasoning_echo`, an automatic-compression policy, or request
overrides in the main-turn shape, and its `step` path strips tools and applies a
summary temperature (`native_agent.rs:3748`). A main-turn fallback must answer the
user with tools and the same turn semantics as the primary.

Instead, extract the provider-config core of the primary build (the block
`main.rs:1124` to `main.rs:1209`: provider identity, profile attach, extra
headers including the OpenRouter defaults, `models_dev` context length, reasoning
config, reasoning echo, output cap, request overrides) into a shared helper
`build_main_turn_client(provider, model, base_url, api_key, ...) -> Result<Client>`
that both the primary path and a new `build_main_fallback_client(entry, ...)`
call. The fallback variant differs only in its inputs (per-entry provider, model,
base_url, key from `direct_api_key`) and in what it deliberately omits:

- no `main_pool`: a fallback route is a static per-entry key for this checkpoint;
  its own pool is deferred (section 13);
- no tools, turn limit, hooks, system prompt, usage state, compression routes,
  or nested fallback: all conversation-scoped and supplied by `self` at dispatch;
- `api_mode` gated to `chat_completions` exactly as `with_provider_profile`
  already enforces (`native_agent.rs:1438`); a non-chat entry is skipped at build
  and logged, never silently mis-sent.

The build loop lives in `build_agent_client_for_home_with_discovery` after the
primary client is assembled and before `with_compression_routes`, iterating
`main_fallback_chain(user_config)`, skipping entries via
`should_skip_main_fallback_provider(entry.provider, requested_provider,
requested_provider)` and any build error, collecting into a `Vec`, then
`c = c.with_main_fallback_routes(routes)` when non-empty.

## 7. Route-specific clients and headers

Each route client owns its own `reqwest::Client`, base_url, key, and provider
headers, so the seam sends the right endpoint and headers with no leakage. This
avoids the `with_runtime_credential` header/TLS trap the pool seam flagged
(`main-provider-pool-seam-claude.md` section 4): a fallback route is a different
provider identity, so it needs different provider headers, and it gets them
because it is a fully independent `NativeAgentClient` built through
`with_provider_profile` and `with_extra_headers`, not a header-preserving key
swap. The primary's per-route header table (`headers_for_main_route`,
`native_agent.rs:1554`) stays with the primary and its pool; it is not consulted
for a fallback route.

Prompt-cache stability. Within a single route the `system_prompt` bytes are the
same `Arc<str>` on every request and `apply_provider_extras` projects the same
wire copy, so the cache prefix is stable for the life of the route. Switching
providers necessarily changes the endpoint and therefore the cache tenant, which
is inherent to cross-provider fallback and unavoidable. The sticky cursor is what
protects cache stability in practice: once a route is chosen it serves every
later round and turn (within the cooldown window), so the conversation does not
oscillate providers and re-warm caches. Note the deferred piece: Python rewrites
the system prompt's model-identity line to the answering model on a switch
(`chat_completion_helpers.py:3150`); this checkpoint sends the primary's system
prompt unchanged to the fallback, which is a benign content difference, not a
protocol error. Flag it as a fidelity gap (section 12), not a blocker.

## 8. Stickiness across rounds and turns, cooldown, and restoration

Across tool rounds within one turn: sticky, for free. Once `commit_active_route`
writes a fallback index, the next `step` in the same tool loop reads it through
`resolve_active_route` and dispatches through the same route. The turn lease
serializes rounds, so the cursor is written by one round at a time.

Across later turns: re-decided with a cooldown gate, matching
`restore_primary_runtime`. `run_native_turn` begins a turn by cloning `self` and
calling `begin_main_usage` (`native_agent.rs:3254`). Add one call there:
`turn_client.restore_primary_route()`, which resets `active` to `0` unless
`cooldown_until` is set and unexpired. `resolve_active_route` therefore returns
the primary on the next turn once the cooldown has passed, and the pool's own
on-disk exhaustion TTL (reloaded by `send_main_request`'s next rotation) decides
whether the primary is actually viable again. When fallback activates on a
rate-limit or billing class, `commit_active_route` also sets `cooldown_until =
Instant::now() + window`, so the turn that switched, and near-future turns within
the window, stay on the fallback instead of hammering a cooling primary. The
exact window and which classes arm it are oracle items (section 12); Python uses
a per-provider cooldown plus a 5s exhausted cooldown
(`chat_completion_helpers.py:67`).

Reset and new conversation. The conversation client is cached under
`(home, session_id)` (`conversation_agent.rs:120`). `retire_session` /
`release_session` (`conversation_agent.rs:334`) drop the client, so a `/reset` or
a brand-new conversation rebuilds from config with `active = 0`, `cooldown_until
= None`, restoring the configured primary. No explicit teardown of fallback state
is needed because the state lives only on the client that is being dropped. An
explicit in-conversation model switch, when that path is ported, must reset the
cursor and re-snapshot, matching `agent_runtime_helpers.py:3470`; it is not part
of this checkpoint.

## 9. Usage attribution

Usage stays on the conversation client's Main bucket. `dispatch_main_turn`
returns `served_provider`, and the caller passes it to `provider_usage::from_response`
(tool path) and `forward_sse` (streaming path) as the provider label, while
`capture_usage` (`native_agent.rs:1847`) accumulates into `self.usage_state.main`
because `self.usage_bucket == UsageBucket::Main` (`native_agent.rs:1358`). This
matches Python attributing the call to the live swapped provider/model with a
`call_role` of `"fallback"` (`conversation_loop.py:3643`). Porting the explicit
`call_role` tag and the billing-route persistence
(`agent_runtime_helpers.py:3506`) is a follow-up; this checkpoint gets the token
accounting and the provider label right, which is the load-bearing part.

## 10. Cancellation, blocking I/O, concurrency

- No new blocking I/O. Route clients are built at startup. The only blocking work
  in the dispatch path is inside `send_main_request`, which already runs
  `credential_is_exhausted` and `rotate_after_failure` under
  `tokio::task::spawn_blocking` (`native_agent.rs:1630`, `:1663`). The seam adds
  only in-memory cursor reads and writes under a short mutex, never held across an
  await.
- Cancellation. A turn future can be dropped at any await. Between two routes,
  before the first send: cursor unchanged. After a route's `send_main_request`
  persisted a pool exhaustion but before the retry send: durable pool state is
  consistent (persist-before-retry is already guaranteed), and the cursor either
  still points at the failed route or has advanced, both consistent on the next
  turn. Mid-stream after a 200: the response already returned from the seam, no
  fallback is in flight, the partial stream is abandoned exactly as today.
- Concurrency. Turns of one conversation are serialized by the turn lease, so the
  cursor and `cooldown_until` are single-writer per conversation. Two
  conversations under one profile hold independent cursors but share the on-disk
  pool for the primary; each route's `send_main_request` loads its own
  request-local pool and writes atomically, the identical race the pool seam
  already tolerates. Fallback routes use static per-entry keys and touch no shared
  store, so they add no new cross-conversation race.

## 11. Retry and chain bounds

- Per route: `send_main_request` keeps its existing bound, at most one cheap 429
  same-key retry plus pool rotation, with the `attempts > 2` guard
  (`native_agent.rs:1591`).
- Across routes: `dispatch_main_turn` tries each chain entry at most once per
  dispatch, advancing the cursor, and returns the terminal error when the chain
  is exhausted. First success wins and sticks. This matches the Python "each
  candidate attempted once per turn, `_fallback_index` advances once per
  candidate" bound (`chat_completion_helpers.py:2739`).
- The Python per-call retry ceiling of 3 and the outer error cap of 8
  (`conversation_loop.py:371`) govern the surrounding conversation loop, not this
  seam. The transport-failure gate ("fall back only after two retries") depends on
  a retry count the current Rust `send_main_request` does not track; porting that
  counter is a prerequisite for treating `timeout`/`overloaded`/`5xx` as fallback
  triggers (section 12). For the rate-limit, billing, and auth classes, which
  fall back immediately, no counter is needed and this checkpoint is complete.

## 12. Ownership table

| State | Owner | Lifetime | Sharing | Mutability |
| --- | --- | --- | --- | --- |
| `main_fallback.fallbacks` | `NativeAgentClient` | build to eviction | cloned per turn (Arc, shared bytes) | immutable |
| `main_fallback.active` cursor | `Arc<Mutex<usize>>` | build to eviction | shared across all clones of one conversation client | interior; reset at turn start, set on switch |
| `main_fallback.cooldown_until` | `Arc<Mutex<Option<Instant>>>` | build to eviction | shared across clones | interior; set on rate/billing switch, read at turn start |
| Each fallback route client (key, base_url, client, headers, model, reasoning) | the route `NativeAgentClient` | build to eviction | cloned per turn | immutable static-key route |
| Primary `main_pool` cursor | `MainPoolCredential.active` | build to eviction | shared across clones | interior; unchanged by this seam |
| Conversation usage, prompt, tools, hooks, compression | `self` | build to eviction | shared/cloned as today | as today; route switch does not touch them |

Only the primary path writes durable store state; fallback routes are static keys
and never mutate the store in this checkpoint.

## 13. Red-first local HTTP integration tests

All drive the real `NativeAgentClient` turn entry (`run_turn` /
`run_turn_with_context`) or `build_agent_client_for_home` end to end, each against
axum test servers, with `secret_scope::with_secret_scope` for profile isolation
as the existing tests do. Each is written to fail against `d87226dbc9`, where the
main turn has no cross-provider fallback.

- Streaming fallback on billing. Primary provider returns 402 with a billing
  marker and has a single-key or exhausted pool; a `fallback_providers[0]` entry
  with its own base_url and key returns a normal stream. Assert the fallback
  server receives the entry's bearer, model, and its own provider header; the
  turn completes; and no `MessageChunk` was emitted before the switch. Fails
  today: the turn errors on the 402.
- Pool drains before fallback. Primary has two pooled keys, both return 429
  rate-limit; a fallback entry succeeds. Assert both primary keys are marked
  exhausted in the store before the fallback server is hit exactly once. Guards
  the pool-first ordering of section 5.
- Tool-loop fallback sticks across rounds. Round 1 succeeds on the primary,
  round 2's `step` returns 429 on a drained primary pool, a fallback serves round
  2; assert rounds 3 and later hit only the fallback server (cursor propagation
  across `step` calls) and the accumulated transcript is sent unchanged to the
  fallback. Fails today.
- Per-route body projection. Give the fallback entry a different `model` and its
  own `extra_body`; capture both request bodies. Assert the fallback request
  carries the fallback model and extra_body, the primary request carried the
  primary model, and the tool definitions are byte-identical across both. Guards
  the per-serving-client build of section 4.
- Chain order and dedup. `fallback_providers = [A, B]`, `fallback_model = [A
  duplicate, C]`; A fails eligible, assert B is tried next, and that the A
  duplicate and the main-provider entry are never built. Binds to the shared
  `main_fallback_chain` and `should_skip_main_fallback_provider`.
- Stickiness resets next turn. Turn 1 switches to a fallback on a rate-limit
  class; turn 2, after the injected cooldown clock has passed, re-attempts the
  primary first. With the cooldown still active, turn 2 stays on the fallback.
  Guards `restore_primary_route` and the cooldown gate of section 8.
- Chain exhausted preserves the error. Every route returns an eligible failure;
  assert the surfaced error equals today's `main_http_error` string for the last
  route and the turn fails, with each route hit at most once. Guards the final
  error shape and the per-dispatch bound.
- Streaming mid-body failure does not fall back. Primary returns 200 then a
  truncated SSE frame; assert no route switch, no cursor change, and no
  duplicated text. Guards the pre-body-only rule.
- No chain configured. A conversation with empty `fallback_providers` /
  `fallback_model`; assert a terminal primary failure fails the turn exactly as
  today with no route built. Guards the strict-subset boundary.
- Non-chat entry skipped at build. A `fallback_providers` entry with
  `api_mode = "anthropic_messages"`; assert it is skipped at build, logged, and
  never dispatched. Guards the deferred-transport boundary of section 14.
- Profile isolation. Two sessions under `red` and `blue` with different
  fallback keys for the same fallback provider name; both switch; assert each uses
  its own scoped key and neither sees the other's. Guards per-entry secret scope.

## 14. Prerequisites and what makes this checkpoint unsafe

- The agy contract oracle is not landed. The task files
  `main-provider-fallback-contract-agy.txt` and `-seam-claude.txt` are freshly
  created and untracked, and `main-provider-fallback-goldens.json` does not exist
  yet. Do not implement until that lane pins: (a) the exact fallback-eligible
  trigger set and whether `timeout`/`overloaded`/`5xx` require the two-retry gate,
  (b) whether a single-entry primary pool should skip the one doomed attempt
  (section 5), (c) the stickiness cooldown window and which classes arm it
  (section 8), (d) whether `fallback_model` legacy entries differ from
  `fallback_providers` in any per-entry handling beyond the shared parser, and
  (e) the same-backend skip semantics for the main turn (backend identity vs
  provider name) and whether it must match the compression `should_skip_candidate`
  scope logic rather than the coarser provider-name predicate.
- `MainTerminal` typing of `send_main_request`. The seam needs the failure class
  out of `send_main_request`, which today returns an opaque string. The typed
  terminal error plus its `From<MainTerminal> for Error` must land first and be
  proven to preserve the exact surfaced error string, or the eligibility gate has
  nothing reliable to branch on and would have to re-parse the body, which drifts
  from `main_pool_failure`.
- The transport-retry counter. Treating transport failures and 5xx as triggers is
  unsafe until a per-turn retry count exists, because firing fallback on the first
  timeout diverges from Python's two-retry gate. Ship the rate-limit, billing, and
  auth classes first; gate the transport class behind the counter.

## 15. Deferred scope, stated plainly

- OAuth and non-chat transports. Anthropic messages, Nous dual-wire,
  openai-codex Responses, Bedrock converse, and xai-oauth fallback entries are out.
  They participate in Python (`chat_completion_helpers.py:2828` onward) but require
  the Responses/messages transports and OAuth resolution the native client does
  not have. `build_main_fallback_client` gates on `chat_completions` and skips the
  rest at build, exactly as `with_provider_profile` already rejects them
  (`native_agent.rs:1438`).
- Per-entry credential pools and dynamic refresh. A fallback route uses a static
  per-entry key. Loading the fallback provider's own pool on switch
  (`chat_completion_helpers.py:2963`) and any dynamic credential refresh are
  deferred; the primary keeps its pool, the fallback does not get one this
  checkpoint.
- Dynamic plugins. Plugin-provided providers are not built here.
- System-prompt model-identity rewrite. The `_sync_failover_system_message` /
  `rewrite_prompt_model_identity` behavior (`conversation_loop.py:1743`,
  `chat_completion_helpers.py:3150`) is a content fidelity follow-up, not part of
  the routing seam.
- Explicit in-conversation model switch. Resetting the cursor and re-snapshotting
  the primary on a deliberate switch (`agent_runtime_helpers.py:3470`) is a
  separate lane; this checkpoint restores the primary only through turn-start
  reset and session retirement.

## Implementation order

1. Type the terminal failure. Make `send_main_request` return a `MainTerminal`
   carrying status and `MainPoolFailure`, with `From<MainTerminal> for Error`
   equal to today's `main_http_error` string. Prove the surfaced error is
   unchanged. No behavior change yet.
2. Extract `build_main_turn_client` from the primary build block
   (`main.rs:1124` to `:1209`) so the primary and fallback share provider-config
   resolution without auxiliary summary semantics.
3. Add `MainFallbackRoutes` and the `main_fallback` field, `with_main_fallback_routes`,
   `resolve_active_route`, `commit_active_route`, `restore_primary_route`, and the
   cooldown gate. Wire `restore_primary_route` into `run_native_turn`'s turn
   setup.
4. Add `dispatch_main_turn` and route both `run_model_turn` and
   `ChatModel::step` through it with per-serving-client body closures; thread
   `served_provider` into `forward_sse` and `capture_usage`. Start with the
   rate-limit, billing, and auth eligible classes only.
5. Build the plan at startup: iterate `main_fallback_chain`, skip via
   `should_skip_main_fallback_provider` and build errors, build each with
   `build_main_fallback_client`, install through `with_main_fallback_routes`.
6. Tests. Add the red-first integration tests of section 13; bind chain-order and
   dedup to the agy goldens once `main-provider-fallback-goldens.json` lands.
7. After the oracle lands, add the transport-retry counter and, gated on it, the
   `timeout`/`overloaded`/`5xx` triggers, then reconcile the cooldown window and
   the same-backend skip against the goldens.

Do not begin before the contract lane pins the trigger set, the stickiness
cooldown, and the same-backend skip semantics in section 14.
