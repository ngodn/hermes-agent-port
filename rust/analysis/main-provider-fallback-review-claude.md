# Review: native ordinary main-provider fallback

Scope: the uncommitted working-tree diff on `rust-rewrite` touching
`compression_auxiliary.rs`, `main.rs`, and `native_agent.rs`. Compared against
`rust/analysis/main-provider-fallback-contract-agy.md`,
`rust/tools/gen_main_provider_fallback_goldens.py`, and
`rust/tools/main-provider-fallback-goldens.json`. I read the live working tree,
not the git snapshot. Findings are ranked most severe first. Each was checked
against source before reporting.

## F1. Turn-start restoration ignores the primary pool reset window (Medium)

File: `rust/crates/hermes-gateway/src/native_agent.rs:1711-1729`
(`restore_primary_route_for_turn`).

`restore_primary_route_for_turn` gates the return to the primary route on one
signal only: `state.cooldown_until`, which is the local rate-limit backoff
(60s doubling to a 4h cap, armed in `activate_main_fallback` at
`native_agent.rs:1802-1808`). The contract requires a second, independent gate:
the primary credential pool's provider-reported reset window
(`next_available_at` / `last_error_reset_at`). See contract section 6.3 and
section 11.2 gate 3, and the MVP list in section 15 step 5 ("check
`rate_limited_until <= now` AND pool `next_available_at <= now`"). The oracle
locks this in with `reset_aware_gate_blocks_restore_when_future_reset`
(`gen_main_provider_fallback_goldens.py:626-651`). Grep confirms
`next_available_at` is never referenced from `native_agent.rs` or `main.rs`, so
the gate is absent, not merely untested.

Failure scenario: a subscription-style primary (for example the `gmi` pool used
in the new `native_main_pool_exhausts_before_cross_provider_fallback` test)
returns 429 with a reset one hour out. Fallback activates and arms only a 60s
local cooldown. On the next user turn 60s later, `restore_primary_route_for_turn`
sees the cooldown elapsed, resets `active = 0`, and dispatch sends a doomed
request to the still-exhausted primary. That request 429s, rotates the pool to
another already-exhausted key, and re-falls-back, re-arming a fresh cooldown and
flipping the system-prompt prefix from the fallback identity back to the primary
identity and then forward again. Every turn inside the provider's reset window
pays one wasted primary round trip plus a prompt-cache prefix flip, which is the
exact waste section 6.3 says the gate exists to prevent. It self-heals (the
backoff escalates toward the 4h cap), so this is bounded, not a hang.

Smallest safe fix: in `restore_primary_route_for_turn`, before resetting
`active`, also consult the primary route's pool reset. The pool infrastructure
already exposes it (`credential_pool.rs:1190` `next_available_at`), and the
primary route is `self.main_route(0)` carrying `self.main_pool`. Because the
read touches `auth.json`, cache the reset deadline into `MainFallbackState`
(alongside `cooldown_until`) whenever `send_main_request` rotates or exhausts the
pool, then have `restore_primary_route_for_turn` stay on the fallback while
either `cooldown_until` or that cached pool deadline is still in the future.
This keeps the function synchronous and lock-only.

## F2. No exhaustion floor cooldown after a non-rate-limit chain wipeout (Low)

File: `rust/crates/hermes-gateway/src/native_agent.rs:1815` (`activate_main_fallback`
returns `None`) and `native_agent.rs:1846-1852` (dispatch converts that into the
terminal error).

Contract section 6.2 says that when the whole fallback chain exhausts and the
triggering failure was not a rate-limit or billing event, the primary cooldown is
floored at `now + 5s` (`_FALLBACK_EXHAUSTED_COOLDOWN_S = 5.0`) to stop
cross-turn replay storms. When `activate_main_fallback` runs out of candidates it
returns `None` and dispatch returns the terminal error without touching
`cooldown_until`. If the failures were auth (which never arms a cooldown, see
`arms_primary_cooldown` at `native_agent.rs:927-932`), `cooldown_until` stays
`None`, so the very next turn restores straight back to the primary and marches
the entire chain again.

Failure scenario: primary and every fallback are returning 401 (a revoked shared
key, an expired org). Back-to-back turns each replay primary plus every fallback
with zero damping, instead of the 5s floor Python applies.

Smallest safe fix: when `activate_main_fallback` returns `None` for a failure
whose class is not rate-limit or billing, set
`state.cooldown_until = max(existing, now + 5s)` before dispatch surfaces the
terminal error.

## Deferred transport and retry features (not defects)

These are consistent with the seam analysis boundary and the MVP scope; calling
them out so they are distinguished from the findings above.

- Transport, timeout, overloaded, and generic 5xx failover are not wired.
  `send_main_request` maps connection or timeout errors to
  `MainRequestError::Internal` (`native_agent.rs:1878-1880`), and
  `activates_provider_fallback` (`native_agent.rs:916-925`) excludes everything
  except `Auth`, `Billing`, `BillingUnverified`, `RateLimit`, and
  `UpstreamRateLimit`. So `timeout`, `overloaded`, `server_error`,
  `content_policy_blocked`, `model_not_found`, `context_overflow`, and
  `ssl_cert_verification` never advance the chain, unlike the ten Python trigger
  points in contract section 4.1. The one item inside the MVP boundary is
  "transport timeout" (contract section 15 step 3); the rest are the deferred
  retry surface. None is a must-fix for this checkpoint, but the transport
  timeout path is the first thing the next checkpoint should close.
- On restoration the primary pool is not proactively re-selected via
  `pool.select()` (contract section 8.2). `MainPoolCredential::route`
  (`native_agent.rs:991`) returns the last-installed credential, and recovery is
  reactive inside `send_main_request` rather than a fresh select at turn start.
  Functionally equivalent, one extra rotation in the worst case.
- A per-entry `reasoning_config` on a main fallback entry is dropped.
  `build_native_main_fallback_client` resolves reasoning from the global config
  and fallback model (`main.rs:1075`,
  `reasoning_effort::resolve_config(user_config, model)`) and never reads
  `entry.reasoning_config` (parsed at `compression_auxiliary.rs:106`). Likewise
  `entry.max_output_tokens` is ignored in favor of the primary
  `model.max_tokens` and the custom-provider `max_output_tokens` at
  `main.rs:1082`. `reasoning_echo` is honored (`main.rs:1152`,
  `compression_auxiliary.rs` parse), so only the effort and cap overrides are
  lost. Minor.

## Checked against source and matches the contract

Recorded so the coverage is explicit, not as endorsement.

- Chain parse, container coercion, merge order, case-insensitive and
  trailing-slash dedup: golden-locked by
  `main_turn_fallback_parser_matches_source_executed_python_corpus`
  (`compression_auxiliary.rs` test) against section
  `container_and_chain_parsing`.
- Same-backend skip including empty base URLs, sibling models, distinct explicit
  endpoints, first-class provider pairs (`xai` vs `xai-oauth`), and custom shim
  aliases: `should_skip_candidate` rewrite (`compression_auxiliary.rs:76-91`)
  plus `first_class_provider` from `provider_profile.is_some()`
  (`native_agent.rs:2118-2123`), golden-locked by
  `main_turn_backend_skip_matches_source_executed_python_corpus`. Applied both at
  build-time dedup (`main.rs:1435-1447`) and runtime advance
  (`native_agent.rs:1791-1796`).
- Pool-before-fallback ordering is structural: the chain only advances on
  `send_main_request`'s terminal return, and rotation runs to pool exhaustion
  first (`native_agent.rs:1909-1970`), matching section 5.1.
- Upstream rate limit bypasses the pool and fails over immediately:
  `UpstreamRateLimit` returns `Terminal` before rotation
  (`native_agent.rs:1896-1906`) yet is in `activates_provider_fallback`, matching
  section 5.2.
- Typed error propagation: `MainRequestError::Terminal` versus `Internal`
  correctly branches fallback eligibility on pre-body HTTP status only; internal
  and transport errors do not fall over. Streaming recovery is pre-body only
  (`dispatch_main_turn` returns a success `Response` before `forward_sse`), so no
  double emit.
- Prompt prefix stability: the primary route sends the untouched cached prefix
  (index 0 uses `self.system_prompt`); each fallback route carries a frozen
  variant with only the last `Model:` / `Provider:` lines rewritten
  (`rewrite_last_prompt_line`, `native_agent.rs:1315-1333`), golden-locked by
  `fallback_prompt_identity_matches_source_executed_python_corpus`. Verified end
  to end by the `preserves_static_prompt_prefix` and auth-restore tests.
- Cooldown escalation formula `min(60 * 2^n, 14400)` with `shift.min(8)` and the
  arm-only-when-leaving-primary rule (`failed_index == 0`) matches section 6.1
  and the golden backoff progression.
- Tool-round and cross-turn stickiness: the cursor lives in a shared
  `Arc<Mutex<MainFallbackState>>` cloned into `turn_client` and into the
  `TranscriptModel` inner reference, so it sticks across tool rounds and holds
  across turns while the cooldown is live. Covered by
  `sticks_across_tool_rounds`.
- Usage attribution: usage accumulates on the shared conversation bucket
  (`begin_main_usage` / `take_main_usage` over the shared `usage_state` Arc)
  while the provider, model, and base_url label follow the serving route via
  `active_main_route` in both `finalize_turn_after_persist`
  (`native_agent.rs:3909-3915`) and `step`'s `capture_usage`
  (`native_agent.rs` step). Read after the turn, before the next turn's restore,
  so no misattribution under serialized turns.
- Provider, header, and credential isolation: each route sends through its own
  `main_pool`, `provider_headers`, and client via `route.send_main_request`;
  fallback routes are built with independent pools and headers, so a fallback's
  401/429 rotates only its own pool and never quarantines the primary
  (structurally satisfies section 8.1).
- Cancellation and mutex discipline: no fallback-state lock is held across an
  `await`; every `state.lock()` in dispatch, activate, and restore is a scoped
  statement.
- Recursion: candidate skipping is a bounded `while` loop in
  `activate_main_fallback`, not the recursive `try_activate_fallback` shape, so
  traversal is bounded by chain length.
- Startup resolution: `build_native_main_fallback_client` rejects non
  `chat_completions` transports (`main.rs:975-978`), resolves credentials in the
  static then pool then profile then endpoint order, and the caller skips routes
  resolving to the primary backend before installing the frozen plan
  (`main.rs:1433-1459`).
