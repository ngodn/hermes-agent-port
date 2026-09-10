# Review: Rust ordinary main-provider successful-body recovery

Scope: the uncommitted working-tree diff on `rust-rewrite` that adds success-body
validation and recovery to `crates/hermes-gateway/src/native_agent.rs` (plus the
one-line wiring in `main.rs`). This is the lane that runs after `send_main_request`
returns an HTTP 200, that is malformed 200 handling, empty-response guard, and
content-policy refusal, for the streaming (no-tools) path and the tool-round
(`ChatModel::step`) path.

Method: read the full diff against the working tree, cross-checked against
`rust/analysis/main-provider-success-body-contract-agy.md`, the golden JSON
(`rust/tools/main-provider-success-body-goldens.json`), and live Python
(`agent/conversation_loop.py`, `agent/empty_response_guard.py`). I re-ran the 9 new
tests: all pass.

Bottom line: the implementation is careful and faithful on the parts it covers. I
did not find a reachable panic, an infinite loop, a double-billing bug, prompt
mutation, a premature `MessageStop`, or a fallback-leakage regression. The billing
and cooldown behavior that the seam analysis worried about is actually correct.
What is left are behavioral divergences from Python and a set of contract items that
are cleanly deferred. The single item with a real cost consequence is the
unimplemented cost-aware retry budget (F1).

---

## Verified correct

These were checked against Python and the goldens, and hold.

- **Refusal detection surface and precedence.** `main_success_body_failure`
  (`native_agent.rs:949`) treats `finish_reason == "content_filter"` as a refusal
  regardless of whether visible content is present, and treats a populated
  `message.refusal` as a refusal only when content and tool_calls are both absent.
  This matches Python exactly: `conversation_loop.py:4060` fires the content-policy
  branch purely on `finish_reason == "content_filter"` and fails over (eager,
  non-retryable), while the transport promotes `message.refusal` to
  `finish_reason="content_filter"` only when it is the sole payload (goldens
  `normalize_refusal_sole_payload_promotion`,
  `normalize_refusal_with_visible_content_unpromoted`,
  `normalize_refusal_with_tool_calls_unpromoted`). The seam analysis red test #5
  ("content_filter with content must not be discarded") is superseded by the oracle:
  Python does discard and fail over, so the Rust behavior is right.
- **Usage accounting is faithful, no double count.** Content-filter and malformed
  200 responses are never billed on the primary that gets failed over. That is
  correct because Python returns from the content-filter branch
  (`conversation_loop.py:4060`) and the invalid-response branch (around 3806)
  before it reaches usage tracking (`conversation_loop.py:4617`). Empty responses
  are billed once per attempt in Rust (`native_agent.rs:4198` streaming,
  `native_agent.rs:4950` tool), which matches Python billing at 4617 before the
  empty ladder at 8567. On a success-body fail-over the failed attempt's usage is
  not re-captured, so there is no double count. Test
  `policy_refusal_uses_frozen_fallback_without_retrying_primary` pins only the
  fallback usage being booked.
- **No primary cooldown, no pool rotation on a bad 200.**
  `activate_main_success_body_fallback` (`native_agent.rs:2250`) only advances
  `MainFallbackState.active`; it never arms `cooldown_until` and never touches the
  credential pool or `MainPoolFailure`. This is the separation the seam analysis
  required (Q4/Q7) and avoids the cache-oscillation trap. Tests assert
  `cooldown_until is None` and `rate_limit_backoff_count == 0`.
- **Deterministic empty needs two same-signature attempts.**
  `main_empty_is_deterministic` (`native_agent.rs:1587`) requires `>= 2` attempts
  with equal `(route_index, finish_reason)` and either all-usage-zero-output or
  all-usage-absent-and-no-generation, and fails open on mixed evidence. Matches
  contract 4.1. Streaming and tool tests both observe exactly two primary calls
  before failover.
- **Disabled guard.** With `enabled=false`, determinism is forced off and the
  budget stays 3 (`disabled_empty_guard_uses_full_three_retry_budget`, four calls).
- **Config coercion** matches the goldens for null/non-object sections, string
  booleans, and threshold parsing (`empty_response_guard_settings_match_python_goldens`).
- **Exactly one `MessageStop` per terminal path.** `forward_sse` no longer emits
  the stop itself (`emit_stop=false` from `run_model_turn`, `native_agent.rs:4142`);
  every terminal branch in the streaming loop emits one stop and the retry/fallover
  `continue` branches emit none. No premature or double stop.
- **Empty `choices` on the streaming path no longer silently succeeds.** A
  non-SSE or `[DONE]`-only body now yields `visible=false`, records an empty
  attempt, and after two identical attempts fails over. This closes the silent
  wrong-answer the seam analysis flagged as highest value.
