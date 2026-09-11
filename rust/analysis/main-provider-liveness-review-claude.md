# Native Main-Provider Liveness: Narrow Correctness Review

Scope: the current uncommitted native liveness diff only.

Files read:
- `git diff` for `rust/crates/hermes-gateway/src/main.rs`
- `git diff` for `rust/crates/hermes-gateway/src/native_agent.rs`
- `rust/crates/hermes-gateway/src/main_provider_timeouts.rs` (on-disk, untracked)
- `rust/analysis/main-provider-stall-contract-agy.md` sections 3, 9, 10, 11, 12, 14

No tests were run, no unrelated modules were inspected, no code was edited.

## Important: this diff moved under review

`native_agent.rs` and `main_provider_timeouts.rs` are being actively edited by the
parallel stall lane while I reviewed them. The first snapshot I read used
`MainRequestError::StreamStale` and a single `stream_stale_timeout`; the current
on-disk version has already renamed that to `MainRequestError::StreamInactivity`
with a `stale_strike` flag and split the streaming policy into
`stream_stale_timeout` (structured monitor) plus a new `stream_inactivity_timeout`
backed by `stream_read_base` / `request_configured` (the section 8 socket-read
model). Mtimes at review time: `main_provider_timeouts.rs` 12:55, `native_agent.rs`
12:59. Treat the findings below as a snapshot against that state; the socket-read
pieces it introduces belong to contract section 8, which is outside the sections I
was asked to read, so I did not validate them against their own spec.

## Findings

### 1. SSE keepalive comments do not re-arm the inactivity deadline (contract 9.1 divergence)

- Evidence:
  - `rust/crates/hermes-gateway/src/native_agent.rs:5407-5417` `main_sse_line_is_activity`
    only returns true for a `data:` line whose payload is `[DONE]` or parses as JSON.
  - `rust/crates/hermes-gateway/src/native_agent.rs:5458-5460` the inactivity deadline
    is re-armed only when `main_sse_line_is_activity(&line)` is true.
  - `rust/crates/hermes-gateway/src/native_agent.rs:12076` the test
    `sse_comments_do_not_reset_the_provider_activity_deadline` pins this behavior: a
    stream that emits only `: keepalive\n\n` comments is killed as stalled.
  - Contract section 9.1 (`main-provider-stall-contract-agy.md:288-292`): "On every
    incoming SSE chunk, `last_chunk_time['t']` updates to `time.time()`. The stale
    condition governs both silence before the first byte and silence between any two
    subsequent bytes." Section 9.3 frames the kill as "no chunks received."

- Defect: Python re-arms the stale timer on any bytes received from the socket,
  including SSE comment lines (`: ...`) and blank separator lines. The Rust code only
  re-arms on parseable `data:` events, so a provider that holds the connection open
  with comment heartbeats but has not yet emitted a token is treated as stalled once
  the inactivity window elapses.

- Failure scenario: OpenRouter (and some upstreams) emit `: OPENROUTER PROCESSING`
  comment heartbeats during a long queue or reasoning phase before the first token.
  With a non-reasoning model the inactivity window is short (default base 180s, and
  `stream_inactivity_timeout` can be lower via `stream_read_base`). A provider that
  sends a comment heartbeat every few seconds but no `data:` token for longer than
  that window is killed and retried/failed-over in Rust, while Python keeps waiting
  because each heartbeat byte resets its timer. That turns a healthy slow start into a
  spurious reconnect, an extra provider call, and (once the inner batch and outer
  budget are spent) an unnecessary fallback, even though the socket never actually
  went silent.

- Smallest fix: treat bytes-level activity, not just parseable data events, as
  liveness. The minimal change is to re-arm `stale_deadline` for every non-empty line
  (or every received chunk) rather than gating on `main_sse_line_is_activity`, i.e.
  also re-arm on SSE comment lines (leading `:`) and blank separator lines. That makes
  the re-arm match "every incoming SSE chunk" from 9.1. Note this directly contradicts
  the current `sse_comments_do_not_reset_the_provider_activity_deadline` test, so that
  test encodes the divergence and would need to flip with the fix. If the lane
  deliberately wants comment-only streams to count as stalls, that decision should be
  reconciled with contract 9.1 rather than left as an undocumented behavior change.

- Verdict: CONFIRMED divergence from the contract text in scope. Real-world impact
  depends on whether the configured provider emits comment heartbeats, so severity is
  medium, but the behavior is unambiguously different from what 9.1 specifies.

## What I verified and found correct (no finding)

