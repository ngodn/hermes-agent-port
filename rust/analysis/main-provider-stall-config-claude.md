# Rust seam: timeout configuration and liveness ownership for the native main provider

Scope: where the request-timeout, connect-timeout, and streaming-stall values
that the client-rebuild seam
([main-provider-client-rebuild-seam-claude.md](main-provider-client-rebuild-seam-claude.md))
depends on actually come from. That document decided the *shape* of the stall
deadline and proved the client rebuild is optional. This document owns the
narrower question it deferred to section 10: what configures the deadline, what
the Rust port should do about it (preserve an environment input, add a
config.yaml key, freeze a constant, or defer), and how a timeout outcome reaches
the landed retry dispatcher.

This is a Rust ownership and configuration lane. The Python behavior contract and
its goldens belong to the agy lane. Live Rust symbols are anchored to the working
tree at HEAD `dd4611ec29` ("Port native stream length continuation"), which is
one commit past the seam document's `d4fd086a7d`, so a few native_agent.rs line
numbers here differ from that document by construction. Live Python is cited only
where a fact pins a Rust interface.

Not restated: the agy tool-truncation contract, and the length-continuation
boundary already drawn in the seam document section 4. This lane keys purely on
liveness (time since last byte) and never inspects `finish_reason`.

## 0. The two configuration surfaces, precisely

### 0.1 Python: a three-tier resolver plus environment fallbacks

Python resolves every main-provider timeout through the same precedence:
per-model config, then per-provider config, then an environment variable, then a
built-in default (with a reasoning floor spliced in for the stale timeouts).

- Config lookup module `hermes_cli/timeouts.py` (83 lines, pure lookup, no
  defaults or scaling of its own):
  - `_coerce_timeout(raw)` (`:4` to `:11`): `float(raw)`, returns `None` on
    `TypeError`/`ValueError` (`:7` to `:8`) and on any non-positive value (`:9`
    to `:10`, `if timeout <= 0: return None`). Zero, negative, and garbage all
    collapse to "unset".
  - `get_provider_request_timeout(provider_id, model=None)` (`:14` to `:40`):
    per-model `models.<model>.timeout_seconds` first (`:34` to `:38`), then
    per-provider `request_timeout_seconds` (`:40`). Returns `None` when nothing
    is configured. Any load exception returns `None` (`:24` to `:25`).
  - `get_provider_stale_timeout(provider_id, model=None)` (`:43` to `:69`): same
    shape over `stale_timeout_seconds`.
  - `_get_model_config` (`:72` to `:82`): reads `provider_config["models"][model]`.
- Effective request timeout, per call, `run_agent.py:_resolved_api_call_timeout`
  (`:1522` to `:1540`): per-model, then per-provider, then
  `env_float("HERMES_API_TIMEOUT", 1800.0)` (`:1540`), so the buffered path's
  real total timeout defaults to 1800.0s, not `None`. `get_provider_request_timeout`
  returning `None` only means "do not override at client-construction time";
  the per-call `timeout=` still resolves to 1800.0 (passed at
  `agent/chat_completion_helpers.py:2138`, `:2286`, `:2319`).
- Buffered inactivity watchdog, `HERMES_API_CALL_STALE_TIMEOUT` default 90.0
  (`run_agent.py:1564`, default at `:1581`), resolved per-model then per-provider
  then env then reasoning floor then 90.0 (`:1545` to `:1581`). This is a
  monitor-thread inactivity bound distinct from the 1800s total.
- Streaming stale base, `HERMES_STREAM_STALE_TIMEOUT` default 180.0, read at
  `agent/chat_completion_helpers.py:5469` (main path) and `:832` (Bedrock path).
  Config `stale_timeout_seconds` wins over env (`:5465` to `:5467`).
