# Main-provider run-budget-aware stale scaling: Python map and Rust seam

**Document target:** `rust/analysis/main-provider-run-budget-scaling-claude.md`
**Scope lane:** the one place where the main-provider timeout depends on how much
wall-clock run budget is left. This is the checkpoint PORT.md calls
"run-budget-aware stale scaling" (`rust/PORT.md:100`, and listed as remaining at
`:28` and `:67`).
**Explicitly out of scope (per task):** dropped-stream recovery, Ollama/GLM stop
correction, operator notice delivery, non-chat transports, OAuth, plugins, and
any implementation. This document only maps Python, compares Rust, and proposes
a seam plus tests.

All line references below were read against the current checkout on the
`rust-rewrite` branch.

---

## 1. One-paragraph answer

Python has exactly one main-provider timeout that scales with remaining run
budget: the **non-stream (buffered) stale timeout**. When a wall-clock run budget
is active and the user did not explicitly configure a stale timeout, the implicit
timeout is capped at `max(60.0, remaining * 0.5)`, and only when that cap is
lower than the value the detector would otherwise use. It never raises the
timeout, never touches an explicitly configured value, and touches nothing else
in the timeout or retry system. The Rust native port already froze the rest of
that buffered-stale computation in `main_provider_timeouts.rs`
(`buffered_stale_timeout`) but has no run-budget input at all, so the cap is the
single missing piece. Nothing else in the run-budget or iteration-budget
machinery scales a timeout, a retry count, or a backoff.

---

## 2. The exact Python behavior

### 2.1 Where it lives (ownership)

The cap lives on the agent object, not in the shared timeout helpers:

- `run_agent.py:1583` `AIAgent._compute_non_stream_stale_timeout(self, api_payload)`
  computes the effective non-stream stale timeout and applies the cap at the end.
- `run_agent.py:1542` `AIAgent._resolved_api_call_stale_timeout_base(self)` returns
  `(timeout_seconds, uses_implicit_default)` with precedence
  provider/model `stale_timeout_seconds` -> provider `stale_timeout_seconds` ->
  `HERMES_API_CALL_STALE_TIMEOUT` env -> reasoning-model floor -> `90.0` default.
  Only the last two are "implicit" (`uses_implicit_default` is `True` only for the
  bare `90.0`; the reasoning floor returns `False`).
- `run_agent.py:1622` `AIAgent._stale_timeout_is_explicit(self)` returns `True`
  only when provider/model `stale_timeout_seconds` (via
  `hermes_cli/timeouts.py:get_provider_stale_timeout`) or the env var
  `HERMES_API_CALL_STALE_TIMEOUT` is set. The reasoning floor and the `90.0`
  default are NOT explicit, so they yield to the cap.

The state the cap reads:

- `agent.run_budget_seconds` (float or `None`). Normalized by
  `agent/agent_init.py:493` `_normalize_run_budget_seconds`: `None`, non-numeric,
  `bool`, `NaN`, and non-positive all become `None` (feature off). Set at
  `agent/agent_init.py:1029` from the constructor arg and, if still `None`, from
  config `agent.run_budget_seconds` at `agent/agent_init.py:2052-2057`. The CLI
  flag `--run-budget` feeds it through
  `hermes_cli/cli_agent_setup_mixin.py:537`.
- `agent._run_budget_started_at` (float or `None`). Stamped per turn in
  `agent/turn_context.py:761-762` at the top of `run_conversation`: `time.time()`
  when a budget is set, else `None`. This is a wall-clock (`time.time()`), not
  monotonic, and it is re-stamped every turn. So `remaining` is measured from the
  current turn's start, not from a persistent run-wide deadline. The Rust port
  must replicate this turn-scoped stamp, not a session-global clock.

### 2.2 The computation, in order (`run_agent.py:1583-1620`)

```
stale_base, uses_implicit_default = _resolved_api_call_stale_timeout_base()
if uses_implicit_default and base_url and is_local_endpoint(base_url):
    return inf                         # local implicit -> watchdog disarmed
est = estimate_request_context_tokens(api_payload)
if est > 100_000:  timeout = max(stale_base, 240.0)
elif est > 50_000: timeout = max(stale_base, 150.0)
else:              timeout = stale_base
# run-budget cap (the missing behavior):
run_budget = getattr(self, "run_budget_seconds", None)
if run_budget and not self._stale_timeout_is_explicit():
    started = getattr(self, "_run_budget_started_at", None)
    if started:
        remaining = float(run_budget) - (time.time() - started)
        deadline_cap = max(60.0, remaining * 0.5)
        if deadline_cap < timeout:
            timeout = deadline_cap
return timeout
```

