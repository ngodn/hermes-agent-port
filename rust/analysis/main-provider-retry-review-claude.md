# Main-Provider Retry Implementation Review (pre-body checkpoint)

**Reviewer lane**: Claude post-implementation review
**Scope**: the uncommitted ordinary main-provider retry work in the working tree
(`M native_agent.rs`, `M main.rs`), checked against
`rust/analysis/main-provider-retry-contract-agy.md`,
`rust/tools/main-provider-retry-goldens.json`, and the live Python sources
(`agent/error_classifier.py`, `agent/conversation_loop.py`,
`agent/retry_utils.py`).
**What this covers**: config coercion, request budgets, connection/send
failures, HTTP 408, overload 429/503/529, generic 5xx, deterministic 500/502
request-validation rejection, context-overflow exclusion, Z.AI overload
ceiling/backoff, credential-pool ordering, fallback cursor/stickiness, stable
request bytes, cancellation/replay safety before body consumption.

**Verification run this session**
- `cargo test -p hermes-gateway` targeting the 11 new/adjacent retry tests:
  all pass (`main_retry_attempts_match_python_config_coercion`,
  `main_retry_thresholds_distinguish_eager_and_full_budget_failures`,
  `main_status_retry_classes_match_source_executed_python_corpus`,
  `overloaded_main_route_retries_once_then_uses_fallback`,
  `dropped_main_connections_retry_once_then_use_fallback`,
  `server_errors_use_full_retry_budget_before_fallback`,
  `transport_without_fallback_uses_configured_attempt_budget`,
  `request_timeout_status_uses_transport_fallback_threshold`,
  `zai_coding_overload_without_fallback_uses_extended_budget`,
  `native_agent_applies_configured_main_retry_budget`).
- `.venv/bin/python3 rust/tools/gen_main_provider_retry_goldens.py --check`:
  `OK: verified byte-for-byte parity across 11 sections and 157 test cases`.

The classification and attempt-budget logic is close to the Python contract and
the golden corpus. One finding is a hard crash; the rest are narrow. The
attempt-count math for transport (2), overload (2), server error (full budget),
Z.AI overload (ceiling 8 when no fallback), and config coercion all match the
oracle.

---

## Findings

### F1 (High) reachable `unreachable!()` panic on HTTP 408 with a main credential pool

**Where**: `native_agent.rs`, `send_main_request`. The terminal `matches!`
guard at lines 2241-2249 lists `FormatError | Unrelated | UpstreamRateLimit |
Overloaded | ServerError` but omits `Transport`. The `failure_reason` match at
lines 2292-2303 then routes `MainPoolFailure::Transport` into
`unreachable!()`.

**Why it is reachable**: `main_retry_failure` returns
`MainPoolFailure::Transport` for exactly one status code, HTTP 408
(`native_agent.rs:1262`, `if status == REQUEST_TIMEOUT`). Connection/send
errors also produce `Transport`, but those return early through the
`MainRequestError::Fallback` arm of the `.send().await` match (lines 2180-2199)
and never reach the status-classification block. A 408 *response*, however,
flows all the way down.

**Reproduction trace** (408 response, main pool configured, retry budget
exhausted):
1. Provider returns HTTP 408. `main_pool_failure(408, ...)` hits the `_ =>`
   arm and yields `Unrelated`; `main_retry_failure(408, ...)` yields
   `Some(Transport)`.
2. `retry_failure == Transport` (not `FormatError`), so the else branch runs:
   `request_failures += 1`, then `main_attempt_limit(Transport, max, has_fallback)`.
   The loop retries until `request_failures` reaches that limit, then sets
   `failure = Transport`.
3. `let Some(pool) = &self.main_pool` is `Some` (a pool is configured), so the
   early Terminal return is skipped.
4. The `matches!` guard at 2241 does not include `Transport`, so it does not
   return.
5. `recovery_attempts` is bumped, the `failure == RateLimit` block is skipped,
   and control reaches `let failure_reason = match failure { ... Transport =>
   unreachable!() }`, which panics.

**Effect**: any provider that both fronts a Hermes credential pool and emits a
408 (reverse proxies in front of self-hosted llama.cpp/Ollama/vLLM backends do
exactly this, per the Python comment at `error_classifier.py:1517`) crashes the
turn once the transport retry budget is spent, instead of failing over. The
existing test `request_timeout_status_uses_transport_fallback_threshold` does
not catch this because it builds the client with `NativeAgentClient::new`
(no `main_pool`), so it takes the `let Some(pool) = ... else` early-return at
line 2233 and never reaches the panic branch.

