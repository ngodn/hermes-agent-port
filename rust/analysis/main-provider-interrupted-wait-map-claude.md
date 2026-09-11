# Main-provider interrupted-wait accounting: read-only seam map

Scope: this document maps the "interrupted-wait accounting" seam only. It traces
how the live Python main chat-completions path waits before a retry, how an
interrupt or steering correction cuts that wait short, what wall-clock time is
counted or excluded from the provider stale breaker and the run budget, which
state is reset, and the public tests that pin the behavior. It then maps the
current Rust surfaces (`wait_before_main_retry`, timeouts, run budget,
dispatcher cancellation, steering) and identifies the narrowest safe production
seam plus the public tests that would guard it.

This map is deliberately independent of operator-notice rendering (the wait
notice text, status buffering, and fallback notice lifecycle are out of scope
and covered by the separate notice analyses). It does not propose an
implementation. All paths and line numbers below were read first-hand against
the current tree.

## 1. Two distinct "waits" in the live Python path

There are two separate waits, and the seam name spans both. Keeping them
separate is essential.

### 1a. The retry backoff wait (sleep between attempts)

This is the wait after a retryable API error, before the next attempt. It lives
in the main conversation retry loop.

- `agent/conversation_loop.py:7259` computes `wait_time`, from the provider's
  `Retry-After` if present (clamped to 600s at `:7256`), else
  `jittered_backoff(retry_count, base_delay=2.0, max_delay=60.0)`. Rate-limit /
  overload errors override it with `adaptive_rate_limit_backoff(...)` at
  `:7262`.
- The sleep itself is chunked, not a single blocking call:
  `agent/conversation_loop.py:7297-7327`. `sleep_end = time.time() + wait_time`,
  then a `while time.time() < sleep_end` loop that calls `time.sleep(0.2)` each
  pass (`:7319`) and checks `agent._interrupt_requested` (`:7300`) every 200ms.
- A second, structurally identical chunked backoff loop guards the empty-response
  retry path: `wait_time` at `agent/conversation_loop.py:8615` from
  `jittered_backoff(agent._empty_content_retries, base_delay=5.0,
  max_delay=60.0)`, sleeping in 0.2s increments at `:8656`. A third occurrence of
  the same pattern is at `:3949` / `:3981`. The 200ms-poll-with-interrupt-check
  is the shared idiom for every main-path backoff.

### 1b. The provider-response wait (stale-call watchdog)

This is the wait for a provider that accepted the connection but has not
produced output. It lives in the non-streaming inline / interrupt-worker call in
`agent/chat_completion_helpers.py`. The stale watchdog kills the call at
`_report_stale_nonstream_kill` (`:759`) and derivation
`_derive_stream_stale_timeout` (`:818`). This wait is what an interrupt "cuts
short" in the accounting sense of section 2b.

## 2. How interrupts and steering cut the waits short

### 2a. Interrupt or steering during the backoff wait

Inside the chunked backoff loop (`agent/conversation_loop.py:7299-7327`), when
`agent._interrupt_requested` is seen (`:7300`):

- Steering correction: `agent.clear_interrupt(preserve_redirect=True)` at
  `:7304`. If a redirect survives, it sets
  `_retry.restart_with_redirected_messages = True` and breaks (`:7305-7306`).
  The loop then leaves the retry wait at `:7328-7332` and rebuilds the iteration
  from the correction instead of re-firing the stale request. Steering therefore
  cuts the wait short and restarts the turn, it does not abort it.
- Plain interrupt (no redirect): `:7307-7318` closes the interrupted tool
  sequence, persists the session, calls `agent.clear_interrupt()`, and returns
  `{"completed": False, "interrupted": True, ...}`. The wait is cut short and the
  turn ends.
- Liveness during the wait: every ~30s the loop calls `agent._touch_activity(...)`
  (`:7323-7327`, gated by `_backoff_touch_counter % 150`), so the gateway
  inactivity monitor does not mistake a long legitimate backoff for a hung
  process. The backoff wall-clock is thus kept "alive" but is not excluded from
  wall-clock budgets (section 3).

### 2b. Interrupt during the provider-response wait (the accounting core)

This is the behavior the seam is actually named for. In the non-streaming
call's poll loop, when `agent._interrupt_requested` becomes true
(`agent/chat_completion_helpers.py:1891`), before force-closing the in-flight
client, it calls `_record_interrupted_provider_wait(...)` (`:1892-1899`). The
same call guards the other request paths at `:3902`, `:3973`, and `:5710`.

`_record_interrupted_provider_wait` is defined at
`agent/chat_completion_helpers.py:734-756` with threshold
`_INTERRUPTED_WAIT_STALE_SECONDS = 30.0` (`:731`):