- Replay safety (contract 10.1 / 10.2, invariant 14.3 #2): `forward_sse` sets
  `outcome.visible = true` exactly when a non-empty chunk is emitted to `events`
  (`native_agent.rs:5428`, plus the tail/flush paths). The post-visible stall branch
  (`native_agent.rs:5445-5448`) sets `finish_reason = "length"` and the run-turn loop
  skips the replay block when `outcome.visible` is true (`native_agent.rs:4574`),
  routing into bounded length continuation instead of replaying. A pre-visible stall
  (`visible == false`) is the only case that retries the request, which is safe
  because nothing was emitted. The `postvisible_stale_stream_uses_length_continuation_without_replay`
  test asserts the visible fragment is not replayed.

- Pre-visible retry ceiling then fallback (contract 10.1): the inner batch runs up to
  `stream_attempts()` (= `HERMES_STREAM_RETRIES` + 1) attempts, then the outer budget
  is `max_attempts.min(2)` when a fallback exists before activating the success-body
  fallback (`native_agent.rs:4580-4603`). That matches the "retry on the same provider,
  then fall back" shape, and `preheader_stale_uses_stream_retry_batch_before_fallback`
  (3 primary calls then 1 fallback at `max_attempts == 1`) agrees.

- Breaker counting (contract 11.1, invariant 14.3 #3/#4): `consecutive_stale_streams`
  is an `Arc<AtomicUsize>` shared through `main_route` for the primary (index 0 clones
  self) and per fallback object for fallbacks, so it persists across turns. Each stall
  increments exactly once: the pre-header `StreamInactivity` path and the `forward_sse`
  stall path both add 1 only when `stale_strike` is set (`native_agent.rs:4590-4592`,
  `5446`), and the buffered paths add 1 on timeout in `send_main_request`
  (`native_agent.rs:2648-2655` region) and in the `step` decode closure
  (`native_agent.rs:5631-5645`). I found no double-count across a single attempt.

- Give-up breaker placement (contract 11.2, invariant 14.3 #4): the threshold is
  checked at loop entry before any HTTP request in both the stream loop
  (`native_agent.rs:4494-4508`, gated on `stale_stream_inner_attempts == 0`) and the
  buffered `step` loop (`native_agent.rs:5576-5597`). The stream and buffered breaker
  tests (`stale_stream_breaker_survives_turns_and_stops_network_replay`,
  `buffered_stale_breaker_stops_retries_and_survives_calls`) confirm the call count is
  bounded and the streak survives across calls. Note the Rust breaker tries a
  success-body fallback before aborting (it raises only when no fallback is left),
  which is a behavior addition over Python's "raise immediately," but it is
  defensible: fallback activation resets the failed route's streak, so it does not
  wedge, and the contract's core requirement (no further request on the wedged route)
  holds.

- Streak reset triggers (contract 11.1): reset on successful/visible stream
  (`native_agent.rs:4605-4607`), on successful buffered step
  (`native_agent.rs:5697` region), on provider fallback activation (the `failed`
  route is reset inside both `activate_main_fallback` and
  `activate_main_success_body_fallback`), and on primary runtime restoration
  (`restore_primary_runtime` now calls `reset_stale_stream_streak`). Post-visible
  stalls reset rather than increment, matching the partial-stub reset in 10.2.

- Timeout precedence (contract 3): `resolve` applies model -> provider -> env ->
  default for the request timeout (`timeout_seconds` / `request_timeout_seconds` /
  `HERMES_API_TIMEOUT` / 1800) and for the stream stale base
  (`stale_timeout_seconds` / `HERMES_STREAM_STALE_TIMEOUT` / 180), and buffered uses
  the same config base then `HERMES_API_CALL_STALE_TIMEOUT` then the reasoning floor
  then `(90, implicit)`. The local-endpoint branch for streaming only triggers when the
  resolved base equals exactly 180 (`stream_stale_timeout`), and the buffered
  local-short-circuit to `None` only triggers when `buffered_stale_implicit` is true,
  so a reasoning floor (which clears `implicit`) correctly keeps its floor on a local
  endpoint instead of going to infinity. That matches the asymmetry called out in 3.2
  vs 3.3.

## Open question for the section 8 lane (not a confirmed finding)

`forward_sse` is given `stream_inactivity_timeout` as its re-arm window and that
window is not capped by `request_timeout`, whereas the pre-header `header_timeout`
is `stream_inactivity_timeout.min(request_timeout)`. When the total request timeout
comes from `HERMES_API_TIMEOUT` (so `request_configured` is false) and is smaller
than the stale base, `stream_inactivity_timeout` can resolve above that env request
timeout, so the body-read phase can out-wait the configured total request timeout.
Whether that is correct depends on section 8 socket-read semantics, which were out of
scope for this review, so I am flagging it rather than asserting a defect.

## Primary disposition after source verification

Finding 1 is rejected. The contract wording used by the review was imprecise:
Hermes updates `last_chunk_time` for every chunk yielded by the OpenAI SDK, not
for every raw byte received by httpx. In the pinned OpenAI SDK 2.24.0,
`SSEDecoder.decode()` returns `None` for lines beginning with `:` and for empty
events. Those comment keepalives never reach `_accept_stream_chunk` or the
`for chunk in _iter_provider_stream_chunks(...)` loop. The Python source also
states the intended behavior directly at `agent/chat_completion_helpers.py`:
the watchdog detects connections kept alive by SSE pings but delivering no real
chunks. The Rust `sse_comments_do_not_reset_the_provider_activity_deadline` test
therefore preserves the source behavior.

The section 8 open question is also resolved. Python's `HERMES_API_TIMEOUT`
fallback is `_base_timeout`, but httpx receives it only for the write timeout.
Unless provider or model configuration is present, the streaming read timeout
comes from `HERMES_STREAM_READ_TIMEOUT` and can be raised to the structured
stale timeout. It is not a total response-body deadline. Rust follows that
split: a configured model/provider request timeout controls body inactivity,
while an environment-only API timeout does not cap each body read.
