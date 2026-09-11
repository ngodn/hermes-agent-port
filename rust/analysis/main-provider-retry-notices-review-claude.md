# Review: native main-provider retry-notice implementation

Scope: the current working-tree diff of `rust/crates/hermes-gateway/src/main_provider_notices.rs`
and `rust/crates/hermes-gateway/src/native_agent.rs` against commit `10870a4846`.
This is a review lane. No production code, tests, PORT.md, INDEX.md, progress files, or
existing analysis were edited. Only this file was authored.

## Method and ground truth

- Read the full `git diff 10870a4846` for both files, then re-read the current source
  because the working tree is being edited by a concurrent operator-notice lane while this
  review ran (see the moving-target note below). All line numbers below are from the
  working tree at review time, not from my first diff snapshot.
- Built and ran the affected tests to distinguish real defects from mid-edit breakage:
  - `cargo test -p hermes-gateway --bin hermes-gateway main_provider_notices` -> 6 passed.
  - `cargo test ... -- retry_wait terminal_main zai_long recovered_main terminal_format terminal_http_summary` -> 7 passed.
  The current tree compiles and every retry-notice test is green.

## Moving-target caution (not a code defect)

While reviewing, the tree changed under me. My first `git diff` snapshot showed
`record_main_terminal_http(class, status, body)` (3 args) and terminal strings hard-coded to
`self.main_retry.max_attempts`. The current tree instead has a plumbed `attempts: usize`
carried on `MainTerminal` (native_agent.rs:1110) and `MainRequestError::Fallback` (native_agent.rs:1125),
extra terminal arms at native_agent.rs:2864 and 2873, and the terminal recorders take an
`attempts` parameter (native_agent.rs:2692-2742). A concurrent lane is actively refining this
surface. Anyone acting on this review should re-diff first; findings below are pinned to the
state I actually read.

Note on the plumbed `attempts`: every construction site passes `attempts: max_attempts`
(native_agent.rs:2979, 3017, 3098, 3115, 3185, 3194), so `attempts` is really "the retry
ceiling in force at failure time" (including the Z.AI overload bump), not the count of
attempts actually made. The name reads like a live count and it is not. Cosmetic, but worth
renaming to `max_attempts` to avoid a future reader wiring in the wrong value.

## Verified defects, ranked by severity

### 1. (Medium) Retry-notice denominator overstates the real per-route attempt cap

`wait_before_main_retry` always renders the denominator as `max_attempts`
(native_agent.rs:3240, 3244), but the loop's actual give-up threshold is
`main_attempt_limit(failure, max_attempts, has_fallback)` (native_agent.rs:1093-1105), which
returns `max_attempts.min(2)` when a fallback route exists and the failure is `Transport` or
`Overloaded`.

Failure scenario: primary route configured with a fallback (per prior analysis, gateway
fallback routes are the normal cross-provider setup), primary returns 503 Overloaded with
`max_attempts = 3`. The user sees `⏳ Retrying in Xs (attempt 1/3)...`, then the route gives
up after the second failure (`request_failures = 2`, `2 < 2` is false) and switches to
fallback. The `/3` denominator implies two more attempts on this route that never happen.

This is worse under Z.AI Coding overload: `max_attempts` is bumped to the Z.AI ceiling (e.g.
8) at native_agent.rs:3055-3058, so the notice reads `attempt 2/8` while the primary route is
still capped at 2 attempts by `main_attempt_limit`. The denominator wildly overstates the
real ceiling for that route, and it also signals a `/8` budget that the fallback-present cap
deliberately defeats. The existing test `terminal_main_fallback_failure_flushes_ordered_retry_and_switch_trace_once`
encodes this behavior (one `attempt 1/3` on the primary before the switch), so it is baked in,
not accidental drift, but it is still a misleading number shown to the operator.

Same class of mismatch on the stale-stream retry path: native_agent.rs:5040-5047 passes
`route.main_retry.max_attempts` as the denominator while the loop's own `outer_limit` is
`max_attempts.min(2)` when a fallback exists (native_agent.rs:5033-5037).

Parity note: I cannot confirm from this lane whether the live Python report prints the
configured ceiling or the effective per-route cap. The mismatch is verifiable inside Rust
regardless; whether `max_attempts` is the intended display value is the parity question.

### 2. (Low-Medium) Numerator convention differs between the generic and Z.AI branches

For the same `attempt` value, the two branches of `wait_before_main_retry` number the attempt
differently:
- generic: `attempt {attempt}/{max_attempts}` (native_agent.rs:3244), i.e. the count of
  failures completed so far (`request_failures`, 1-based).