- If `response_started` is true, or `elapsed < 30.0s`, it returns `False` and is
  neutral (`:747-748`). A quick user cancel or a mid-response interrupt does not
  count.
- Otherwise it treats the user's interrupt as evidence of an unresponsive
  attempt and calls `_bump_stale_streak(agent)` (`:749`), advancing the
  cross-turn stale circuit breaker.

The interrupt then marks the request cancelled and force-closes the
worker-local HTTP client (`:1904-1918`), so the abort is recognized as a cancel
rather than a network error that would burn retry cycles.

## 3. What time is counted or excluded from stale and run budgets

### 3a. Cross-turn stale circuit breaker

Documented at `agent/chat_completion_helpers.py:699-708`. The agent carries
`_consecutive_stale_streams`, read via `_stale_streak` (`:710`), incremented via
`_bump_stale_streak` (`:717`), reset via `_reset_stale_streak` (`:724`). The
give-up check `_check_stale_giveup` (`:804-815`) reads
`HERMES_STREAM_STALE_GIVEUP` (default 5) and, past threshold, raises immediately
with no network attempt and no stale-timeout wait. What counts toward the
streak: real stale kills (`_bump_stale_streak` at `:1873`) and interrupted
pre-response waits over 30s (section 2b). What does not count: interrupts under
30s or after first output.

### 3b. Wall-clock run budget

`run_agent.py:1605-1620`. When `run_budget_seconds` is active and the stale
timeout is not explicit (`_stale_timeout_is_explicit`, `:1622-1632`), the
implicit stale timeout is capped at `max(60.0, remaining * 0.5)` where
`remaining = run_budget_seconds - (time.time() - _run_budget_started_at)`
(`:1616-1619`). It never raises the timeout, and an explicit user-configured
`stale_timeout_seconds` or `HERMES_API_CALL_STALE_TIMEOUT` wins untouched.

The budget clock is stamped once per turn at `agent/turn_context.py:762`
(`agent._run_budget_started_at = time.time()`, else `None` at `:764`), and the
wrap-up latch resets at `:765`. A soft wrap-up notice is injected once at 80% of
the budget by `_maybe_inject_run_budget_wrapup` (`agent/conversation_loop.py:198`,
threshold at `:219`, invoked at `:2433`); that is advisory, it does not abort.

Because the run budget is pure wall-clock (`time.time() - started`), backoff
sleep time and provider-response wait time are both implicitly counted against
it. There is no subtraction or exclusion of wait time from the run budget. The
only "exclusion" anywhere is the liveness `_touch_activity` heartbeat (section
2a), which affects the inactivity monitor, not the budget clock.

### 3c. State reset points

`_reset_stale_streak` is called on a completed/healthy call and on provider
swaps: `agent/chat_completion_helpers.py:1371`, `:1931`, `:3180`, `:3985`,
`:5868`, `:5874`, and in the runtime helpers on switch/fallback/restore at
`agent/agent_runtime_helpers.py:1943-1944` and `:3432-3433`. Reset semantics:
the streak measured the OLD provider, so `switch_model` /
`try_activate_fallback` / `restore_primary_runtime` clear it. On the backoff
side, a steering restart resets the iteration
(`restart_with_redirected_messages`) but does not by itself reset the stale
streak.

## 4. Relevant public Python tests

- `tests/run_agent/test_stream_stale_circuit_breaker.py:64`
  `test_interrupted_pre_response_wait_advances_streak` calls
  `_record_interrupted_provider_wait` directly: 29.9s neutral, 45s with
  `response_started=True` neutral, 45s and 60s with `response_started=False`
  advance the streak (`:76-83`). This is the seam's spec.
- Same file: `test_short_circuits_when_streak_at_threshold` (`:88`),
  `test_success_resets_streak` (`:106`), `test_stale_kill_increments_streak`
  (`:120`).
- `tests/run_agent/test_stream_stale_breaker_reset.py`:
  `test_switch_model_resets_stale_streak` (`:86`),
  `test_switch_model_failure_does_not_reset_streak` (`:106`),
  `test_fallback_activation_resets_stale_streak` (`:132`),
  `test_fallback_exhaustion_keeps_stale_streak` (`:148`),
  `test_non_streaming_short_circuits_at_threshold` (`:165`),
  `test_non_streaming_success_resets_streak` (`:177`).
- `tests/agent/test_run_budget.py`: `test_active_budget_caps_implicit_reasoning_floor`
  (`:122`), `test_active_budget_cap_half_remaining` (`:142`),
  `test_active_budget_never_raises_timeout` (`:156`),
  `test_explicit_provider_config_wins_over_budget_cap` (`:168`),
  `test_explicit_env_var_wins_over_budget_cap` (`:177`),
  `test_budget_without_started_clock_is_inert` (`:187`).