**Smallest safe fix**: add `| MainPoolFailure::Transport` to the terminal
`matches!` list at lines 2241-2247. That returns
`Terminal(Transport)`, and `Transport::activates_provider_fallback()` is already
`true`, so `dispatch_main_turn` fails over exactly like `Overloaded` and
`ServerError` do. No credential rotation is wanted here (Python 408 -> timeout
sets `should_rotate_credential=False`), so keeping it out of the rotation path
is correct.

**Test gap**: add a 408 case with a configured `main_pool` and assert the
turn fails over (or returns a terminal error) rather than panics.

---

### F2 (Low) HTTP 503/529 carrying an empty-response advisory is classed
`Overloaded` instead of `ServerError`

**Where**: `native_agent.rs`, `main_retry_failure`, lines 1287-1336.

**Contract**: `error_classifier.py:1498-1515` handles `status in {503, 529}` by
checking `_EMPTY_PROVIDER_RESPONSE_PATTERNS` first and returning
`FailoverReason.server_error` (retryable, no compress) for those bodies, only
falling through to `overloaded` otherwise. The 500/502 block
(`error_classifier.py:1484-1489`) does the same.

**Divergence**: the Rust `empty_response` guard is used only to suppress the
context-overflow exclusion (the `if !empty_response && <context markers>`).
When `empty_response` is true, the function falls through to
`main_response_is_overloaded(status, text)`, which returns `true` for 503/529,
so the result is `Overloaded`. For 500/502 this is harmless (they are not
overloaded, so they still land on `ServerError`, matching Python). For 503/529
it flips the ladder: `Overloaded` falls back at attempt 2, whereas Python's
`server_error` uses the full retry budget and falls back at attempt 3.

**Reproduction**: a 503 response whose body contains e.g. "provider returned an
empty response" reaches fallback one attempt sooner in Rust than in Python.

**Effect**: narrow. It only bites when a provider emits an empty-response
advisory *under a 503/529 status* (unusual), and the only observable difference
is one fewer same-provider retry before failover. Not a crash, not a data issue.

**Smallest safe fix**: in the `empty_response` case for 503/529, return
`Some(MainPoolFailure::ServerError)` before the `main_response_is_overloaded`
check, mirroring `error_classifier.py:1503-1508`. Guard it so 500/502 keep their
current (already-correct) path.

**Test gap**: the golden-driven test
`main_status_retry_classes_match_source_executed_python_corpus` only iterates the
`error_classification_taxonomy_matrix`, which has no 503/529 empty-response row,
so this edge is unverified in either direction.

---

## Areas checked and confirmed correct

- **Config coercion** (`main_retry_attempts`, `native_agent.rs:200`; wired in
  `main.rs:1338`). Null/invalid-string/array/object -> 3, `int`-truncation of
  floats, `max(_, 1)` floor, bool -> 1/1. Matches Python `int(val)` + `max(val,
  1)` and the `retry_budgets_and_config_defaults` goldens. Covered by
  `main_retry_attempts_match_python_config_coercion`.
- **Request budgets** (`main_attempt_limit`, `native_agent.rs:943`). Transport
  and Overloaded cap at `min(max, 2)` when a fallback exists (contract Tier 2,
  fallback at `retry_count >= 2`); everything else uses the full budget
  (contract Tier 3). Verified against the goldens' `transport_*_attempt_2` and
  `server_error_attempt_2` rows and by
  `main_retry_thresholds_distinguish_eager_and_full_budget_failures`.
- **Connection/send failures**. Bounded by `request_failures < attempt_limit`,
  then `MainRequestError::Fallback { Transport }`; `dispatch_main_turn`
  fails over. No panic here because it never touches the status match.
  `dropped_main_connections_retry_once_then_use_fallback` (2 attempts) and
  `transport_without_fallback_uses_configured_attempt_budget` (budget 4) pass.
- **HTTP 408** classification maps to `Transport` (Python `timeout`), correct
  as far as classification and the no-pool path. See F1 for the pool path.
- **Overload 429/503/529**. `main_response_is_overloaded` covers 503/529 plus
  the 429 overload-body markers; `main_pool_failure` deliberately yields
  `Unrelated` for overloaded 429 and `main_retry_failure` overrides to
  `Overloaded`, so credential rotation is bypassed (Python
  `should_rotate_credential=False`). `overloaded_main_route_retries_once_then_uses_fallback`
  confirms 2 primary attempts then fallback, with identical request bytes.
