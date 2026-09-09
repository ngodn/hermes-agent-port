# Rust seam: ordinary main-turn transport failure and retry fallback

Scope: map the narrowest safe ownership seam for the part of the ordinary
main-turn path that is still unwired, the transport layer. Connection failures,
request and read timeouts, HTTP 408, provider overload (503/529), server errors
(500/502/504/524), unknown non-success statuses, malformed HTTP 200 bodies,
missing or empty `choices`, and content-policy refusals. Everything up to and
including credential-pool rotation plus cross-provider fallback on auth, billing,
and rate-limit classes has already landed at `0beda20d68` ("Port native main
provider fallback"). This lane is the next increment and it must not rebuild any
of that.

This is a Rust ownership and interface lane. The Python behavior contract and its
goldens belong to the parallel agy lane (`main-provider-retry-contract-agy.md`,
`gen_main_provider_retry_goldens.py`, `main-provider-retry-goldens.json`, none
landed yet, the task files are freshly created and untracked). This document does
not re-derive that contract. It cites the live Python only where a fact pins a
Rust interface, and every Rust symbol is anchored to code readable in the working
tree at `0beda20d68`. What needs the oracle before it is safe is in section 8.

## 0. What exists today, precisely

Two nested seams already own the two axes the earlier lanes built:

- `send_main_request` (`native_agent.rs:1924`) owns per-route dispatch and
  within-provider credential rotation. It loops over pool keys, gives an ordinary
  429 one cheap same-key retry (`native_agent.rs:1995` to `:2019`), bounds each
  `(credential_id, api_key)` to two attempts (`native_agent.rs:1949`), persists an
  exhaustion before rotating, and returns either a success `reqwest::Response` or
  a `MainRequestError`.
- `dispatch_main_turn` (`native_agent.rs:1884`) owns cross-provider fallback. It
  reads the sticky `MainFallbackState.active` cursor (`native_agent.rs:1350`),
  builds the body for the serving route through the caller's closure, calls
  `send_main_request`, and on a `Terminal` whose class
  `activates_provider_fallback()` is true (`native_agent.rs:916`) advances to the
  next eligible route via `activate_main_fallback` (`native_agent.rs:1837`). Both
  transports enter here: streaming `run_model_turn` (`native_agent.rs:3619`) and
  the tool round `ChatModel::step` (`native_agent.rs:4146`).

The failure taxonomy is `MainPoolFailure` (`native_agent.rs:906`): `Auth`,
`Billing`, `BillingUnverified`, `RateLimit`, `UpstreamRateLimit`, `Unrelated`.
`activates_provider_fallback()` is true for everything except `Unrelated`.
`arms_primary_cooldown()` (`native_agent.rs:927`) is true for the rate and billing
family and drives the exponential primary cooldown in `activate_main_fallback`
(`native_agent.rs:1855`).

So the transport layer is exactly what is missing, and it is missing in three
distinct ways:

1. Transport errors never reach classification. A `reqwest` send error at
   `native_agent.rs:1964` is mapped to `Error::Other` and returned with `?`,
   which converts through `From<Error> for MainRequestError`
   (`native_agent.rs:953`) into `MainRequestError::Internal`. `dispatch_main_turn`
   handles `Internal` with the catch-all `Err(error) => return
   Err(error.into_error())` (`native_agent.rs:1919`). No retry, no rotation, no
   fallback. A single TCP reset or a slow provider fails the whole turn.
2. Overload and server statuses are terminal and inert. `main_pool_failure`
   (`native_agent.rs:1084`) maps `429` carrying "overloaded" or "at capacity" to
   `Unrelated` (`native_agent.rs:1189`), and maps 500/502/503/504/408/524 and
   every other status through the final `_ => Unrelated` arm
   (`native_agent.rs:1205`). `Unrelated` neither rotates in `send_main_request`
   (`native_agent.rs:1982`) nor activates fallback in `dispatch_main_turn`. These
   are precisely the classes Python retries and then fails over.
3. There is no timeout and no backoff. Both the pooled client
   (`fresh_main_http_client`, `native_agent.rs:1056`) and the plain client
   (`NativeAgentClient::new`, `native_agent.rs:1431`) are bare
   `reqwest::Client::builder().build()` with no `connect_timeout`, no read
   timeout, no total timeout. `retry_utils.rs` already carries
   `jittered_backoff` (`retry_utils.rs:186`), `parse_retry_after_seconds_*`, and
   the Z.AI overload schedule, all golden-tested, and none of it is called from
   the main path (the only caller outside `retry_utils` is `signal_rate_limit.rs`).

There is also a post-body gap that is separate from transport but shares the same
"is this replayable" question. The tool round decodes with `.json()` and then
requires `choices[0].message` (`native_agent.rs:4185`), returning a terminal
`Error::Other("native agent step: no choices[0].message")` on an empty or
malformed 200. The streaming path is worse: a non-success-shaped but 200 body such
as `{"choices":[]}` runs through `forward_sse`, emits no delta, sends
`MessageStop`, and returns `Ok(None)` (`native_agent.rs:4121`), so a malformed 200
silently completes the turn with no text and no error. Neither path detects a
content-policy refusal at all.

## 1. What the Python ordinary retry path forces onto the interface

Cited to the live path for grounding, values owned by the agy lane. The relevant
loop is `agent/conversation_loop.py:3328` onward.

1. Retry budget. `max_retries = agent._api_max_retries`
   (`conversation_loop.py:3329`), default 3 (`agent_init.py:2138`,
   `_agent_section.get("api_max_retries", 3)`). The Rust path has no such config
   key and no per-turn retry counter; the only bound today is the pool's
   `attempts > 2` per credential (`native_agent.rs:1949`), which is a different
   axis.
2. Class-specific fallback threshold. `is_rate_limited` (rate_limit, billing,
   upstream_rate_limit) falls back immediately, subject to the pool-recovery gate
   (`conversation_loop.py:6000`, `:6040`). `_is_transport_failure` (timeout,
   overloaded) falls back only after `retry_count >= 2`
   (`conversation_loop.py:6020`, `:6038`), that is two failed requests and one
   same-route retry before fallback.
   server_error and unknown go through the generic tail: backoff and retry to
   `max_retries`, then one primary-transport rebuild
   (`_try_recover_primary_transport`, `conversation_loop.py:7022`), then fallback
   (`conversation_loop.py:7017` to `:7045`).
3. Malformed or empty 200 is eager-fallback-first. An invalid response bumps
   `retry_count` and, if a fallback exists, switches immediately with
   `retry_count = 0` (`conversation_loop.py:3844` to `:3858`); only with no
   fallback does it back off and retry the same route
   (`jittered_backoff(retry_count, base_delay=5.0, max_delay=120.0)`,
   `conversation_loop.py:3949`), and at `retry_count >= max_retries` it fails over
   or terminates (`:3921`).
4. Content-policy refusal (HTTP 200, `finish_reason == "content_filter"` or a
   populated `message.refusal`) is never retried. It tries a configured fallback
   once, otherwise surfaces the refusal terminally
   (`conversation_loop.py:4060` to `:4144`).
5. Retry counter resets after any fallback activation, and fallback stays sticky
   through later tool rounds. Every `_try_activate_fallback()` success sets
   `retry_count = 0` and rebuilds (`:3854`, `:3928`, `:4106`, `:7042`).
6. Backoff is interruptible and touches activity every 30s so the inactivity
   monitor does not kill a turn during a wait (`conversation_loop.py:3954` to
   `:3989`).
7. Timeout is configured per attempt and, for transport failures, the client is
   rebuilt once before giving up (stale connection pool, TCP reset,
   `conversation_loop.py:7022`).

## 2. Q1. Which module and interface owns same-route retry versus cross-provider fallback

Keep the two existing seams and split the new work along the same axis. Do not add
a third loop and do not introduce a transport abstraction; there is one wire shape
here (chat-completions) and the codebase rule against a broad abstraction without a
second real adapter applies directly (Anthropic messages, Responses, and Bedrock
are deferred, section 9).

- Same-route retry and the retry budget belong inside `send_main_request`. It is
  already the single per-route dispatcher, it already owns attempt counting and
  persist-before-retry, it is strictly pre-body (it returns before any byte of the
  answer is consumed), and it is the only place a `reqwest` send error is
  observable. Transport, overload, and server retries with backoff live here,
  bounded by an `api_max_retries` budget carried on the client. The critical
  design point: `send_main_request` only surfaces a `Terminal` transport class
  after it has exhausted its same-route budget. That is how the "fall back only
  after two failures" rule of `conversation_loop.py:6038` is enforced without
  `dispatch_main_turn` knowing anything about counters.
- Cross-provider fallback stays in `dispatch_main_turn`. The only change it needs
  is that the new transport and server classes report
  `activates_provider_fallback() == true`, so that once `send_main_request` gives
  up on a route, the cursor advances. Because the budget is spent below, the
  threshold semantics are already correct: an immediate-fallback class (rate,
  billing) surfaces terminal on the first failure, and a retry-first class
  (transport, overload, server) surfaces terminal only after its budget.
- The post-body cases (malformed 200, empty choices, refusal) cannot live in
  `send_main_request` because that returns before the body is read, and they
  cannot live in `dispatch_main_turn` as written because it hands back a raw
  `reqwest::Response`. The narrowest home is a validator the caller passes into
  `dispatch_main_turn`: for the non-streaming `step` path the body is fully
  buffered by `.json()`, so a `Fn(&Value) -> Option<InvalidResponseClass>`
  inspected before the response is accepted lets dispatch treat a bad 200 as a
  synthetic terminal and either advance the cursor (eager fallback) or, when the
  chain is exhausted, retry the same route under budget. The streaming path passes
  no validator because it cannot buffer; see section 3 for why that is the correct
  and safe asymmetry. This validator is an extension of the one existing seam, not
  a new loop or a new abstraction.

Concretely, the seam grows one enum and one field, nothing structural:

- Extend `MainPoolFailure` (or add a sibling pre-body class it composes with) with
  `Transport`, `Overloaded`, and `ServerError`. `main_pool_failure` learns
  503/529 to `Overloaded`, 500/502/504/524 and 408 to `ServerError`, and the 429
  "overloaded"/"at capacity" arm moves from `Unrelated` to `Overloaded`. The
  `reqwest` send error at `native_agent.rs:1964` is caught (not `?`-propagated),
  its `is_timeout()`/`is_connect()`/`is_request()` inspected, and mapped to
  `Transport`.
- Add the retry budget and backoff inside `send_main_request`'s existing loop,
  reusing `retry_utils::jittered_backoff` and `parse_retry_after_seconds_header_map`
  rather than a new helper. The budget count is per dispatch, tracked in the same
  local bookkeeping style as the existing `attempts` map.
- `activates_provider_fallback()` returns true for `Transport`, `Overloaded`, and
  `ServerError`, keeping `Unrelated` (genuine 4xx like 400 malformed request that
  no other provider will accept differently) terminal.

## 3. Q2. What is safe to replay before body consumption, and what is not

The replay barrier is body consumption plus visible effects, and the two
transports sit on opposite sides of it, which is why the seam is shaped the way it
is.

Safe to replay, because nothing visible has happened yet:

- A `reqwest` send error before any response (connection refused, reset, connect
  or request timeout). No status, no bytes, no tool executed.
- Any non-success HTTP status. Auth, billing, rate, overload, server, and unknown
  all arrive as a status line before the answer body streams and before any tool
  round runs. This is the region `send_main_request` and `dispatch_main_turn`
  already operate in, and it is why pool rotation and provider fallback are
  correct today for the classes they cover.
- A fully buffered non-streaming 200 whose decoded body is invalid, empty in
  `choices`, or a refusal, detected inside `step` before `parse_message_step`
  returns and before the tool loop dispatches any call. The round has produced no
  durable transcript entry and executed no tool, so replaying it or falling over
  is safe. This is the validator case.

Not safe to replay, because a visible effect has already occurred:

- Streaming after the first `MessageChunk`. `forward_sse` emits deltas
  incrementally (`native_agent.rs:4084`), so once any text has been sent to the
  caller a replay double-emits. A mid-stream truncation or error after deltas
  must fail the turn, not retry and not fall back. The seam guarantees this simply
  by giving the streaming path no post-body validator: `dispatch_main_turn` only
  ever retries or fails over on the pre-body status, and the moment `forward_sse`
  starts consuming `bytes_stream()` the seam has already returned.
- A tool round after its tool calls have executed and been appended to the durable
  transcript. Detection therefore has to happen inside `step` before it returns
  the `Step`, which is exactly where the buffered `.json()` already sits.
- A read failure during `.json()` on a 200 (the send succeeded, the body read
  failed) is a gray case. No tool has run for `step`, so it is technically
  replayable, but it means re-issuing the request, which for a non-idempotent
  billed call the contract lane should confirm before enabling. Today it is a
  terminal `Error::Other("native agent step decode")` (`native_agent.rs:4179`);
  keeping it terminal is the safe default until the oracle says otherwise.

## 4. Q3. How each concern should be represented

| Concern | Representation | Where it lives |
| --- | --- | --- |
| Transport failure (connect/reset/timeout) | New `MainPoolFailure::Transport`, mapped by catching the `reqwest` error at `native_agent.rs:1964` and reading `is_timeout`/`is_connect`/`is_request` | classify in `send_main_request`; retryable, then fallback-eligible |
| Overload | `MainPoolFailure::Overloaded` for 503/529 and the 429 overloaded arm (`native_agent.rs:1189`) | `main_pool_failure`; retry with backoff (honor `Retry-After`), then fallback after budget |
| Server status | `MainPoolFailure::ServerError` for 500/502/504/524/408 | `main_pool_failure`; retry with backoff, then fallback at budget, mirroring the Python generic tail |
| Malformed 200 / empty `choices` | Post-decode validator returning an invalid-response signal, checked in `step` before `parse_message_step` | validator closure into `dispatch_main_turn`; eager fallback first, same-route backoff retry only when chain exhausted |
| Safety refusal | Validator detects `finish_reason == "content_filter"` or a populated `message.refusal`; represented as a distinct non-retryable class | `step` validator; try fallback once, else surface terminally, never retry |
| Cancellation | Existing Tokio drop semantics; no new state. The single mutex on `MainFallbackState` is never held across an await, and pool writes persist before retry | unchanged from `send_main_request`/`dispatch_main_turn` |
| Timeout configuration | `connect_timeout` plus an idle/first-byte read budget on the main client, not a hard total timeout for streaming (a long thinking pause must not be killed); a total timeout is fine for the buffered `step` call. Source the value from an agent-config key, defaulting to the Python value | client build in `fresh_main_http_client` / `NativeAgentClient::new`; a new `with_request_timeouts` builder |
| Retry budget | Per-dispatch counter carried alongside `attempts` in `send_main_request`, seeded from an `api_max_retries` config field on the client, default 3 | `send_main_request`; the budget is the fallback threshold for retry-first classes |
| Backoff | `retry_utils::jittered_backoff(count, 5.0, 120.0)` for invalid/server/transport, `parse_retry_after_seconds_header_map` when the response carries `Retry-After`, adaptive Z.AI schedule already available | reuse `retry_utils`, do not reimplement; sleep must be injectable for tests (section 5) |
| Route stickiness | Existing `MainFallbackState.active` cursor and `restore_primary_route_for_turn` (`native_agent.rs:1724`); a fallback activation resets any in-flight same-route retry count exactly as Python resets `retry_count = 0` | unchanged cursor; the reset is a one-line clear when `dispatch_main_turn` advances |
| Credential pools | Unchanged. Pool rotation still runs fully inside `send_main_request` before any transport terminal is surfaced, so pool-before-fallback ordering stays structural (`_pool_may_recover_from_rate_limit`, Python) | `send_main_request`, `MainPoolCredential` |
| Preserved request bytes | Same as today. `send_main_request` re-sends the same `&Value` byte-for-byte on a same-route retry; `dispatch_main_turn` rebuilds the body per route through the closure so a fallback route gets its own model/extra_body while the primary bytes are untouched | `dispatch_main_turn` closure, `send_main_request` reference send |

## 5. Q4. Smallest vertical-slice test seam at the real local HTTP boundary

The existing test module already establishes the pattern and it is the right one:
axum test servers bound to `127.0.0.1:0`, a `TempHome` with an `auth.json`
credential pool, and a direct call to `client.dispatch_main_turn(...)`
(`native_agent.rs:4216` onward, `primary_pool_reset_deadline_keeps_the_active_fallback_sticky`).
That is the smallest slice: `dispatch_main_turn` is the seam under test, the two
axum servers are the real HTTP boundary, and the assertions read
`client.main_fallback.state` and the servers' received requests.

The one thing that must be designed for testability up front is the backoff sleep.
A jittered 5s-to-120s wait cannot run in a unit test. The sleep has to go through
an injectable hook (a sleeper on the client, or a test-only zero-delay override)
so the retry ladder runs instantly. The existing tests already reach into
`state.cooldown_until` directly, so an injectable clock/sleeper is consistent with
the established style.

Minimum red-first slices, each written to fail at `0beda20d68`:

- Connection refused replays then fails over. Bind a listener, capture its
  address, drop it so the port refuses; point the primary there and give a live
  fallback server. Assert the primary is attempted up to the budget, then the
  fallback serves, with an injected zero backoff.
- 503 overload retries then fails over. Primary returns 503 on the first two
  attempts, fallback succeeds; assert exactly the budgeted retries hit the primary
  before the switch, and that a `Retry-After` header shortens the wait through
  `parse_retry_after_seconds_header_map`.
- 500 server error walks the generic tail. Primary returns 500 every time, one
  fallback; assert retry-to-budget then fallback, and that the surfaced error on a
  no-fallback variant equals today's `main_http_error` shape.
- Streaming mid-body truncation does not fall back. Primary returns 200 then a
  truncated SSE frame after one delta; assert no cursor change, no second request,
  and no duplicated `MessageChunk`. Guards the pre-body-only rule of section 3.
- Empty choices on a tool round fails over. Primary `step` returns
  `{"choices":[]}`; assert the validator triggers, the fallback serves the round,
  and no tool executed on the empty response.
- Refusal is not retried. Primary returns 200 with `finish_reason ==
  "content_filter"`; assert one fallback attempt at most and no same-route retry.
- Timeout is bounded. Primary accepts the connection and never responds; assert the
  configured read/connect budget fires and the turn does not hang, then falls over.

## 6. Q5. Concrete race, replay, prompt-cache, and request-accounting risks in the current implementation

These are defects or latent traps observable at `0beda20d68`, independent of the
new work, that the transport lane will either expose or must avoid amplifying.

- Replay, silent empty turn. A malformed or empty 200 to the streaming path runs
  through `forward_sse`, emits nothing, and returns `Ok(None)`
  (`native_agent.rs:4121`), so the user gets an empty successful turn instead of a
  retry or a fallback. The tool path is stricter but wrong the other way: it hard
  errors on `no choices[0].message` (`native_agent.rs:4189`) with no retry or
  fallback. Both diverge from the Python invalid-response loop.
- Replay, decode failure is terminal. A `.json()` failure on a 200
  (`native_agent.rs:4179`) fails the turn even though no tool ran. The transport
  lane should decide deliberately whether this narrow case is replayable rather
  than leaving it an accident.
- Race, none inside one conversation, but confirm the cursor read in
  `dispatch_main_turn`. It reads `state.active` once at entry
  (`native_agent.rs:1888`) and writes it on success (`native_agent.rs:1901`).
  Turns of one conversation are serialized by the turn lease and rounds share the
  `Arc<Mutex<MainFallbackState>>`, so this is single-writer per conversation.
  `restore_primary_route_for_turn` correctly re-checks `state.active != active`
  after its async `next_available_at` gap (`native_agent.rs:1772`), so the
  read-modify-write across the await is guarded. The new retry-count reset on
  fallback must live under the same mutex discipline and never be held across the
  backoff sleep.
- Replay accounting, the 429 cheap retry double-bills silently. The same-key retry
  at `native_agent.rs:1995` issues a second real provider request that the provider
  bills, but a failed attempt captures no usage (`capture_usage` runs only on the
  returned success). Adding transport and server retries multiplies this: each
  retried attempt is a billed request the usage bucket never sees. This matches
  Python (retries are not accounted) but the transport lane should state it, not
  discover it.
- Accounting, partial stream is unbilled. If `forward_sse` emits text then the
  stream errors, it returns `Err` and `capture_usage` (`native_agent.rs:3637`) is
  never reached, so a partially delivered, partially billed stream records zero
  usage. The transport rule that streaming never retries post-delta is correct,
  but the usage gap is real and should be noted for the accounting lane.
- Prompt-cache, oscillation is the main risk and is already mitigated, keep it
  that way. Within a route the body bytes are stable across retries (reference
  send), and `with_main_fallback_routes` rewrites only the identity lines per route
  (`native_agent.rs:1614`), so a fallback route has a stable prefix too. The
  cache-tenant flip happens on a provider switch, which is inherent. The load
  bearing mitigation is stickiness plus the exponential primary cooldown
  (`activate_main_fallback`, `native_agent.rs:1855`) so a flapping primary does not
  ping-pong the cache. A transport class that fell back and then restored too
  eagerly would re-warm caches every turn; the new server/overload classes must
  route through the same `arms_primary_cooldown` gate or its analog so restoration
  stays damped. Note `arms_primary_cooldown()` today covers only the rate/billing
  family; decide whether overload should arm it too (a chronically overloaded
  primary should not be retried every turn).
- Prompt-cache, credential rotation can change base_url mid-provider. A rotated
  pool entry may carry its own `base_url` (`MainPoolCredential::route`,
  `native_agent.rs:999`), which is a different cache tenant for the same provider.
  This is existing behavior, but a transport retry that rotates a key changes the
  cache prefix target as a side effect; the contract lane should confirm the
  same-provider base_url assumption the earlier pool seam already flagged.

## 7. Timeout, the one genuinely new wire concern

Streaming and the buffered tool call need different timeout shapes and this is the
subtle part of "timeout configuration".

- The buffered `step` call can take a total timeout safely, because the whole
  response arrives before it is used.
- The streaming completion must not take a hard total timeout. A long thinking
  pause with no tokens is normal, and Python does not kill it with a read timeout;
  it uses a stall monitor on first-chunk latency
  (`chat_completion_helpers.py:5566` region). For Rust the safe minimum is a
  `connect_timeout` plus a first-byte deadline enforced before `forward_sse` starts
  consuming, not a total read timeout across the stream. A total timeout here would
  truncate long legitimate answers and, worse, would surface mid-stream (post
  delta) where replay is unsafe.

Represent this as an explicit `with_request_timeouts` on the client, distinct
values for the connect phase and the pre-first-byte phase, sourced from an
agent-config key with the Python default. Do not fold a streaming read timeout
into the same knob.

## 8. Prerequisites and what makes this checkpoint unsafe

- The agy contract oracle is not landed. Do not implement until it pins: the exact
  `api_max_retries` default and whether the Rust `attempts > 2` pool bound composes
  with it or is separate; the precise transport threshold (Python is `retry_count
  >= 2` for timeout/overloaded, generic tail for server_error) and whether
  overloaded arms the primary cooldown; the eager-fallback-first ordering for
  malformed 200 versus same-route retry; whether a `.json()` read failure on a 200
  is replayable; the refusal detection surface (`finish_reason` plus
  `message.refusal`) for the chat-completions shape; and the exact backoff
  constants and `Retry-After` precedence.
- A per-turn retry counter must be added to `send_main_request` or its dispatch
  frame. Today the only counter is per-credential (`attempts`), which is the wrong
  axis for the transport budget. Treating transport, overload, or server statuses
  as fallback triggers before this counter exists would fire fallback on the first
  timeout and diverge from Python.
- The backoff sleep must be injectable from day one or the vertical-slice tests
  cannot run.
- Config plumbing for `api_max_retries` and the timeouts does not exist in
  `config*.rs` (no `max_retries`, `request_timeout`, or `read_timeout` keys were
  found). That plumbing is a prerequisite, not part of the seam itself.

## 9. Deferred scope, stated plainly

- Non-chat transports. Anthropic messages, Responses/codex, and Bedrock converse
  have their own finish-reason and refusal shapes (`conversation_loop.py:3997` to
  `:4044`). This lane is chat-completions only, matching the single wire shape the
  native client sends, and deliberately does not introduce a transport trait for
  them; that is the "no broad abstraction without a second real adapter" rule.
- Fine-grained non-transport 4xx recovery (context overflow, payload too large,
  image too large, thinking-signature, reasoning-mandatory, and the rest of the
  `FailoverReason` catalog in `error_classifier.py:30`) is a separate lane. These
  are request-shape repairs, not transport retries, and stay `Unrelated` here.
- The primary-transport rebuild before max-retry fallback
  (`_try_recover_primary_transport`, `conversation_loop.py:7022`) is a refinement.
  A rotated or fresh client is already built on pool rotation and on route switch;
  whether to also rebuild the primary client once on a bare TCP reset before
  falling over can follow the first cut.
- The interruptible-backoff activity touch (`conversation_loop.py:3985`) maps to
  the gateway's own inactivity handling and is out of this seam.