- `tests/agent/test_bedrock_interrupt_post_worker.py:77` patches
  `_record_interrupted_provider_wait` to assert the interrupt path invokes it.
- Backoff wait cut short by interrupt:
  `tests/run_agent/test_run_agent.py` `TestRetryAfterCap` (`:1961`), whose
  `_drive_once` (`:1966`) sets `agent._interrupt_requested = True` mid-backoff
  (`:1993`) so the chunked sleep breaks, and `test_retry_after_under_cap_is_honored`
  (`:2000`) asserts the Retry-After wait. This exercises the hard-interrupt branch
  of the backoff loop (section 2a).
- Backoff math: `tests/test_retry_utils.py` (`test_backoff_is_exponential`,
  `test_backoff_respects_max_delay`, `test_backoff_attempt_1_is_base`, the Z.AI
  overload tiers).
- Steering / redirect plumbing: `tests/run_agent/test_steer.py`
  (`test_cancels_only_an_active_model_request`,
  `test_hard_interrupt_wins_over_new_redirect`,
  `test_clear_interrupt_drops_pending_steer`), and interrupt-vs-retry interaction
  in `tests/run_agent/test_stream_interrupt_retry.py`
  (`test_interrupt_prevents_stream_retry`, `test_normal_retry_still_works_without_interrupt`).
- Coverage gap worth noting: no dedicated public test asserts that a steering
  redirect landing during a backoff sleep rebuilds the iteration
  (`restart_with_redirected_messages` at `agent/conversation_loop.py:7305`).
  `TestRetryAfterCap` covers only the hard-interrupt branch of that same loop.
- Golden generator (not a test but the Rust oracle):
  `rust/tools/gen_main_provider_stall_goldens.py:71` imports
  `_record_interrupted_provider_wait` and exercises 35s/45s/10s cases at
  `:970-992`.

## 5. Current Rust surfaces

### 5a. `wait_before_main_retry` (the gap)

`rust/crates/hermes-gateway/src/native_agent.rs:3007-3030`. Signature
`async fn wait_before_main_retry(&self, attempt, base_url, error)`. Duration is
`jittered_backoff(attempt, base, 60.0, 0.5)` (`:3017`) or
`adaptive_rate_limit_backoff(...)` when an error is passed (`:3018-3028`). The
sleep is a single, non-interruptible call:
`tokio::time::sleep(Duration::from_secs_f64(wait)).await` at `:3029`. There is no
`select!`, no cancellation token, no interrupt check, and no activity heartbeat
inside it. The sibling `wait_before_empty_response_retry` at `:3032-3039` has the
same shape.

Call sites, all `.await` on the bare sleep: `:2794` (pre-header timeout retry),
`:2826` (transport-error retry), `:2892` (retryable status retry), and `:4787`.

This is the structural counterpart to Python's chunked backoff loop
(section 1a), minus every interrupt/steering/heartbeat affordance.

### 5b. Timeout surfaces

`rust/crates/hermes-gateway/src/main_provider_timeouts.rs`. `resolve` (`:112`)
builds `request_timeout` (`:184`), `stream_stale_base`, `buffered_stale_base`
with implicit/explicit flags (`:26-27`, `:145-147`), and `stale_giveup` (`:180`).
`buffered_stale_timeout_capped` (`:260-283`) mirrors Python's run-budget cap:
`cap = (remaining * 0.5).max(60.0)`, only when not explicit. Applied in
`native_agent.rs:2740-2762` and header timeout at `:2764-2804`.

### 5c. Run budget

`native_agent.rs`: `run_budget_seconds` / `run_budget_started_at` fields
(`:1919-1920`), `with_run_budget_seconds` (`:2224`, via
`normalize_run_budget_seconds`), `begin_run_budget_turn` stamps the clock
(`:2234-2243`), `run_budget_remaining` computes
`budget - (SystemTime::now() - started)` (`:2245-2254`). Turn entry stamps it at
`:5174`. Like Python, the budget is wall-clock, so any time spent in
`wait_before_main_retry` is implicitly charged against it, and the retry sleep is
invisible to the budget as anything other than elapsed time.

### 5d. Stale streak

`native_agent.rs`: `consecutive_stale_streams` atomic, `reset_stale_stream_streak`
(`:2256-2259`), `stale_stream_giveup_error` (`:2261`). Reset sites: `:2448`,
`:2533`, `:2574`, `:4854`, `:6082` (turn boundaries and provider swaps). The
streak is bumped on real stale detections (`:2815-2817` on buffered timeout). No
Rust path bumps the streak for a user-interrupted pre-response wait, because no
Rust wait is interruptible (section 5e). The Python
`_record_interrupted_provider_wait` behavior has no Rust equivalent today.