- Z.AI short/long: `attempt {}/{max_attempts}` with `attempt.saturating_add(1)`
  (native_agent.rs:3240-3241), i.e. the upcoming attempt number.

`request_failures` is incremented before every `wait_before_main_retry` call
(native_agent.rs:2960, 2998, 3077), so after the first failure the generic message says
`attempt 1/...` and the Z.AI message says `attempt 2/...` for the identical loop position.
On a Z.AI route that mixes an overload retry (Z.AI branch, +1) with, say, a transport retry
(generic branch, no +1) inside one turn, the operator sees the attempt counter jump
inconsistently. Verifiable in Rust. Which convention matches Python is unverified here.

### 3. (Low-Medium) Z.AI long-wait notice is live-only, so it can invert order and has no buffered fallback

The `zai_coding_overload_long` branch sends the notice immediately over the live channel via
`try_send` (native_agent.rs:3246-3258) and never records it to `main_notices`. Every other
retry notice is buffered and flushed at turn end (native_agent.rs:5514-5529, after the model
task finishes and before the held final `MessageStop`). Two consequences:

- Ordering inversion. In the normal Z.AI overload escalation (several short attempts first,
  then the adaptive long backoff), the short-wait notices are buffered and only flush at turn
  end, while the long-wait notice goes out live mid-turn. On the wire the operator sees the
  long-wait `attempt N` before the earlier short-wait `attempt N-1`, so the retry trace is
  out of recorded order. On terminal failure the buffered short waits are appended after the
  long wait that already shipped.
- No fallback path. If `main_live_notice_tx` is `None`, the long-wait notice is silently
  dropped (native_agent.rs:3252, `if let Some(live)` with no else), rather than falling back
  to the buffer. Today the live sender is always set on the turn client
  (native_agent.rs:5396-5397) and inherited by routes (native_agent.rs:2504), so the
  `None` case looks unreachable on the run-turn path, but the long wait, which is the one the
  operator most wants to see, is the only notice with no durable fallback. A `try_send` drop
  under channel backpressure has the same effect: the most important notice is the least
  reliably delivered.

These are design consequences of the short-vs-long split rather than crashes. Whether Python
also emits the long wait "live" and out of trace order is unverified in this lane. If the
intent is real-time feedback for long waits, an alternative that both emits live and records
a buffered copy (deduped at flush) would remove the inversion and the drop hazard.

## Cleared: checks that came back clean

- Buffer clear/flush ordering. `TurnNotices::drain` (main_provider_notices.rs:61-69) clears
  `buffered` and returns `pending_durable` on recovery, and clears `pending_durable` and
  returns `buffered` on terminal failure. `record_fallback`/`record_primary_restore` push to
  both vectors; `record_transient` pushes only to `buffered`. So recovery keeps only durable
  switches/restores in recorded order and drops transient chatter; terminal failure flushes
  the full buffered trace once with no duplication of the durable copies. The new tests at
  main_provider_notices.rs:101-143 pin exactly this, and they pass.
- Retry numerators feeding `record_transient`. Each retry path increments `request_failures`
  before the wait call, so no attempt is skipped or double-counted in the numerator (the
  denominator issue is Finding 1, separate).
- Route reset on fallback. `activate_main_fallback_with_status` records the attempt notice and
  the switch notice only when a next route exists (native_agent.rs:2574-2593), resets the
  failed route's stale-stream streak, arms the primary cooldown only for cooldown-arming
  failures, and sets `state.active`. On exhaustion (no next route) it records nothing and lets
  the dispatch arm emit the terminal notice (native_agent.rs:2842-2849). No double emission.
- Format-rejection reason. `FormatError::notice_reason()` is now "provider failure"
  (native_agent.rs:1002) with a comment that Python's non-retryable client-error branch
  activates fallback without forwarding the classifier reason. The `fallback_attempt` line for
  FormatError still carries the HTTP code ("Non-retryable error (HTTP 500)",
  native_agent.rs:2668-2676) and the terminal line does too (native_agent.rs:2711-2716), while
  only the switch line drops to "provider failure". Internally consistent and test-pinned
  (`terminal_format_rejection_flushes_nonretryable_fallback_trace`). Exact parity with the
  live Python string set is unverified in this lane, but the change is deliberate and
  documented.
- Terminal-summary secret safety. `main_http_error_summary` (native_agent.rs:1684) extracts
  `error.message`/`message`, collapses whitespace, caps at 300 chars, and routes the result
  through `compression_redact::redact` (native_agent.rs:1706). `redact`
  (compression_redact.rs:8-51) covers known key prefixes (including `sk-`), Bearer/Basic,
  URL creds, JSON token fields, query params, and `key=value` pairs, so
  `api_key=sk-...` is scrubbed (test `terminal_http_summary_extracts_and_redacts_provider_message`).
  `record_main_terminal_error` also redacts the error string (native_agent.rs:2731). Residual
  risk is only a provider echoing a bare high-entropy secret with no prefix and no label,
  which is a general `redact` limitation, not something this diff introduces.