- **Prompt bytes and no synthetic leakage.** The per-route body closure is
  unchanged and re-invoked per route on fail-over; `empty_attempts` and counters
  reset on activation, and no synthetic rows are carried across the switch.
- **Config wiring.** `with_empty_response_guard` (`main.rs:1341`) runs before
  `with_main_fallback_routes` (`main.rs:1476`), but the fallback builder copies
  `self.empty_response` into each route, so the guard reaches fallback routes.
  Missing config key resolves to `Value::Null` and to the enabled default.

---

## Findings, ranked

### F1 (Medium): cost-aware reduced retry budget is not implemented, billing divergence

`MainEmptyResponsePolicy._cost_threshold_usd` (`native_agent.rs:1647`) is parsed
and stored but deliberately unused (underscore, only asserted by the config test at
`native_agent.rs:5092`). The retry budget is hardcoded to 3 in both empty ladders
(`native_agent.rs:4201` streaming, `native_agent.rs:4953` tool). Python's
`empty_retry_budget` (`empty_response_guard.py:262-272`) drops the budget to
`REDUCED_EMPTY_RETRY_BUDGET = 1` when the estimated per-attempt input cost is at or
above the threshold (default 0.25 USD).

Consequence: on a large-context provider that returns unsignaled empties, Rust
re-sends the full context three times before failing over where Python re-sends it
once. That is up to two extra paid attempts per empty streak, on exactly the
expensive turns the guard exists to protect. The whole stated purpose of the guard
(contract 4.1: "protects against billing loops where large context windows are
repeatedly re-sent") is only half realized.

This is the boundary case between a bug and a deferred feature. It is clearly
deferred (no cost-estimation plumbing exists yet, threshold parsed only), so it is
also listed first under "deferred contract items." I rank it as a finding because it
has a concrete money cost and the config it reads implies the behavior is present.

Suggested fix: gate the budget through a cost estimate. Reuse the pricing that
`provider_usage` already carries for the last prompt tokens, compute an estimated
input cost per attempt, and when it is at or above `_cost_threshold_usd` set the
effective budget to 1 for that streak. Until then, document in the code that the
threshold is inert so it is not mistaken for active protection.

### F2 (Medium): thinking-only prefill and terminal reasoning delivery are absent

A response with structured reasoning but no visible content is contract 6.2 and 6.3
territory: Python runs up to two thinking-prefill continuation passes first, and on
exhaustion delivers a labeled reasoning excerpt while persisting `content="(empty)"`
with `_empty_terminal_sentinel`.

Rust has none of this. On the tool path, `empty` is computed only from the visible
answer (`native_agent.rs:4920`), so a reasoning-only turn drops straight into the
empty retry ladder. Because a reasoning-only response with usage has
`reasoning_tokens > 0`, `zero_output` is false and `observed_generation` is true, so
`main_empty_is_deterministic` never trips; the turn burns the full budget of
same-route retries and then delivers the bare string `(empty)` instead of the
model's reasoning. The streaming path behaves the same, via
`observe_main_sse_line` setting `observed_generation` on reasoning deltas.

Consequence: reasoning-model turns get more paid retries than Python and lose the
reasoning excerpt Python would surface. No crash, bounded loop.

Suggested fix: track prefill retries and the last reasoning text on the empty path,
run up to two prefill continuations before entering the ladder, and on exhaustion
deliver the reasoning preview while persisting the sentinel form. This depends on a
reasoning-extraction and prefill-message capability that may not exist yet, so it
may stay deferred; if so, list it explicitly.

### F3 (Low to Medium): streaming empty and refusal terminals skip the sentinel discipline

On streaming exhaustion the loop emits `(empty)` as a real `MessageChunk`
(`native_agent.rs:4222`), and on a no-fallback refusal it emits the raw
`outcome.refusal` text (`native_agent.rs:4176`). Both become the visible response
string that `run_native_turn` collects (`native_agent.rs:4323-4330`) and can feed
into `pending_memory_turn` (`native_agent.rs:4335`). Python instead persists
`content="(empty)"` tagged `_empty_terminal_sentinel=True` so the empty turn cannot
poison later context, and wraps a refusal in the `_CONTENT_POLICY_RECOVERY_HINT`
message rather than echoing the provider's raw refusal.

Consequence: a streaming empty or refusal can be treated downstream as a genuine
assistant answer, and the refusal wording diverges from Python. Lower severity than
F1/F2 because it only affects the terminal, no-recovery case, but it is a real
transcript-fidelity gap.

Suggested fix: mark the streaming empty terminal with the sentinel form when the
transcript is persisted, and route the refusal through the same formatted-message
wrapper Python uses.

### F4 (Low): post-tool empty nudge fires at most once per whole turn, not per tool round

In `step` the empty path defers to the tool loop only when
`recent_tool && !already_nudged` (`native_agent.rs:4939`), and `already_nudged`
scans the entire message list for any `_empty_recovery_synthetic` flag
(`native_agent.rs:4934`). Combined with the pre-existing tool-loop guard
`post_tool_empty_retried`, which is set once and never reset
(`native_tools.rs:611,626`), the substantive-tool nudge can happen only once for the
entire turn. Python resets `_post_tool_empty_retried` whenever tool calls land
(contract 7.3), so it re-enables the nudge after each tool round.

Consequence: in a multi-round tool conversation, a later post-tool empty skips the
one-shot nudge and goes straight to the retry ladder. The root cause is the
pre-existing tool-loop flag, but the new `already_nudged` gate makes `step` inherit
the same once-per-turn ceiling. Bounded, no loop.

Suggested fix: scope the nudge state to the current tool round rather than the whole
turn, and reset it when a tool round succeeds, mirroring the Python counter reset.

### F5 (Low): observed_generation is broader than Python's inline-thinking test

`main_empty_attempt` (`native_agent.rs:1604`) sets `observed_generation` true when
content has no visible answer but is non-empty, which catches bare scaffolding
markers like `[memory]`. Python's `_has_structured` uses `_has_inline_thinking`,
which is specifically `<think>` block detection (`conversation_loop.py:8542`).

Consequence: in the narrow usage-absent determinism branch, a repeated
scaffolding-only empty could be classified as "generation observed" and so not be
recognized as deterministic, costing extra retries. Very narrow, no correctness
break.

Suggested fix: restrict the content arm of `observed_generation` to inline
`<think>` detection to match `_has_inline_thinking`.

### F6 (Low): streaming content_filter after visible output errors the turn

When the stream delivers visible text and then a content_filter finish reason,
`run_model_turn` returns `Err` (`native_agent.rs:4151`) and does not book usage.
This is a deliberate no-replay choice and is safer than double-streaming, but it
diverges from Python's content-filter branch, which would attempt a fallback or
return a formatted refusal. The seam analysis already flagged mid-stream refusal as
deferred, so this is expected; noting it so the divergence is on record. Test
`policy_refusal_after_visible_stream_output_is_not_replayed` pins the current Err
behavior.

---

## Deferred contract items still missing (not bugs)

Listed separately because they are intentionally out of the current lane, but they
are part of the full success-body contract and should be tracked.

1. **Cost-aware guard mechanics** (contract 4.1): the reduced budget (F1), the
   per-streak cost accumulation `streak_cost_usd`, the "estimated cost of these
   empty attempts" status line, and the streak reset points (turn start, tool
   success, compaction, fallback).