### 5e. Dispatcher cancellation and steering

`rust/crates/hermes-gateway/src/dispatch.rs`: each turn is spawned as its own
tokio task (`:179` from the inbound loop, agent task at `:695`, event drain loop
at `:717`). Cancellation is coarse `task.abort()` at the whole-task level, only
exercised in `cancelled_push_waiter_keeps_history_and_delivery_owned` (`:820`,
abort at `:869-870`). There is no `CancellationToken`, no `select!` over a
steering channel, and no mid-turn injected-message queue that reaches into the
agent turn. Searching `native_agent.rs` for interrupt/steer/redirect surfaces
returns only test-fixture strings (`:11107`, `:13157`), not a runtime interrupt
path. `StreamEvent` lives in `hermes-core/src/stream.rs` and carries no
interrupt/steer variant relevant here.

Backoff math itself is already tested in
`rust/crates/hermes-gateway/src/retry_utils.rs` (`backoff_delay_math_matches_python`,
`backoff_jitter_is_added_linearly`, `jittered_backoff_stays_in_bounds`, and the
adaptive-tier tests), so a seam that only changes how the sleep is awaited, not
the duration, leaves those green.

Consequence: a Rust turn cannot currently cut a backoff wait short on steering,
cannot end it early on interrupt, and cannot account an aborted pre-response
wait toward the stale breaker. An `abort()` drops the whole task, bypassing the
Python restart-with-redirect and the 30s stale-attribution logic entirely.

## 6. Narrowest safe production seam

The narrowest seam that preserves the append-only gateway model and does not
touch operator-notice rendering is to make the retry backoff wait cancellable and
attributable, without introducing a full interactive steering channel:

- Replace the bare sleep at `native_agent.rs:3029` with a wait that can observe a
  cancellation signal (for example a `select!` over the sleep and a
  cancellation future owned by the turn), so a cancelled turn stops waiting
  promptly instead of running the whole backoff before `abort()` tears it down.
  This is the single load-bearing line; the four call sites (`:2794`, `:2826`,
  `:2892`, `:4787`) do not need to change their control flow if the function
  returns a "was cancelled" signal they can propagate.
- Route the dispatcher's existing task cancellation (`dispatch.rs:869`) into that
  signal instead of a blind `abort()`, so teardown is cooperative. This keeps the
  history-and-delivery ownership guarantee that
  `cancelled_push_waiter_keeps_history_and_delivery_owned` already asserts.
- Interrupted-wait accounting (`_record_interrupted_provider_wait`, 30s
  threshold, `response_started` gate) belongs to the provider-response wait, not
  the backoff sleep. In Rust that maps to the buffered/stream stale path around
  `native_agent.rs:2815-2817`, where a cancellation before first output and past
  a 30s elapsed threshold would bump `consecutive_stale_streams` via
  `reset_stale_stream_streak`'s counterpart. This should be a distinct, pure
  helper (mirroring the Python free function) so it is unit-testable without a
  live provider.

Explicitly out of the seam: emitting any wait/retry notice text (owned by the
notice analyses), a full mid-turn steering/redirect channel (a larger surface
than this seam), and the liveness `_touch_activity` heartbeat (a live-surface
concern, not required on the append-only gateway).

## 7. Public tests to guard the seam (3 to 6)

Rust, current and to-extend:

1. `native_agent.rs:8893` `run_budget_caps_the_live_buffered_reasoning_stall`
   and `:8953` `run_budget_spares_a_live_explicit_buffered_deadline` pin the
   wall-clock run-budget cap and the explicit-timeout escape. Any wait-accounting
   change must keep these green.
2. `native_agent.rs:8829` `buffered_stale_breaker_stops_retries_and_survives_calls`
   and `:8687` `stale_stream_breaker_survives_turns_and_stops_network_replay` pin
   the stale-streak give-up and cross-turn survival, the counter the interrupted
   wait would feed.
3. `native_agent.rs:7232` `dropped_main_connections_retry_once_then_use_fallback`
   pins the retry-then-fallback path that runs through
   `wait_before_main_retry`; a cancellable sleep must not change its terminal
   outcome.
4. `dispatch.rs:820` `cancelled_push_waiter_keeps_history_and_delivery_owned`
   pins the cancellation ownership contract that cooperative teardown must
   preserve.

New public tests the seam would add (behavior mirrored from Python
`tests/run_agent/test_stream_stale_circuit_breaker.py:64`):

5. A cancelled backoff wait returns promptly and reports cancellation rather than
   sleeping the full duration.
6. A pre-response cancel past the 30s threshold with no output bumps the stale
   streak, while a cancel under 30s or after first output leaves it unchanged
   (direct analog of `test_interrupted_pre_response_wait_advances_streak`).