- Mutex/await safety. Every `main_notices` and `main_live_notice_tx` lock is a short
  `std::sync::Mutex` critical section that ends before any `.await`: `wait_before_main_retry`
  drops the lock (or clones the sender out of it) before `tokio::time::sleep`
  (native_agent.rs:3246-3267); the turn-end drain drops the lock before the send loop
  (native_agent.rs:5514-5529). No lock is held across an await, and all locks use
  `unwrap_or_else(|e| e.into_inner())` so a poisoned mutex cannot panic the turn.
- mpsc backpressure. The live path uses non-blocking `try_send` (native_agent.rs:3253), so it
  cannot deadlock against a slow or full consumer; the cost is a possible silent drop
  (covered under Finding 3). The turn-end flush uses awaiting `send`, matching the existing
  notice-delivery pattern.
- Prompt and transcript isolation. Notices flow only as `StreamEvent::GatewayNotice`. The turn
  accumulates `response` from `MessageChunk` text only (native_agent.rs:5499-5501); notices
  never touch `model_content`, `durable_history`, or the persisted reply. No transcript or
  prompt-cache contamination from the retry trace.
- Fallback clone ownership. `run_native_turn` clones the client, `mem::take`s the base
  client's notices into a fresh per-turn `Arc<Mutex<TurnNotices>>`, and installs a fresh
  `Arc<Mutex<Some(events)>>` for the live sender (native_agent.rs:5387-5397). Routes made from
  the turn client share both Arcs (native_agent.rs:2503-2504), so every fallback route records
  into the same turn buffer and the same live channel. The taken base buffer carries only the
  cross-turn durable notices (primary restore), consistent with prior analysis. The live
  sender clone is dropped when the turn client drops at function end, so it does not keep the
  caller's channel open past the turn.
- Existing retry budgets. The `base <= 0.0` reshaping (native_agent.rs:3210-3228) changes the
  old early-return into "wait 0.0s, still record the notice, skip the sleep." Wall-clock budget
  and retry counts are unchanged; the only behavior change is that a backoff-disabled config
  now records `Retrying in 0.0s` notices (dropped on recovery, shown on terminal). The
  `adaptive_rate_limit_backoff` call now keeps the `(wait, policy)` tuple instead of `.0`; the
  wait value is unchanged and `policy` only drives notice text and the live-vs-buffered split.

## Test coverage gaps worth closing

High value, currently missing:
- No test asserts the Finding 1 denominator behavior explicitly (that a fallback-present
  Overloaded/Transport route shows `/max_attempts` while capped at 2 attempts). The fallback
  test encodes it implicitly but does not name or lock the intended denominator, so a future
  change to `main_attempt_limit` would not trip a clearly-labeled expectation.
- No test for the Z.AI long-wait ordering relative to earlier buffered short waits within one
  turn (Finding 3 inversion). The existing `zai_long_...` test drives `wait_before_main_retry`
  directly and checks the two notices in isolation, not their interleaving with a buffered
  trace on the wire.
- No test for the long-wait drop when `main_live_notice_tx` is `None` or the live channel is
  full. Even if unreachable today, a test would pin the "long wait must not vanish" contract.
- `record_main_terminal_error` redaction (the transport/fallback terminal path) is untested;
  only `main_http_error_summary` redaction is covered. A secret in a reqwest error string
  would rely on unverified redaction.

Lower value:
- The two non-fallback-activating terminal arms (native_agent.rs:2864, 2873) added by the
  concurrent lane are exercised only indirectly by the 502 terminal test; a direct case for a
  non-activating class would be cleaner.
- `StreamInactivity` (native_agent.rs:2879) and the catch-all `Err` (native_agent.rs:2889)
  return without a `terminal_failure` notice. If internal/inactivity terminations are meant to
  stay silent, a short assertion documenting that would prevent an accidental notice later.

## Bottom line

No crashes, no lock-across-await, no transcript or secret leakage, and the buffer
clear/flush ordering matches the durable-vs-transient contract with passing tests. The
substantive issues are presentation-fidelity: Finding 1 (denominator overstates the real
per-route cap, especially under the Z.AI ceiling bump) is the one worth resolving before
merge; Finding 2 (numerator convention split) and Finding 3 (live-only long wait: ordering
inversion plus no buffered fallback) are lower and partly depend on unverified Python parity.
Because the file is under concurrent edit, re-diff against the current tree before acting.