2. **Thinking-only handling** (contract 6.2 to 6.4): prefill continuation passes,
   terminal reasoning-excerpt delivery, `_empty_terminal_sentinel` persistence,
   thinking-budget-exhaustion detection under `finish_reason == "length"`, and the
   ephemeral reasoning-off continuation request.
3. **Stream truncation and the no-replay continuation machinery** (contract 8): the
   streaming path has no `finish_reason == "length"` handling at all, no partial
   stream stub, no dropped-tool continuation prompt, and no tool-call truncation
   token-boost. Same-provider length continuation (up to 4 passes) is entirely
   absent.
4. **Post-tool empty refinements** (contract 7.3): housekeeping-content reuse
   (`fallback_prior_turn_content`) when the prior turn had content alongside only
   housekeeping tools, and the per-round nudge reset (F4).
5. **Status and notice emission** (contract 9): the fallback notice, empty-retry
   status lines, and the cost summary line are not surfaced.
6. **Provider-specific distinctions** (contract 10): DeepSeek/Kimi reasoning
   echo-back padding on tool-call messages, Poolside integer finish reasons,
   Ollama/GLM `<think>` strip and premature-stop-to-length rewrite, Gemini
   `extra_content` preservation, and xAI tool-search alias reversal. Most of these
   are outside the chat-completions success-body seam, but they are part of the
   contract's scope item 8.

## Test quality

The 9 new tests are real-HTTP axum tests in the established pattern and cover the
load-bearing behaviors: deterministic empty failover on both transports (two
primary calls), pre-visible refusal failover, post-visible refusal no-replay,
disabled-guard full budget, malformed tool response failover and same-route retry,
and the cooldown/usage assertions that pin the "not conflated with pool health"
rule. Gaps worth adding when the deferred items land: a fail-over-does-not-double-
count-usage assertion where the malformed primary also returns a `usage` block (the
current malformed test omits usage on the primary body, so it does not exercise the
"drop the failed attempt's usage" path), a content_filter-with-visible-content case
to lock in that Rust intentionally fails over (matching Python), and a reduced-budget
test once F1 is implemented.
