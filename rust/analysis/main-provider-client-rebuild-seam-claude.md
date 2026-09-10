# Rust seam: main-provider response stall and primary-client rebuild

Scope: the still-unwired part of the ordinary main-turn transport path that sits
*after* the retry budget and *around* body consumption. Two concerns only:

1. Response stall. A primary chat-completions request that connects and then
   goes silent (no HTTP status for a buffered call, or a 200 followed by an SSE
   stream that delivers no further bytes) must be bounded and turned into a
   replayable transport failure, not an unbounded hang.
2. Primary-client retirement and rebuild. Python retires and rebuilds the shared
   OpenAI client once, after the retry budget exhausts, before cross-provider
   fallback. This document decides whether the equivalent reqwest rebuild is
   actually required for parity.

This is a Rust ownership and interface lane. The Python behavior contract and
its goldens belong to the agy lane. This document cites live Python only where a
fact pins a Rust interface, and anchors every Rust symbol to the working tree at
`d4fd086a7d` ("Port native successful response recovery").

Not in scope, and deliberately kept out: text length continuation
(`finish_reason == "length"` re-request). That is a clean terminal that asks for
more tokens. A stall is a dead or silent connection that asks for a new one.
Section 4 draws that boundary precisely and does not restate the agy
length-continuation contract.

The transport *retry* seam already landed at `0beda20d68` and after. Same-route
retry with backoff, the `Transport` / `Overloaded` / `ServerError` classes, the
`main_attempt_limit` fallback threshold, and the streaming empty-stream replay
all exist today (section 1). This lane is the next increment on top of that and
must not rebuild any of it.

## 0. What the two sides actually do

### 0.1 Python, precisely

- `try_recover_primary_transport` (`agent/agent_runtime_helpers.py:1437` to
  `:1540`; forwarder `run_agent.py:7673` to `:7678`). After the retry budget
  exhausts, this rebuilds the primary client and grants one more primary attempt
  before fallback. It is gated: returns `False` if fallback is already active
  (`:1452`), if the error type is not transient (`:1457`), or if the provider is
  an aggregator that runs its own retry infra (OpenRouter `:1461`, Nous non
  anthropic-messages `:1469`). On success it retires the old client (`:1483` to
  `:1489`), rebuilds from the `_primary_runtime` snapshot (`:1492` to `:1528`),
  clears `_transport_cache` (`:1499`), sleeps `min(3 + retry_count, 8)` seconds
  (`:1530`), and returns `True`.
- Transient trigger set (`agent/agent_runtime_helpers.py:1974` to `:1978`):
  `ReadTimeout`, `ConnectTimeout`, `PoolTimeout`, `ConnectError`,
  `RemoteProtocolError`, `APIConnectionError`, `APITimeoutError`.
- Caller gate (`agent/conversation_loop.py:7017` to `:7034`). Fires only when
  `retry_count >= max_retries`, only once per API-call block
  (`_retry.primary_recovery_attempted`). On a `True` return it resets
  `retry_count = 0`, `_retry.has_retried_429 = False`, `agent._fallback_index =
  0`, `agent._fallback_activated = False`, and continues, that is one more full
  primary attempt. On `False` it falls through to `_try_activate_fallback`.