- Streaming stall scaling, `agent/chat_completion_helpers.py:5464` to `:5522`:
  local-endpoint 900.0 variant (`:5479` to `:5494`, config key
  `agent.local_stream_stale_timeout` default 900 at `hermes_cli/config_defaults.py:360`,
  env `HERMES_LOCAL_STREAM_STALE_TIMEOUT` at `:5494`); context scaling
  `>100_000 tokens -> max(base, 300.0)` (`:5506` to `:5507`),
  `>50_000 -> max(base, 240.0)` (`:5508` to `:5509`); reasoning-model floor
  `:5519` to `:5522`.
- Streaming give-up circuit breaker, `HERMES_STREAM_STALE_GIVEUP` default 5
  (`agent/chat_completion_helpers.py:807`): after N consecutive stale kills the
  monitor stops replaying and aborts. This is the bound on how many times a
  pre-visible stall re-enters the loop.
- httpx client timeouts (hardcoded, not configurable), `agent/process_bootstrap.py:568`:
  `httpx.Timeout(connect=15.0, read=None, write=15.0, pool=10.0)`. `read=None`
  is intentional; the stall monitor bounds read inactivity, not a read timeout.
  SDK retries forced to 0 (`run_agent.py:5965`) so they never compound the outer
  loop; the outer loop count is `agent.api_max_retries` default 3
  (`agent/agent_init.py:2136` to `:2143`).
- config.yaml surface, documented at `cli-config.yaml.example:226` to `:257`:
  `providers.<id>.request_timeout_seconds` (`:247`, `:250`),
  `providers.<id>.stale_timeout_seconds` (`:248`), per-model
  `models.<model>.timeout_seconds` (`:253`) and `stale_timeout_seconds` (`:257`).
  Config wins over env. Explicitly not wired for AWS Bedrock (`:242` to `:243`,
  boto3 owns its own timeouts).

### 0.2 Rust: no ownership path exists yet

- The main chat client has no timeout of any kind. All three builders call
  `reqwest::Client::builder().build()` with no `.timeout()`, no
  `.connect_timeout()`, no `.read_timeout()`, no pool tuning:
  `fresh_main_http_client` (`native_agent.rs:1166` to `:1170`),
  `NativeAgentClient::new` (`:1878` to `:1880`), and `with_runtime_credential`
  (`:1934` to `:1936`).
- `send_main_request` (`native_agent.rs:2440`) posts at `:2467` to `:2474` with
  no `.timeout()`. The only `.timeout()` in the file is the summary call at
  `:2944`.
- `forward_sse` (`native_agent.rs:4978`) drives
  `while let Some(chunk) = stream.next().await` (`:4996`) with no
  `tokio::time::timeout` anywhere in the file.
- No config struct carries a timeout field. The config structs are `Config`
  (`config.rs:12`, env-built in `Config::from_env`, not serde), `PlatformConfig`
  (`config_types.rs:356`), `StreamingConfig` (`config_types.rs:515`), and
  `GatewayConfig` (`config_gateway.rs:219`). A grep across all `config*.rs` finds
  no field named or containing `request_timeout`, `read_timeout`,
  `connect_timeout`, `stale`, `stream_stale`, `pool`, `max_retries`, or
  `api_max_retries`. The only nearby timeout is
  `GatewayConfig.loop_watchdog_probe_timeout_s` (`config_gateway.rs:241`, default
  10.0 at `:63`), an unrelated liveness probe.
- No environment variable in this domain is read in Rust. A grep for
  `HERMES_*` intersected with `TIMEOUT|STALE|RETRY|POOL` returns zero.
  `HERMES_STREAM_STALE_TIMEOUT`, `HERMES_API_TIMEOUT`, and
  `HERMES_API_CALL_STALE_TIMEOUT` are read nowhere.
- The one config value that already flows into the main client through a raw
  `user_config` JSON path is the retry count: `main.rs:1339` reads
  `&user_config["agent"]["api_max_retries"]` and passes it to
  `native_agent::main_retry_attempts` (`native_agent.rs:1456`, defaults to 3 when
  `Null` at `:1458`), wired via `with_main_retry_attempts` (`native_agent.rs:2083`).
  This is the existing, proven pattern for reading a scalar out of the untyped
  user config without a schema change, and it is the model this lane should
  follow.