Ordering that matters:

1. The **local-endpoint short-circuit returns `inf` before the cap is reached**.
   So an implicit-default local endpoint is never capped; it stays unbounded.
   The cap can only ever apply to a finite, non-local, implicit timeout.
2. Context scaling (`240`/`150` floors) runs **before** the cap. The cap is
   applied to the already-context-scaled value.
3. The cap is a pure downward clamp: `if deadline_cap < timeout: timeout = cap`.
   It never raises.

### 2.3 Bounds

- **Lower bound:** `60.0` seconds. `remaining * 0.5` can be small or negative
  (`time.time()` is wall clock, and a turn can start after most of the budget is
  already spent, or the clock can jump), so `max(60.0, ...)` floors it. A run
  budget can therefore never drive the buffered stale timeout below 60s.
- **Upper bound:** the pre-cap value (the context-scaled base, or the reasoning
  floor, or `90.0`). The cap is `min`-like against that; it cannot exceed it.
- **Half-remaining rationale (from the source comment):** so a single hung
  provider call cannot eat the whole run. The worked example in the comment is
  deepseek's 600s reasoning floor inside a 900s eval ceiling.

### 2.4 What it changes and what it does NOT

It changes **only the non-stream (buffered) stale watchdog budget**. In the
Python transport layer that value flows to:

- the buffered/non-streaming inline hard read backstop
  (`agent/chat_completion_helpers.py:1112` `_resolve_direct_stale_timeout` ->
  `_inline_nonstream_hard_timeout` at `:1137`, used by `direct_api_call` at
  `:1302-1310`), and
- the interrupt-worker non-stream stale detector
  (`agent/chat_completion_helpers.py:1591`
  `agent._compute_non_stream_stale_timeout(api_kwargs)`).

It does NOT change any of the following (verified by reading each site):

| Timeout / control | Site | Run-budget capped? |
| :--- | :--- | :---: |
| Total request timeout (`request_timeout_seconds` / `HERMES_API_TIMEOUT`, default 1800s) | provider config | No |
| Pre-header wait (streaming, no-tools) | `chat_completion_helpers.py` streaming path | No |
| SSE inactivity / stream stale timeout (default 180s, local 900s, context 240/300, reasoning floor) | `chat_completion_helpers.py:5464-5522`, `_derive_stream_stale_timeout` at `:818-853` | No |
| Stream socket read timeout (120s -> base) | streaming path | No |
| Stale outer/inner attempts (`HERMES_STREAM_RETRIES`, `HERMES_STREAM_STALE_GIVEUP`) | retry contract | No |
| Retry backoff (jittered exponential, 5s/120s and 2s/60s) | `conversation_loop.py:3948-3949`, `:7259-7260` | No |
| Empty-response retry budget, Z.AI overload ceiling, compression/thinking ceilings | retry contract | No |

I confirmed the streaming stale block at `chat_completion_helpers.py:5464-5522`
has no `run_budget` reference; the only run-budget influence in the whole streaming
derivation is none. `grep` for `run_budget` across `agent/` returns only the
wrap-up-notice path and `agent_init`/`turn_context` state setup.

### 2.5 The two other budget behaviors that are NOT this (so they are not confused)

These are budget-driven but do not scale a timeout or a retry, so they are out of
this checkpoint:

- **Run-budget wrap-up notice** (`agent/conversation_loop.py:198-242`, invoked at
  `:2432-2433`). At 80% elapsed it appends a one-shot text notice to the newest
  tool message. It is a prompt injection, not a timeout or retry change.
- **Iteration budget and grace call** (`agent/conversation_loop.py:2289`,
  `:2333-2339`). This gates loop entry and grants one extra "grace" iteration.
  It changes how many turns run, not any timeout or retry budget.

### 2.6 Cancellation semantics

There is no new cancellation path. The capped value is just a smaller number fed
to the existing non-stream stale watchdog. When it fires, the existing machinery
runs unchanged: the inline path raises a retryable `TimeoutError`
(`chat_completion_helpers.py:1275-1278` and the abort hook), the interrupt-worker
path aborts the in-flight socket through the already-registered abort hook, and
the stale streak breaker advances exactly as it does for an uncapped expiry. A
lower cap only makes that same expiry fire sooner. It does not add or remove a
cancellation, and it does not change which errors are retryable.

### 2.7 Source-executed proof and focused tests

The cap is captured as executed-source goldens by
`rust/tools/gen_main_provider_stall_goldens.py:641-668`, producing two cases in
`rust/tools/main-provider-stall-goldens.json`:

- `non_stream_run_budget_cap_caps_implicit_floor`
  (`main-provider-stall-goldens.json:172`): model `deepseek/deepseek-r1`
  (reasoning floor 600s), `run_budget_seconds=300`, started 100s ago ->
  `remaining=200` -> `cap = max(60, 100) = 100`, and `100 < 600`, so the executed
  result is `100.0`.
- `non_stream_run_budget_cap_spares_explicit_config`
  (`main-provider-stall-goldens.json:182`): same agent but with an explicit
  provider `stale_timeout_seconds=600` -> explicit wins untouched, result `600.0`.

The AGY liveness/stall contract already documented this cap as
"Wall-Clock Run Budget Cap" in
`rust/analysis/main-provider-stall-contract-agy.md:149-151`. Note that the
identifier names it uses (`_stale_timeout_is_explicit`, `remaining_budget`) are
methods/locals inside `run_agent.py`, not module-level symbols; verified they
exist only in `run_agent.py` and the golden generator, not in `agent/*.py`.

---

## 3. The Rust native side today

### 3.1 What exists

`rust/crates/hermes-gateway/src/main_provider_timeouts.rs` is the frozen liveness
policy. It already ports the non-run-budget parts of
`_compute_non_stream_stale_timeout`:

- `Policy::buffered_stale_timeout(self, base_url, body)`
  (`main_provider_timeouts.rs:227-242`): returns `None` for an implicit local
  endpoint (the Rust equivalent of Python's `inf` short-circuit), otherwise the
  context-scaled base with the same `240.0`/`150.0` floors.
- The implicit/explicit distinction is captured as the private field
  `buffered_stale_implicit` (`:26`, set at `:134-138`, `:162`). It is `true` only
  when the base fell through to `DEFAULT_BUFFERED_STALE_TIMEOUT_SECONDS` (90s);
  provider/model config, `HERMES_API_CALL_STALE_TIMEOUT`, and the reasoning floor
  all set it `false`. This mirrors Python's `_stale_timeout_is_explicit` for
  config/env, plus the reasoning-floor case.

Note one deliberate parity difference already baked into Rust: Python's
`uses_implicit_default` is `True` only for the bare 90s (the reasoning floor is
`uses_implicit_default=False` but `_stale_timeout_is_explicit=False`). Rust's
single `buffered_stale_implicit` flag folds both "90s default" and "reasoning
floor" into the same `true`/`false` used for the local short-circuit. For the
local short-circuit this is already consistent with Python because the reasoning
floor sets `uses_implicit_default=False` and Rust sets `buffered_stale_implicit=false`
for a reasoning floor, so a reasoning-floor local endpoint stays bounded in both.
For the run-budget cap the correct gate is Python's `_stale_timeout_is_explicit`
(config/env only), under which the reasoning floor IS eligible for the cap. See
the interface note in section 4.2 about not reusing `buffered_stale_implicit`
verbatim for the cap.

### 3.2 The two buffered call sites

`buffered_stale_timeout` is consumed at exactly two places in
`rust/crates/hermes-gateway/src/native_agent.rs`:

- `native_agent.rs:2638-2657`: the main streaming/buffered request loop. For the
  buffered path (`!label.is_empty()`) the value becomes the reqwest per-request
  timeout, `min` with `request_timeout()` (`:2653-2657`), and it feeds the
  stale-strike accumulation on timeout (`:2707-2711`).
- `native_agent.rs:5788-5790`: the `step()` non-streaming tool path. Same
  `min(request_timeout)` stale-strike logic on decode timeout (`:5793-5800`).

### 3.3 What is missing

There is **no run-budget input anywhere in Rust**. `grep` for `run_budget` /
`budget_seconds` across `crates/hermes-gateway/src` returns nothing. `Policy` is
`Copy` and stateless; `NativeAgentClient` has no run-budget field and no
turn-scoped wall-clock start stamp for budget purposes. So the missing behavior
is precisely: cap the value returned by `buffered_stale_timeout` at
`max(60.0, remaining * 0.5)` when a run budget is active, the timeout is implicit,
and the value is finite (`Some`).

Because the Rust `buffered_stale_timeout` returns `None` for the local implicit
case, the cap in Rust naturally applies only to `Some(..)` results, matching
Python's "the local `inf` short-circuit is reached before the cap" ordering.

---

## 4. Proposed narrowest deep Rust seam

Keep `Policy` pure and config-derived; keep the wall clock and per-turn budget
state on the agent. The cap formula and the explicit/implicit gate belong to
`Policy` (it already owns the config-derived distinction); the elapsed-time
computation belongs to the caller (it owns the clock and the turn).

### 4.1 One new method on `Policy`

```rust
/// Python run_agent._compute_non_stream_stale_timeout run-budget cap.
/// `run_budget_remaining` is `Some(seconds_left_in_this_turn)` only when a
/// run budget is active for this turn; `None` disables the cap. The cap is
/// applied only to an implicit (default- or reasoning-floor-derived) buffered
/// stale timeout, never to an explicitly configured one, and only downward.
pub fn buffered_stale_timeout_capped(
    self,
    base_url: &str,
    body: &Value,
    run_budget_remaining: Option<f64>,
) -> Option<Duration> {
    let base = self.buffered_stale_timeout(base_url, body)?; // None => unbounded, no cap
    let Some(remaining) = run_budget_remaining else { return Some(base); };
    if self.stale_timeout_is_explicit() {
        return Some(base);
    }
    let cap = (remaining * 0.5).max(60.0);
    Some(if cap < base.as_secs_f64() {
        Duration::from_secs_f64(cap)
    } else {
        base
    })
}
```

with a small helper that mirrors Python's `_stale_timeout_is_explicit` exactly
(config/model or env only, NOT the reasoning floor):

```rust
fn stale_timeout_is_explicit(self) -> bool { self.buffered_stale_explicit }
```

### 4.2 One field-shape note

Do NOT gate the cap on the existing `buffered_stale_implicit` field. That field
is `false` for the reasoning floor, but Python's `_stale_timeout_is_explicit` is
also `false` for the reasoning floor, so under Python a reasoning-floor buffered
timeout IS eligible for the cap (and the golden
`non_stream_run_budget_cap_caps_implicit_floor` proves it: deepseek-r1's 600s
reasoning floor is capped to 100s). The seam therefore needs a distinct
"explicit" bit that is `true` only for provider/model config or
`HERMES_API_CALL_STALE_TIMEOUT`, matching `run_agent.py:1622-1632`. Record it at
`Policy::resolve` as a second boolean (`buffered_stale_explicit`) computed from
the config/env branches only, leaving `buffered_stale_implicit` unchanged for the
local short-circuit. This is the one-line addition that keeps the two gates
(local disable vs run-budget cap) from being conflated.

### 4.3 Agent-side plumbing (caller owns the clock)

- Add `run_budget_seconds: Option<f64>` to `NativeAgentClient`, populated the
  same way as the other frozen config (a builder such as `with_run_budget`,
  fed from `agent.run_budget_seconds` in the gateway config with the same
  normalization as `_normalize_run_budget_seconds`: reject bool/NaN/non-positive
  to `None`).
- Stamp a turn-scoped wall-clock start at the top of `run_turn` /
  `run_turn_with_context` (`native_agent.rs:5206`, `:5216`) only when a budget is
  set, matching `turn_context.py:761-762`. Use a wall clock (`SystemTime`) to
  match Python's `time.time()` semantics, and re-stamp each turn.
- At the two call sites (`:2640-2641`, `:5788-5790`) compute
  `run_budget_remaining = run_budget_seconds.map(|b| b - elapsed_since_turn_start)`
  and call `buffered_stale_timeout_capped(...)` instead of
  `buffered_stale_timeout(...)`. Everything downstream (`min(request_timeout)`,
  stale-strike accumulation) is unchanged.

For deterministic tests, inject the clock: store the turn start as a value the
test can set (for example an optional `turn_started_at` override on the client, or
a clock trait), so a test can force "most of the budget already elapsed" without
sleeping.

### 4.4 Why this is the narrowest deep seam

- It adds one method and one boolean to `Policy`, both pure, both derivable at
  `resolve` time.
- It leaves the streaming, pre-header, request-timeout, attempt-count, and
  backoff paths untouched, exactly as Python does.
- It puts the only non-pure piece (wall clock, per-turn start) on the agent,
  which already owns turn lifecycle, and passes the result in as a plain `f64`.
- It reuses the existing `min(request_timeout)` and stale-strike code with no
  change, so cancellation and circuit-breaker semantics are identical.

---

## 5. Proposed public run-turn tests

These follow the existing harness (axum mock via `serve_main_retry`,
`Policy::resolve`, `NativeAgentClient::new(...).with_provider_identity(...)
.with_main_timeouts(policy)...`, driving `run_turn`), as used by
`buffered_stale_breaker_stops_retries_and_survives_calls`
(`native_agent.rs:8118`) and `preheader_stale_uses_stream_retry_batch_before_fallback`
(`:8039`). The buffered path is exercised with a tool-call response so the
request takes the `!label.is_empty()` branch.

1. **Cap lowers an implicit buffered stale timeout.**
   Reasoning model (for example `deepseek/deepseek-r1`, implicit 600s floor), no
   provider `stale_timeout_seconds`, `with_run_budget(300)`, turn start forced to
   100s ago (remaining 200 -> cap 100). Mock buffered endpoint delays its JSON
   body past 100s of simulated time but under the uncapped 600s. Assert the
   request times out at the capped value (observable via the stale-strike
   increment / the bounded `run_turn` returning the timeout classification), and
   that with no run budget the same mock would not have tripped. Mirrors golden
   `non_stream_run_budget_cap_caps_implicit_floor`.

2. **Explicit provider stale timeout is spared.**
   Same reasoning model but provider `stale_timeout_seconds=600` set in config,
   `with_run_budget(300)` and 100s elapsed. Assert the effective buffered timeout
   stays 600 (cap not applied), i.e. the request that would trip at 100s does not
   trip. Mirrors golden `non_stream_run_budget_cap_spares_explicit_config`.

3. **60s floor holds when remaining is tiny or negative.**
   `with_run_budget(80)`, turn start forced so `remaining` is near zero or
   negative. Assert the effective buffered timeout is exactly 60s, never below,
   and never `panic`s on a negative `remaining` (the `max(60.0, ...)` floor).

4. **Cap never raises.**
   Small implicit base (plain non-reasoning model, 90s), large run budget
   (`with_run_budget(100000)`), fresh turn. Assert the effective buffered timeout
   stays 90s, not `remaining * 0.5`.

5. **Streaming path is untouched.**
   No-tools streaming turn (`label.is_empty()`) with a run budget set and mostly
   elapsed. Assert the SSE inactivity / stream stale deadline is unchanged from
   the no-budget case (the cap must not leak into `stream_stale_timeout` or
   `stream_inactivity_timeout`).

6. **Local implicit endpoint stays unbounded under a run budget.**
   Local base URL, implicit default, run budget set and mostly elapsed. Assert
   `buffered_stale_timeout_capped` returns `None` (no cap applied, matching
   Python reaching the `inf` short-circuit before the cap).

A `Policy`-level unit test should also assert `buffered_stale_timeout_capped`
byte-matches the two executed goldens for the exact `(model, run_budget,
elapsed)` triples, so the Rust cap is anchored to source-executed Python, not to
prose.

---

## 6. Verified reference list

Python:
- `run_agent.py:1542-1581` base resolver and precedence.
- `run_agent.py:1583-1620` non-stream stale computation and the run-budget cap
  block (`:1605-1620`).
- `run_agent.py:1622-1632` `_stale_timeout_is_explicit`.
- `agent/agent_init.py:493-509` normalization; `:1029`, `:2052-2057` resolution.
- `agent/turn_context.py:761-765` per-turn wall-clock start stamp.
- `hermes_cli/timeouts.py:43-69` `get_provider_stale_timeout`.
- `hermes_cli/cli_agent_setup_mixin.py:537` `--run-budget` plumbing.
- `agent/chat_completion_helpers.py:818-853` streaming derive (no cap);
  `:5464-5522` streaming stale block (no cap); `:1112-1163` inline non-stream
  backstop; `:1302-1310` inline apply; `:1591` worker non-stream apply.
- `agent/conversation_loop.py:198-242`, `:2432-2433` wrap-up notice (not this);
  `:2289`, `:2333-2339` iteration budget and grace (not this).
- `rust/tools/gen_main_provider_stall_goldens.py:641-668` and
  `rust/tools/main-provider-stall-goldens.json:172-186` executed goldens.
- `rust/analysis/main-provider-stall-contract-agy.md:149-151` prior AGY note.

Rust:
- `rust/crates/hermes-gateway/src/main_provider_timeouts.rs:19-46` `Policy`
  fields/defaults; `:26`,`:134-138`,`:162` `buffered_stale_implicit`;
  `:227-242` `buffered_stale_timeout`.
- `rust/crates/hermes-gateway/src/native_agent.rs:2638-2657`, `:2707-2711` main
  loop buffered site; `:5788-5800` `step()` buffered site; `:5206`,`:5216`
  `run_turn` entry points; `:8039`,`:8118` existing buffered/pre-header stale
  run-turn tests.
- `rust/PORT.md:28`,`:67`,`:100` "run-budget(-aware) scaling" listed as remaining.