- The retirement is deliberately not a hard close. `_retire_shared_openai_client`
  (`run_agent.py:5706` to `:5745`) shuts the pooled sockets down (FD-safe from
  any thread) and defers the raw FD release to garbage collection. The docstring
  is explicit about why: a `client.close()` from a stranger thread can release an
  FD that another thread's SSL layer still caches, the kernel recycles it into an
  unrelated `open()` such as `kanban.db`, and the unwinding TLS flush corrupts
  that file (the #29507 / #67142 / #70773 native-corruption family). The same
  discipline drives `_replace_primary_openai_client` (`run_agent.py:5834`) and
  `_drain_transports_after_abandonment` (`run_agent.py:5747`).
- Response-stall detection lives on the streaming monitor loop
  (`agent/chat_completion_helpers.py`, trip at `:5651` to `:5652`:
  `_stale_elapsed = time.time() - last_chunk_time["t"]; if _stale_elapsed >
  _stream_stale_timeout`). `last_chunk_time` is reset on every real chunk, so one
  deadline covers both the first-byte gap and inter-chunk gaps. On a stall it does
  not raise; it cancels the current stream attempt and closes the request-local
  client, which surfaces to the worker as a forced transport error that re-enters
  the retry loop. It does not close the shared primary client from the poll
  thread (same #67142 / #70773 reason). Base threshold is
  `HERMES_STREAM_STALE_TIMEOUT`, default 180.0s, scaled up for large context
  (240s over 50k tokens, 300s over 100k) and floored for reasoning models, with a
  900s local-endpoint variant.
- Timeouts. The httpx keepalive client uses `connect=15.0, read=None,
  write=15.0, pool=10.0` (`agent/process_bootstrap.py`); `read=None` is
  intentional, the stall monitor is what bounds read inactivity. The SDK request
  timeout `get_provider_request_timeout` defaults to `None` (no override). SDK
  retries are forced to 0 so they never compound the outer loop.

### 0.2 Rust, precisely

- Every main client is a bare pool with no timeouts. `fresh_main_http_client`
  (`native_agent.rs:1128` to `:1132`), `NativeAgentClient::new`
  (`:1840` to `:1842`), and `with_runtime_credential` (`:1896` to `:1898`) are
  all `reqwest::Client::builder().build()` with no `connect_timeout`, no read or
  total timeout. `http_client_limits.rs` exists and models the httpx limits but
  is not applied to the main client. The only main-path timeout anywhere is the
  auxiliary summary call `.timeout(self.summary_timeout)` (`native_agent.rs:2906`,
  default 300s at `:1862`), which is a different lane.
- The transport retry loop reuses the same client. `send_main_request`
  (`native_agent.rs:2402`) catches a reqwest send error at `:2439`, maps it
  through `main_transport_retry_failure` (`:1301`, everything non-certificate to
  `Transport`), and on `continue` re-reads the route at `:2414`. That route read
  clones the *same* `reqwest::Client` Arc (`MainPoolCredential::route`, `:1063`
  to `:1078`, `client: active.client.clone()`). So a same-route transport retry
  runs on the identical pool. The only place a genuinely fresh reqwest client is
  built mid-turn is `install_replacement` on a credential rotation
  (`native_agent.rs:1086`), never on a bare transport retry.
- There is no response-stall detection. `forward_sse` (`native_agent.rs:4903`)
  drives `while let Some(chunk) = stream.next().await` (`:4920`) with no deadline.
  A 200 followed by silence blocks forever, because reqwest has no read timeout
  configured. A mid-stream transport error maps to `Error::Other` and returns
  `Err` at `:4921`, which is neither retried nor failed over (correct once visible
  output exists, but a pre-delta silent hang never even reaches that arm).
- The closest existing behavior is the empty-*completed*-stream replay
  (`native_agent.rs:4281` to `:4285`): a stream that finishes with no
  finish-reason and no generated content is replayed up to three times. That
  handles a stream that *ends* empty. It does nothing for a stream that never
  ends.
- No client is ever retired or rebuilt after a transport failure. There is no
  analog of `try_recover_primary_transport`, `_retire_shared_openai_client`, or a
  post-budget primary rebuild.
- `session_stall.rs` and `stream_consumer.rs` are unrelated. `session_stall.rs`
  is the gateway pending-inbound notify-once policy; `stream_consumer.rs` is
  display-fence helpers. Neither touches the HTTP stream. There is no HTTP-stream
  stall handling in the native path today.

## 1. Verified trigger classes and non-triggers

Triggers for the stall-and-rebuild seam (each maps to a Python transient class):

- Buffered `step` request that connects and then hangs with no HTTP status.
  Python `ReadTimeout` / `APITimeoutError`. In Rust today this is an unbounded
  hang; there is no `.timeout()` on the `send_main_request` post at
  `native_agent.rs:2435`.
- Buffered `step` request that fails to connect or is reset before any status.
  Python `ConnectError` / `ConnectTimeout` / `APIConnectionError`. Already
  classified in Rust: the send error at `:2439` becomes `Transport` and retries
  under budget. What is missing is the bounded connect deadline and the optional
  post-budget rebuilt attempt.
- Streaming 200 that opens and then delivers no further bytes past the deadline,
  before any visible delta. Python stall trip at `chat_completion_helpers.py:5651`
  producing a forced transport error. In Rust this is the unbounded
  `stream.next().await` hang at `native_agent.rs:4920`.

Non-triggers, verified, that must stay out of this seam:

- Any non-success HTTP status (auth, billing, rate, overload, server, unknown).
  Already owned by `send_main_request` / `dispatch_main_turn`. A stall is defined
  by the *absence* of a status or of bytes, not by a status.
- A stream that *completes* empty or with a refusal. Owned by the empty-stream
  replay (`native_agent.rs:4281`) and the success-body fallback
  (`main_success_body_failure`, `native_agent.rs:999`; refusal handling in
  `run_model_turn` `:4227` to `:4274`). A completed stream is not a stall.
- `finish_reason == "length"` truncation. That is length continuation, the agy
  lane, and it is a clean terminal, not a stall. See section 4.
- A streaming transport error *after* a visible delta. Must fail the turn, never
  retry or fall back (replay would double-emit). Current `forward_sse` return-Err
  at `:4921` is correct; the stall deadline must preserve it.
- Aggregator providers. Python skips the rebuilt-client attempt for OpenRouter
  and Nous-OpenAI-wire (`agent_runtime_helpers.py:1461`, `:1469`) because they run
  server-side retry infra. If the Rust seam adopts a post-budget rebuilt attempt
  it must carry the same skip; if it adopts only the stall deadline (recommended,
  section 5) the skip is moot because the deadline is provider-neutral and simply
  reclassifies a hang as a normal `Transport` failure the existing loop already
  handles per provider.

## 2. Concurrency and ownership invariants

- reqwest client sharing. A `reqwest::Client` is a cheap `Arc` handle over one
  connection pool. `route()` clones that Arc under the `active` mutex
  (`native_agent.rs:1063` to `:1078`). Any rebuild must go through the existing
  `install_replacement` shape (`:1081` to `:1100`): take the `active` lock, and
  swap only if the credential identity still matches the failed route, so a
  concurrent rotation is not clobbered. The stall deadline itself is stateless
  and shares nothing, so it needs no new synchronization.
- No lock is held across an await today, and the seam must keep it that way. The
  `MainFallbackState` mutex in `dispatch_main_turn` is taken twice, each time
  released before the next await (`:2356`, `:2370`). The stall deadline runs
  entirely inside the stream loop and touches no shared state.
- FD safety is a Python-only hazard and must not be ported. The entire
  shutdown-not-close discipline (`_retire_shared_openai_client`,
  `_drain_transports_after_abandonment`, `_force_close_tcp_sockets`, the poisoned
  request-client cache) exists because httpx hands out a client whose
  `close()` releases raw integer FDs, and doing that from the wrong thread while
  another thread's OpenSSL BIO still caches the fd corrupts unrelated files
  (`run_agent.py:5706` to `:5745`, #29507 / #67142 / #70773). reqwest has no such
  surface: there is no manual `close()`, hyper owns socket lifetime through Rust
  ownership and `Drop`, and dropping one `Client` clone never frees a socket that
  another clone or an in-flight request still borrows. A stalled stream in Rust
  is aborted by dropping the `Response` / `bytes_stream()` future, which drops the
  connection cleanly. So none of the FD-safety machinery has a reqwest analog and
  none of it should be recreated. This is the single most important parity
  clarification in this document.
- Pool-health on transport error. hyper marks a pooled connection broken on a
  read or write error and does not hand it back out; the next request dials a
  fresh connection. So a reset or half-dead keepalive connection is already not
  reused on the Rust retry, without rebuilding the client. This is what makes the
  client rebuild optional (section 3).

## 3. Is rebuilding the reqwest client actually required for parity?

No, not for correctness, and yes only as an optional connection-hygiene
refinement. The reasoning:

1. The behaviorally observable effect of `try_recover_primary_transport` is
   threefold: (a) one extra primary attempt after the retry budget, with a
   `min(3 + retry_count, 8)`s wait, before fallback; (b) resetting the 429 and
   fallback bookkeeping so a post-recovery 429 can still activate fallback; and
   (c) dropping stale pooled connections. Only (c) is the client rebuild itself.
2. (c) is redundant in Rust. hyper already refuses to reuse a connection that
   errored, and `pool_idle_timeout` retires idle keepalives. The Python rebuild
   exists largely because httpx keepalive pools plus the process-shared transport
   (`process_bootstrap.py`) can otherwise hand back a connection that died
   silently, and because the FD dance forces an explicit swap. Neither pressure
   exists here.
3. (a) and (b) are real parity behavior, but they are a *retry-budget and
   fallback-state* concern, not a *client-object* concern. If the agy contract
   says parity requires the extra post-budget primary attempt, it can be
   implemented as one more iteration of the existing `send_main_request` budget
   (raise the `Transport` / `Overloaded` attempt ceiling by one, or add a single
   post-budget replay in `dispatch_main_turn` before `activate_main_fallback`),
   with no client rebuild at all.
4. If, and only if, the oracle wants byte-for-byte connection freshness on that
   extra attempt, swap in a fresh client cheaply through the existing
   `install_replacement`-style path under the `active` mutex. It is a few lines
   and FD-safe. Recommendation: do not do this in the first cut. Add the stall
   deadline and the timeouts, treat the connection rebuild as a deferred
   refinement, and let hyper's pool health carry the staleness case.

Conclusion: the required parity work is the stall deadline plus bounded connect
and buffered-call timeouts. The client rebuild is optional and, if adopted,
belongs to the retry-budget seam, not to a new client-lifecycle subsystem.

## 4. Stall versus length continuation, the boundary only

Stated once, without restating the agy contract. A length continuation is a
completed response: the stream ends, `finish_reason == "length"`, usage is
recorded, and the agent re-requests more tokens with the prior content preserved.
A stall is an incomplete response: no `finish_reason` ever arrives, no terminal
SSE frame closes the stream, and the connection is silent past the deadline. The
two are orthogonal signals on opposite ends of a stream:

- Length: clean terminal, replay is a semantic continuation, client is healthy.
- Stall: no terminal, replay is a fresh transport attempt, connection is suspect.

The Rust seam keys purely on liveness (time since last byte) and never inspects
`finish_reason`, so it cannot collide with length continuation. A stalled stream
that is later revealed to have been a slow-but-live reasoning pause is guarded
the same way Python guards it: the deadline is generous (180s base, higher for
large context and reasoning models) and resets on every real byte, so a thinking
model that emits keepalive or token bytes never trips it. This seam does not
implement, alter, or depend on length continuation.

## 5. The smallest safe Rust seam

Two additive pieces, no new subsystem, no transport trait (one wire shape,
chat-completions, so the no-broad-abstraction rule applies).

### 5.1 Buffered `step` path: a total request timeout

The buffered tool-round call is fully consumed before use, so a hard total
timeout is safe and is the exact shape already used for summaries. Add
`.timeout(request_timeout)` to the `send_main_request` post
(`native_agent.rs:2435`), sourced from a new config key. A resulting reqwest
timeout error already flows through the send-error arm at `:2439` and
`main_transport_retry_failure` (`:1301`) into `Transport`, so it retries under
budget and then activates fallback with no other change. This closes the
buffered-hang trigger by reusing the landed classification.

### 5.2 Streaming path: an inactivity deadline, pre-visible only

Wrap the per-chunk await in `forward_sse` (`native_agent.rs:4920`) with
`tokio::time::timeout(stale_timeout, stream.next())`, resetting the clock on
every received chunk (matching Python's `last_chunk_time`). On elapse:

- If no visible delta has been emitted yet (`!outcome.visible`), return a
  distinct retryable stall outcome. `run_model_turn` treats it exactly like the
  existing empty-stream replay branch (`native_agent.rs:4281`): replay the route
  under budget, then activate fallback. Nothing durable or visible has crossed
  the boundary, so replay is safe.
- If a visible delta has already been emitted, return `Err` as today. The turn
  fails, no retry, no fallback, no double emit. This preserves the section-1
  post-delta invariant.

This is a deadline on `next()`, not a total read timeout on the whole stream, so
a long legitimate answer with steady bytes is never truncated, and the stall can
only ever surface pre-visible where replay is safe. Dropping the timed-out
`bytes_stream()` future drops the connection cleanly (section 2), which is the
Rust equivalent of Python cancelling the stream attempt.

### 5.3 Connect timeout and config plumbing

Add `connect_timeout` (and the `http_client_limits` pool settings) to the three
main client builders (`native_agent.rs:1128`, `:1840`, `:1896`) through a shared
`with_request_timeouts` helper. Do not add a streaming total read timeout. The
config keys do not exist yet: a grep of `config*.rs` finds no `request_timeout`,
`read_timeout`, `connect_timeout`, `stale_timeout`, or `max_retries`. That
plumbing (mirroring `hermes_cli/timeouts.py`: per-model then per-provider, with
the Python defaults, `None` request timeout, 180s stall base) is a prerequisite,
not part of the seam body.

### 5.4 What this seam does not add

- No client retirement subsystem, no `_retire_shared_openai_client` analog, no
  socket-shutdown helper. Rust `Drop` covers it (section 2).
- No mandatory post-budget primary rebuild. Optional, deferred to the retry
  seam if the oracle requires the extra attempt (section 3).
- No new shared state, no new mutex, no new loop.

## 6. Public-seam red tests

Same public seam and harness as the retry lane: axum servers on `127.0.0.1:0`, a
`TempHome` pool, and a direct `dispatch_main_turn` / `run_turn` call, asserting on
`main_fallback.state` and the servers' received requests. The stall timeout must
be injectable from day one, in the style of the existing
`with_main_retry_backoff` test hook (`native_agent.rs:2062`); add a
`with_stream_stale_timeout(Duration)` (and a buffered `request_timeout`
override) so the deadline fires in milliseconds. Each test is written to fail at
`d4fd086a7d`.

- Pre-visible stream stall replays then fails over. Primary returns 200 then
  holds the socket open emitting only SSE comment keepalives past the injected
  deadline; live fallback server. Assert the deadline fires, the primary is
  attempted up to the budget, the fallback serves, and no `MessageChunk` was ever
  emitted from the primary. Fails today because `stream.next().await`
  (`:4920`) blocks forever.
- Post-visible stream stall fails the turn without fallback. Primary sends one
  delta then goes silent past the deadline. Assert the turn errors, the cursor is
  unchanged, no second request reaches any server, and no duplicated
  `MessageChunk`. Guards the replay barrier.
- Buffered `step` hang classifies as transport and fails over. Primary `step`
  accepts the connection and never responds; fallback serves. Assert the injected
  request timeout fires, the primary is retried under budget, and the fallback
  serves the round. Fails today because `send_main_request` sets no `.timeout()`.
- Connect refusal already replays then fails over (retry-lane behavior, kept
  green as a guard). Bind then drop a listener so the port refuses; assert budget
  attempts then fallback. This should already pass and pins that the stall seam
  does not regress the landed connect path.
- Optional, only if the rebuild is adopted: post-budget primary retry runs one
  extra attempt after the budget before fallback, with the aggregator skip. Assert
  attempt count and the OpenRouter/Nous skip. Marked optional per section 3.

## 7. Commands run

All read-only.

- `grep -n` over `native_agent.rs` for the client builders, `send_main_request`,
  `dispatch_main_turn`, `forward_sse`, `run_model_turn`, `main_retry`,
  `is_timeout`/`is_connect`, and stall/timeout keywords.
- `sed -n` reads of `native_agent.rs` ranges 900-1000, 1040-1148, 1290-1335,
  1820-1900, 1924-2064, 2352-2400, 2402-2626, 2900-2915, 4167-4285, 4903-4986.
- `head`/`grep` of `session_stall.rs`, `stream_consumer.rs`, `http_client_limits.rs`.
- `grep -rn` over `config*.rs` for `request_timeout`, `read_timeout`,
  `connect_timeout`, `api_max_retries`, `max_retries` (none found).
- Python: `grep -rn`/`Read` of `run_agent.py:7673`, `run_agent.py:5706-5885`,
  `agent/agent_runtime_helpers.py:1437-1540` and `:1974-1978`,
  `agent/conversation_loop.py:7017-7034`,
  `agent/chat_completion_helpers.py:820-853, 1290-1384, 5648-5652`.
- `git log --oneline -1` (head `d4fd086a7d`).
- Test discovery: `grep -rln` over `tests/` for `try_recover_primary_transport`,
  `primary_recovery`, `_retire_shared_openai_client`, `STREAM_STALE`,
  `stale_call_kill`, `29507`/`70773`.

## 8. Focused Python tests to mirror

- `tests/run_agent/test_primary_runtime_restore.py:567`
  `TestTryRecoverPrimaryTransport`: `test_recovers_on_read_timeout` (`:569`),
  `test_skipped_when_already_on_fallback` (`:600`),
  `test_allowed_for_nous_anthropic_messages` (`:613`),
  `test_wait_time_scales_with_retry_count` (`:648`),
  `test_wait_time_capped_at_8` (`:660`), `test_survives_rebuild_failure` (`:673`).
- `tests/run_agent/test_32646_fallback_429_after_timeout.py`: fallback state
  reset after primary recovery, and a post-recovery 429 still reaching fallback.
- `tests/run_agent/test_direct_contexts_stream_inline.py:182`
  `test_inline_stream_stale_detector_still_fires_from_monitor_thread` and `:203`
  cross-thread abort promptness.
- `tests/agent/test_non_stream_stale_timeout.py`,
  `tests/agent/test_reasoning_stale_timeout_floor.py`,
  `tests/agent/test_codex_ttfb_watchdog.py` for the stall-threshold derivation.
- `tests/run_agent/test_70773_shared_client_fd_corruption.py`: cited only to
  document that the FD-safety behavior it locks in is Python-httpx-specific and is
  a deliberate non-port for Rust (section 2), not a parity gap.

## 9. Deferred scope, stated plainly

- OAuth and credential-recovery client rebuilds. Nous OAuth recovery and pool
  credential rotation already rebuild the reqwest client through
  `install_replacement` (`native_agent.rs:1086`) and their own landed lanes
  (commits `1b3b4173c3`, `d87226dbc9`). This seam does not touch them.
- Auxiliary and compression clients. The summary path has its own per-request
  `.timeout(summary_timeout)` (`native_agent.rs:2906`); its stall and rebuild
  behavior is a separate lane and is not folded into the main-turn deadline.
- Non-chat transports. Python's recovery has dedicated anthropic_messages and MoA
  branches (`agent_runtime_helpers.py:1505` to `:1522`) and the process-shared
  transport supports Bedrock and codex. The native Rust client is
  chat-completions only (`with_provider_profile` rejects other api_modes,
  `native_agent.rs:1967`). No transport trait, no non-chat rebuild here.
- The Python process-shared httpx transport pool and the per-request wire-client
  cache (`process_bootstrap.py`, `run_agent.py:5937` to `:6019`) have no reqwest
  analog and are not ported. reqwest's own pool plus `pool_idle_timeout` is the
  equivalent.
- The mandatory post-budget primary rebuild attempt. Optional refinement, owned
  by the retry-budget seam if the oracle requires it (section 3), not this lane.

## 10. Prerequisites before implementation

- Config plumbing for `connect_timeout`, the buffered `request_timeout`, and the
  streaming `stale_timeout` (per-model then per-provider, Python defaults, 180s
  stall base). None exist in `config*.rs` today.
- An injectable stall timeout and buffered request timeout on the client, in the
  `with_main_retry_backoff` test-hook style, or the streaming red tests cannot
  run in milliseconds.
- The agy oracle to pin: the exact stall base and its context and reasoning-model
  scaling; whether a pre-visible stall is replayable the same number of times as
  the empty-stream branch (three) or on its own budget; and whether parity
  actually requires the extra post-budget primary attempt (and if so, whether a
  fresh connection on that attempt is observable, which decides section 3).