- `http_client_limits.rs` is a red herring for this lane. It is a port of
  `gateway/platforms/_http_client_limits.py`, carries `#![allow(dead_code)]`
  (`:5`), models only the two pool keep-alive knobs
  (`max_keepalive_connections` default 10, `keepalive_expiry_s` default 2.0,
  `:28` to `:29`), reads only `HERMES_GATEWAY_HTTPX_MAX_KEEPALIVE` /
  `HERMES_GATEWAY_HTTPX_KEEPALIVE_EXPIRY` (`:94`, `:98`), and is applied to no
  client anywhere. It does not model connect/read/write/pool timeouts. Its one
  contribution to this lane is a reusable coercion precedent: `env_float`
  (`:55` to `:67`) and `env_int` (`:74` to `:84`) already reproduce Python's
  `_env_float` / `_env_int` (trim, empty or unparseable or non-positive falls
  back to default), tested at `:144` to `:188`. A Rust timeout env bridge should
  reuse that exact coercion so the `> 0` gate matches Python `_coerce_timeout`.

## 1. The ownership decision, per knob

The task is to decide, for each configurable value the stall seam needs, whether
Rust should preserve an environment-only compatibility input, add a config.yaml
key, freeze a constant, or defer. The decision differs by knob because their
parity risk differs.

| Knob | Python source | Python default | Rust recommendation |
| --- | --- | --- | --- |
| Buffered total request timeout | `HERMES_API_TIMEOUT`, per-model, per-provider | 1800.0s | Freeze 1800.0s constant, plus read `HERMES_API_TIMEOUT` as an env compatibility input. Config keys deferred. |
| Connect timeout | hardcoded in httpx | 15.0s | Freeze 15.0s constant. No env, no config (Python does not expose it either). |
| Streaming stall base | `HERMES_STREAM_STALE_TIMEOUT`, per-provider, per-model | 180.0s | Freeze 180.0s constant, plus read `HERMES_STREAM_STALE_TIMEOUT` as an env compatibility input, plus an injectable test hook. Context/reasoning/local scaling deferred. |
| Streaming give-up count | `HERMES_STREAM_STALE_GIVEUP` | 5 | Freeze the replay ceiling as a constant (reuse the empty-stream ceiling of 3, or a dedicated 5). Env optional. |
| Buffered inactivity watchdog | `HERMES_API_CALL_STALE_TIMEOUT` | 90.0s | Defer. No clean reqwest analog; the total request timeout already bounds a buffered hang (section 3). |
| Read/write timeout | hardcoded `read=None`, `write=15.0` | none / 15.0s | Do not set a read timeout (parity with `read=None`; the stall deadline is the analog). Write timeout is moot for reqwest. |
| Pool wait timeout | hardcoded `pool=10.0` | 10.0s | Moot. reqwest does not expose a pool-acquire timeout knob; hyper dials a fresh connection instead. |
| Context / reasoning / local scaling | `chat_completion_helpers.py:5464-5522` | 240/300/900 + floor | Defer to the broader config port and the agy oracle (section 4). |
| Per-provider / per-model config keys | `cli-config.yaml.example:226-257` | unset | Defer to the broader config port; the `user_config` JSON-path pattern (`main.rs:1339`) can carry them later with no schema change. |

Recommended posture in one line: freeze the Python defaults as constants, add the
existing environment variables back as optional compatibility inputs using the
`http_client_limits.rs` coercion, and defer both the config.yaml schema keys and
the context/reasoning/local scaling to a later broader config port. Rationale:

1. Correctness (turning an unbounded hang into a bounded, replayable transport
   failure) needs only a sane bound, not per-provider tuning. The frozen Python
   defaults are that bound.
2. Operators may already set `HERMES_API_TIMEOUT` and
   `HERMES_STREAM_STALE_TIMEOUT` in their deployment. Reading them back is one
   `env_float` call per knob and preserves that muscle memory at near-zero cost,
   with the coercion already written and tested in-tree.