- **Generic 5xx** -> `ServerError`, full budget then fallback
  (`server_errors_use_full_retry_budget_before_fallback`, 3 attempts). 504 and
  other 5xx are not in the context-overflow exclusion set, matching Python which
  only reclassifies context overflow for `{500,502}` and `{503,529}`.
- **Deterministic 500/502 request-validation** -> `FormatError`, eager (no
  retry), and eager provider fallback via `activates_provider_fallback`. The
  server-injected `prompt_cache_retention` guard and its sender allowlist
  (`meta, muse, msl, model-api, bedrock, mantle`) match
  `error_classifier.py:526` and `_is_server_injected_param_rejection`. The
  corpus test asserts the `format_error` row.
- **Context-overflow exclusion**. `main_retry_failure` returns `None` for
  500/502/503/529 bodies carrying context markers (and not empty-response
  markers), so those are not retried as server errors. Matches
  `error_classifier.py` and the `context_overflow` corpus row (asserted as
  `None` in the test). The downstream compression path is a deferred boundary
  (see below); the exclusion itself is correct.
- **Z.AI overload ceiling/backoff**. The `max_attempts.max(ceiling)` bump
  (ceiling 8) only changes behavior when no fallback exists, because
  `main_attempt_limit(Overloaded, .., true)` caps at 2. This is correct against
  Python `conversation_loop.py:6034-6039`: `_is_zai_coding_overload` raises
  `max_retries` to 8, but `_should_fallback = (_is_transport_failure and
  retry_count >= 2)` still fails over at attempt 2 when a fallback chain is
  configured; the ceiling only matters when the chain is empty/exhausted.
  `zai_coding_overload_without_fallback_uses_extended_budget` confirms 8 calls
  with no fallback. Adaptive long-tier backoff routes through
  `adaptive_rate_limit_backoff`.
- **Credential-pool ordering**. `RateLimit` stays out of `main_retry_failure`,
  so it still runs the persist/rotate path (retried_429 set, `rotate_after_failure`)
  and only returns `Terminal(RateLimit)` after pool exhaustion, preserving
  pool-before-fallback. The `recovery_attempts` counter was correctly narrowed
  to the credential-recovery path so transport/overload retries no longer share
  its 2-entry ceiling.
- **Fallback cursor/stickiness**. `dispatch_main_turn` seeds `index` from
  `state.active`, writes it back on success, and advances only on
  `activate_main_fallback`. `has_fallback` is recomputed per index via
  `next_main_fallback_index`, so a route already on the last fallback correctly
  uses the full budget rather than the min-2 eager cap.
- **Stable request bytes**. `build_body` is invoked once per provider index in
  `dispatch_main_turn`; the inner `send_main_request` retry loop reuses the same
  `&Value`. `overloaded_main_route_retries_once_then_uses_fallback` asserts
  `primary_bodies[0] == primary_bodies[1]` and that the fallback body's
  `messages`/`tools` match the primary.
- **Cancellation/replay safety before body consumption**. All retries and
  failovers happen before a successful `reqwest::Response` is handed back; on
  every non-success path the body is drained with `response.text().await`
  before the next attempt, so no partially streamed response is ever replayed.
  `wait_before_main_retry`'s `tokio::time::sleep` is cancel-safe.

---

## Intentionally deferred boundaries (not defects in this checkpoint)

These are out of scope per the task and are not regressed by the new code:

- Malformed successful responses (empty/`None` `choices` on a 200), safety
  refusals, and the `content_filter` incomplete-as-valid special case. The
  overload test's fallback returns `{"choices":[]}` and is accepted at dispatch;
  validation of the 200 body is a post-body concern.
- Response-body stalls, post-delta reconnect, mid-stream tool-call
  reconnect, and partial-length continuation.
- Primary-client rebuild / `try_recover_primary_transport` at attempt 3 when no
  fallback is configured (Python `conversation_loop.py:7022`). Rust currently
  goes straight to terminal for the no-fallback transport/overload/server case.
- Context-overflow compression. The exclusion is present (F-area above), but the
  compress-and-retry recovery that Python runs afterward is not wired on the
  main path.
- Server-injected `prompt_cache_retention` stripping-and-retry. The guard
  correctly declines to call it `FormatError`, but the param is not stripped
  before the `ServerError` retry, so those retries repeat the injected param.
- User-facing fallback notice banners and the operator status strings in
  section 10 of the contract are not emitted on this path yet.