3. Adding a single typed config.yaml key ahead of the `providers.<id>` schema
   port would create a divergent partial surface (one timeout key present, the
   sibling keys absent) that a later schema port would have to reconcile. The
   `user_config` JSON-path read already used for `api_max_retries` is the
   lower-risk seam for per-provider timeouts when that port lands.
4. The context/reasoning/local scaling only ever *lengthens* the deadline
   (`max(base, ...)`), so a flat frozen base is conservative but safe: it can
   only trip sooner, never later. The one parity risk this leaves is a
   legitimately slow reasoning or local model that goes fully silent past 180s;
   section 4 bounds that risk and hands the exact scaling to the agy oracle.

## 2. First-byte versus inter-chunk semantics

Python uses one deadline for both gaps. `last_chunk_time` is reset on every real
chunk, and the trip is `time.time() - last_chunk_time["t"] > _stream_stale_timeout`
(`agent/chat_completion_helpers.py:5651` to `:5652`, reset at `:5697`). Because
the clock starts when the stream opens and resets on every byte, the same
threshold bounds the first-byte gap (time from request send to first SSE frame)
and every inter-chunk gap after it. There is no separate time-to-first-byte
constant on the main OpenAI/Anthropic path; the dedicated TTFB watchdog
(`HERMES_CODEX_TTFB_TIMEOUT_SECONDS`, `chat_completion_helpers.py:1657`) is
codex-only and out of this lane.

The Rust port must keep this single-deadline shape:
`tokio::time::timeout(stale, stream.next())` inside the `forward_sse` loop
(`native_agent.rs:4996`), with the timeout re-armed on every returned chunk. That
one wrapper covers both the pre-first-byte silence and every subsequent
inter-chunk silence, exactly as Python's single `last_chunk_time` does. It is a
deadline on `next()`, not a total read timeout on the whole stream, so a long
legitimate answer with steady bytes is never truncated.

For the buffered `step` path there is no inter-chunk notion; the body is consumed
whole. A single total `.timeout(request_timeout)` on the `send_main_request` post
(`native_agent.rs:2467`) is the correct and complete bound there.

## 3. How a timeout outcome reaches the retry dispatcher

Two entry points, both already landed, neither needing a classifier change.

Buffered path. A reqwest `.timeout()` elapse surfaces as a send error on the
`send_main_request` post (`native_agent.rs:2477` to `:2491`). That arm already
maps every send error through `main_transport_retry_failure`
(`native_agent.rs:1339` to `:1354`), which returns `MainPoolFailure::Transport`
for everything except a certificate-verification failure. A reqwest timeout
error chain contains no certificate marker, so it classifies as `Transport`,
retries under `main_attempt_limit` (`:2484`) with `wait_before_main_retry`
(`:2485` to `:2488`), and then returns `MainRequestError::Fallback` (`:2490`)
which activates cross-provider fallback. No change to the classifier is required;
a red test should pin this so a future edit to `main_transport_retry_failure`
cannot silently drop timeout classification. Note this is distinct from the
existing HTTP 408 status path: `main_retry_failure` maps a
`REQUEST_TIMEOUT` *status* to `Transport` at `native_agent.rs:1372` to `:1373`,
which is what `request_timeout_status_uses_transport_fallback_threshold`
(`native_agent.rs:6584`) and `pooled_request_timeout_status_bypasses_credential_rotation`
(`native_agent.rs:6635`) already cover. A client-side stall is the absence of a
status, so it takes the send-error arm, not the status arm.

Streaming path, pre-visible versus post-visible. This is the load-bearing
distinction and it decides the outcome:

- Pre-visible stall (no `MessageChunk` emitted yet, the turn's visible flag is
  still false). The deadline elapse must return a distinct retryable stall
  outcome that the caller treats like the existing empty-stream branch. The
  empty-stream replay already lives in the main turn loop, not in `forward_sse`:
  `empty_stream_attempts` counter at `native_agent.rs:4242`, replay branch at
  `:4355` to `:4378` (replay while `empty_stream_attempts < 3`, then
  `activate_main_success_body_fallback`). A pre-visible stall joins that branch:
  replay the route under a bounded ceiling, then fall back. Nothing durable or
  visible has crossed the boundary, so replay is safe. The give-up ceiling
  (Python's `HERMES_STREAM_STALE_GIVEUP` default 5) maps directly onto this
  counter; the minimal port reuses the empty-stream ceiling of 3 unless the agy
  oracle pins the exact count.
- Post-visible stall (a `MessageChunk` already emitted). The deadline elapse must
  return `Err` and fail the turn with no retry and no fallback, because a replay
  would double-emit already-visible output. This preserves the seam document's
  section-1 post-delta invariant. Dropping the timed-out `bytes_stream()` future
  drops the connection cleanly (seam document section 2), which is the Rust
  equivalent of Python cancelling the stream attempt.

The stall deadline never invents a new dispatch route. Pre-visible reuses the
empty-stream replay-then-fallback route; the buffered timeout reuses the
send-error Transport route; post-visible reuses the existing fail-the-turn arm.

## 4. Scaling deferral and its one parity risk

Deferred to the broader config port and the agy oracle, stated so the deferral is
explicit and bounded:

| Python scaling | Site | Effect | Deferral note |
| --- | --- | --- | --- |
| `>50_000` tokens -> `max(base, 240.0)` | `chat_completion_helpers.py:5508` | lengthen | needs the not-yet-ported context-token estimator |
| `>100_000` tokens -> `max(base, 300.0)` | `:5506` | lengthen | same estimator |
| reasoning-model floor | `:5519` to `:5522` | lengthen | needs the reasoning-floor table |
| local endpoint -> 900.0 | `:5479` to `:5494` | lengthen | needs `is_local_endpoint` parity and the local config key |

Every one of these can only raise the deadline above the 180.0 base. Freezing the
base therefore trips no earlier than Python for a normal cloud model and only
risks a *false* early trip for a reasoning or local model that stays fully silent
past 180s. Three facts bound that risk to acceptable for a first cut: the deadline
resets on every byte, and reasoning and streaming models almost always emit
thinking or keepalive bytes inside 180s; a false trip is a *replay*, not a hard
failure, because it re-enters the pre-visible replay branch; and the give-up
ceiling still terminates a genuinely dead stream. The agy oracle owns the final
call on whether parity requires porting the reasoning floor and local variant in
the first cut or whether the conservative flat base is acceptable, and on the
exact replay ceiling (3 versus 5).

## 5. Cache and concurrency implications

- The connect and total request timeouts are baked into the reqwest client at
  build time, so they must be applied in all three builders
  (`native_agent.rs:1166`, `:1878`, `:1934`) and in any future rebuild through a
  single shared helper (for example `with_request_timeouts(builder)`), or a
  credential-rotation rebuild via `install_replacement` would silently produce a
  client with no timeouts. The helper is pure and shares nothing; the builder
  swap already happens under the `active` mutex (seam document section 2), so no
  new synchronization is introduced.
- The stall deadline is per-stream local state (one `Instant` re-armed in the
  `forward_sse` loop). It shares nothing, needs no mutex, and holds no lock across
  an await, matching the invariant the seam document section 2 requires.
- The replay counter for a pre-visible stall is per-turn local state, exactly like
  the existing `empty_stream_attempts` (`native_agent.rs:4242`). It is not a
  shared or cached field and has no cross-turn or cross-session lifetime.
- Configuration caching. Python re-derives the timeout on every call, so a live
  config or env change takes effect on the next call. The Rust recommendation
  captures the frozen constants and env inputs once, at client construction, for
  two reasons: env is process-global and reading it per request in the hot path
  is both wasteful and racy against tests that mutate env, and the native path
  never live-reloads config mid-session anyway. The only observable divergence is
  under a hypothetical live env change mid-session, which the native path does not
  support, so it is not a real parity gap. This is why the values must be
  injectable through builder hooks rather than read from env inside the stream
  loop (section 6).

## 6. Public-seam red tests

Same public seam and harness as the retry lane: axum servers on `127.0.0.1:0`, a
`TempHome` pool, and a direct `dispatch_main_turn` / `run_turn` call, asserting on
`main_fallback.state` and the servers' received requests. The configuration
concern this lane adds is injectability: the deadlines must be settable in
milliseconds from a test, in the style of the existing `with_main_retry_backoff`
hook (`native_agent.rs:2101`) and `with_main_retry_attempts` (`:2083`). Neither
`with_stream_stale_timeout` nor a `with_request_timeout` hook exists today (grep
returns zero), so both are prerequisites for the tests below. Each test is written
to fail at HEAD `dd4611ec29`.

- Buffered `step` hang classifies as transport and fails over. Primary `step`
  accepts the connection and never responds; fallback serves. Assert the injected
  request timeout fires, the primary is retried under budget, and the fallback
  serves the round. Fails today because `send_main_request` sets no `.timeout()`
  (`native_agent.rs:2467`).
- Timeout error classifies as `Transport`, guard test. Feed
  `main_transport_retry_failure` (or the send-error arm) a reqwest timeout error
  chain and assert it returns `MainPoolFailure::Transport`, so a future edit to
  the certificate-only allow list at `native_agent.rs:1341` to `:1353` cannot
  silently strip timeout retryability.
- Pre-visible stream stall replays then fails over. Primary returns 200 then holds
  the socket open emitting only SSE comment keepalives past the injected deadline;
  live fallback server. Assert the deadline fires, the primary is attempted up to
  the replay ceiling, the fallback serves, and no `MessageChunk` was ever emitted
  from the primary. Fails today because `stream.next().await`
  (`native_agent.rs:4996`) blocks forever.
- Post-visible stream stall fails the turn without fallback. Primary sends one
  delta then goes silent past the deadline. Assert the turn errors, the cursor is
  unchanged, no second request reaches any server, and no duplicated
  `MessageChunk`. Guards the replay barrier.
- Give-up ceiling caps replays. Primary stalls pre-visibly on every attempt.
  Assert the number of primary attempts equals the frozen ceiling and then
  fallback activates, so a persistently dead stream cannot loop unbounded.
- Environment compatibility coercion. Mirror the `http_client_limits.rs` env
  tests (`:144` to `:188`): `HERMES_STREAM_STALE_TIMEOUT` and `HERMES_API_TIMEOUT`
  present and positive are used; empty, unparseable, zero, and negative fall back
  to the frozen default. This pins the `> 0` gate to Python `_coerce_timeout`.
- Connect refusal already replays then fails over, kept green as a guard. Bind
  then drop a listener so the port refuses; assert budget attempts then fallback.
  This should already pass and pins that the timeout wiring does not regress the
  landed connect path.

## 7. Deferred scope, stated plainly

- Per-provider and per-model config.yaml keys (`request_timeout_seconds`,
  `stale_timeout_seconds`, `timeout_seconds`, `cli-config.yaml.example:226` to
  `:257`). Deferred to the broader config port. When it lands, the untyped
  `user_config` JSON-path read already used for `api_max_retries`
  (`main.rs:1339`) is the low-risk carrier, not a new typed field ahead of the
  schema.
- Context-token and reasoning-model stall scaling and the local-endpoint 900s
  variant (section 4). Deferred to the agy oracle and the config port.
- Buffered inactivity watchdog (`HERMES_API_CALL_STALE_TIMEOUT`, 90.0s). No clean
  reqwest analog for a buffered response; the total request timeout bounds the
  hang. Not ported in the first cut.
- Codex TTFB watchdog (`HERMES_CODEX_TTFB_*`, `chat_completion_helpers.py:1609`
  to `:1696`). codex-responses api_mode only; the native Rust client is
  chat-completions only. Out of this lane.
- Auxiliary and summary timeouts. The summary path already owns
  `summary_timeout` (field `native_agent.rs:1831`, default 300s at `:1900`,
  applied at `:2944`, wired from the compression/auxiliary config via
  `with_summary_request_policy` at `main.rs:891` and `:1379`,
  `compression_auxiliary.rs` floor 300s). It does not govern the main chat request
  and is not folded into the main-turn deadline.
- OAuth and credential-rotation client rebuilds. Nous OAuth recovery and pool
  credential rotation rebuild the reqwest client through `install_replacement` on
  their own landed lanes; this lane only requires that the shared
  `with_request_timeouts` helper (section 5) is applied there too, so a rebuilt
  client is not born without timeouts. No other change to those paths.
- Non-chat transports (anthropic-messages, MoA, Bedrock, codex). Bedrock is not
  timeout-wired in Python either (boto3 owns it, `cli-config.yaml.example:242` to
  `:243`); the native Rust client rejects non-chat api_modes. No transport trait,
  no non-chat timeout config here.
- The client rebuild itself. Optional per the seam document section 3, owned by
  the retry-budget seam if the oracle requires the extra post-budget attempt, not
  this lane.

## 8. Prerequisites before implementation

- A shared `with_request_timeouts`-style builder helper applied at
  `native_agent.rs:1166`, `:1878`, `:1934`, freezing connect 15.0s and total
  1800.0s, reading `HERMES_API_TIMEOUT` as an env override through the
  `http_client_limits.rs` `env_float` coercion.
- A `.timeout(request_timeout)` on the `send_main_request` post
  (`native_agent.rs:2467`) and a `with_request_timeout(Duration)` test hook.
- A `tokio::time::timeout(stale, stream.next())` wrapper in `forward_sse`
  (`native_agent.rs:4996`) with a re-armed clock, a `with_stream_stale_timeout(Duration)`
  test hook, and the pre-visible outcome joined to the empty-stream replay branch
  (`native_agent.rs:4355` to `:4378`).
- The agy oracle to pin: the exact stall base and whether the context and
  reasoning-model scaling and the local 900s variant are required in the first cut
  or the conservative flat 180.0 base is acceptable (section 4), and the
  pre-visible replay ceiling (3 versus 5).

## 9. Commands run

All read-only.

- `git log --oneline -1` (HEAD `dd4611ec29`).
- `find` for `native_agent.rs`, `config*.rs`, `http_client_limits.rs` under
  `rust/crates`.
- `grep -rn` over `rust/crates/hermes-gateway/src/config*.rs` and
  `config_env_overrides.rs` for `stale`, `request_timeout`, `read_timeout`,
  `connect_timeout`, `max_retries` (no timeout fields found).
- `grep -n` over `native_agent.rs` for `summary_timeout`, `.timeout(`,
  `connect_timeout`, `HERMES_STREAM`, `request_timeout`, and the request-timeout
  status tests.
- `Read` of `http_client_limits.rs` (full), `native_agent.rs` ranges 1301-1375,
  2140-2200, 6584-6644.
- Two read-only sub-agent sweeps: one over the Python config plumbing
  (`hermes_cli/timeouts.py`, `run_agent.py`, `agent/chat_completion_helpers.py`,
  `agent/process_bootstrap.py`, `cli-config.yaml.example`,
  `hermes_cli/config_defaults.py`), one over the Rust config schema and native
  client construction (`config*.rs`, `native_agent.rs`, `main.rs`,
  `http_client_limits.rs`, `compression_auxiliary.rs`).

## 10. Primary-lane disposition

This report freezes the ownership evidence, not the final public configuration
decision. The project rule against new environment-only behavioral settings
means the existing `providers.<id>.request_timeout_seconds`,
`providers.<id>.stale_timeout_seconds`, and per-model timeout keys must be the
user-facing authority when this seam is implemented. Existing Python
environment variables may remain as compatibility bridges behind that config
precedence, but cannot become the only documented tuning surface.

The next checkpoint must also source-execute the exact stream-stall replay
ceiling and context/reasoning/local scaling before choosing between the flat
base and the longer Python deadlines. Those values remain deliberately
unimplemented here.
