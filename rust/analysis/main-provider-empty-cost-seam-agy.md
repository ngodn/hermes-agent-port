# Main Provider Empty-Response Cost-Aware Retry Budget Seam Analysis

**Document Target**: `rust/analysis/main-provider-empty-cost-seam-agy.md`
**Evidence Lane**: Live Python Empty-Response Retry Budget & Rust Native Chat-Completions Path
**Primary Sources**:
- [`agent/empty_response_guard.py`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py)
- [`agent/usage_pricing.py`](file:///home/eins0fx/development/hermes-agent-port/agent/usage_pricing.py)
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/agent_init.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_init.py)
- [`tests/agent/test_empty_response_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_empty_response_guard.py)
- [`rust/crates/hermes-gateway/src/provider_usage.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_usage.rs)
- [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs)
- [`rust/crates/hermes-gateway/src/models_dev.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/models_dev.rs)
- [`rust/crates/hermes-gateway/src/provider_registry.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs)
- [`rust/tools/gen_main_provider_success_body_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_success_body_goldens.py)
- [`rust/tools/main-provider-success-body-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-success-body-goldens.json)

---

## 1. Executive Summary and Verdict

### Verdict: DEFER per-attempt cost threshold reduction; MAINTAIN explicit fail-open boundary (fixed 3-retry budget)

This research lane inspected Python's empty-response cost-aware retry budget (NS-503) and the current Rust gateway implementation to determine whether Rust already possesses enough frozen provider/model pricing data to safely enforce Python's per-attempt cost threshold in the native chat-completions path.

The verdict is an explicit **DEFER**:
1. **Zero Frozen Pricing Data in Rust**: The Rust codebase currently contains token normalization logic ([`provider_usage.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_usage.rs)) and model capability metadata ([`models_dev.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/models_dev.rs)), but possesses **zero** token pricing tables, zero rate schedules, zero billing route normalizers, and zero endpoint pricing metadata clients.
2. **Python Contract Explicitly Demands Fail-Open**: In Python ([`agent/empty_response_guard.py:28-30`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py#L28-L30)), the cost-aware retry budget is designed to fail open: unknown pricing, missing usage, or subscription routes leave the retry budget untouched at `DEFAULT_EMPTY_RETRY_BUDGET = 3`.
3. **Current Rust Tree Perfectly Matches the Fail-Open Boundary**: In [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1641-L1658), `MainEmptyResponsePolicy` already parses and persists the configuration threshold (`_cost_threshold_usd`), enforces the deterministic empty response detection rule (`main_empty_is_deterministic`), and defaults to a 3-retry budget (`retry_budget = 3`). Under absent pricing data, keeping the budget at 3 is the exact specified fail-open behavior.
4. **Implementing Incomplete Pricing Hacks Would Fail Closed**: Attempting to hardcode partial pricing or crude token count thresholds without cache read/write discounts or context-tier rates would falsely penalize cached requests and violate the behavioral contract.

---

## 2. Python Cost-Aware Retry Budget Architecture and Exact Trace

### 2.1 The Problem Statement (NS-503)
When upstream providers encounter silent failures or unhandled internal exceptions, they may return HTTP 200 with an empty completion (zero completion tokens, generic `finish_reason="stop"`). When an agent conversation has accumulated 50k to 200k tokens of context, retrying 3 times on a paid route re-sends that entire context 3 times, repeatedly billing the user for zero output (the "$2.33 incident class").

Python addresses this via two independent guards in [`agent/empty_response_guard.py`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py):
1. **Deterministic-Empty Guard**: Two consecutive empty completions with matching `(model, provider, finish_reason)` and verified zero output short-circuit retries and jump immediately to fallback.
2. **Cost-Aware Retry Budget**: When the estimated input cost of a single empty attempt is greater than or equal to `cost_threshold_usd` (default $0.25), the retry budget for that streak drops from 3 to 1.

### 2.2 Configuration Coercion Trace
The configuration lives under `agent.empty_response_guard` in `config.yaml`:
```yaml
agent:
  empty_response_guard:
    enabled: true            # false = legacy fixed 3-retry behavior
    cost_threshold_usd: 0.25 # per-attempt cost threshold that drops budget to 1
```

- **Resolution Site**: [`agent/agent_init.py:2064-2068`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_init.py#L2064-L2068) calls [`resolve_guard_settings(section)`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py#L86-L119).
- **Boolean Coercion**:
  - If `enabled` is a boolean: accepted directly.
  - If `enabled` is a string (due to YAML parsing variations): case-insensitive string values `"0"`, `"false"`, `"no"`, and `"off"` coerce to `False`. All other non-empty strings coerce to `True`.
  - Non-dict section or absent field: defaults to `DEFAULT_GUARD_ENABLED = True`.
- **Threshold Coercion**:
  - `cost_threshold_usd` accepts integers, floats, or numeric strings.
  - Parsed as `Decimal(str(threshold_raw))`.
  - Must satisfy `candidate > 0`. If candidate is negative, zero, non-numeric, a boolean, or raises an exception: silently falls back to `DEFAULT_COST_THRESHOLD_USD = Decimal("0.25")`.

### 2.3 Attempt Recording and Cost Estimation Pipeline
Inside [`agent/empty_response_guard.py`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py):

1. **Streak Reset**:
   [`record_empty_attempt`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py#L203-L238) checks `agent._empty_content_retries == 0`. When zero, it clears previous attempts and resets `_empty_streak_cost_usd` to `Decimal("0")`.
2. **Usage Extraction and Zero-Output Check**:
   [`_zero_output(agent, response)`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py#L172-L201) normalizes usage via [`agent.usage_pricing.normalize_usage`](file:///home/eins0fx/development/hermes-agent-port/agent/usage_pricing.py#L1297-L1450).
   - If `prompt_tokens <= 0` or usage is absent: returns `(False, False)` (fails open).
   - If `(output_tokens + reasoning_tokens) == 0`: returns `(True, True)`.
   - If `reasoning_tokens > 0` (e.g. thinking-only responses): returns `(True, False)`. Reasoning counts as real generation; it is never classified as zero output.
3. **Per-Attempt Cost Estimation**:
   [`_estimate_attempt_cost(agent, response)`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py#L146-L170):
   ```python
   raw_usage = getattr(response, "usage", None)
   if not raw_usage:
       return None
   canonical = normalize_usage(
       raw_usage,
       provider=getattr(agent, "provider", None),
       api_mode=getattr(agent, "api_mode", None),
   )
   result = estimate_usage_cost(
       getattr(agent, "model", "") or "",
       canonical,
       provider=getattr(agent, "provider", None),
       base_url=getattr(agent, "base_url", None),
       api_key=getattr(agent, "api_key", None),
   )
   return getattr(result, "amount_usd", None)
   ```
4. **Streak Cost Accumulation**:
   If `cost` is returned and `cost > 0`, adds `cost` to `agent._empty_streak_cost_usd`.

### 2.4 Dynamic Budget Calculation
In [`agent/empty_response_guard.py:262-273`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py#L262-L273):
```python
def empty_retry_budget(agent: Any, response: Any) -> int:
    if not guard_enabled(agent):
        return DEFAULT_EMPTY_RETRY_BUDGET
    cost = _estimate_attempt_cost(agent, response)
    if cost is None:
        return DEFAULT_EMPTY_RETRY_BUDGET
    if cost >= _cost_threshold_usd(agent):
        return REDUCED_EMPTY_RETRY_BUDGET
    return DEFAULT_EMPTY_RETRY_BUDGET
```
- When guard is disabled: returns `3`.
- When cost estimation returns `None` (unknown pricing, missing usage, proxy error): returns `3` (fails open).
- When cost is strictly below threshold: returns `3`.
- When cost is greater than or equal to threshold: drops budget to `1`.

### 2.5 Conversation Loop Integration Trace
In [`agent/conversation_loop.py:8590-8664`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8590-L8664):
1. Upon detecting an empty response candidate:
   - Invokes `_empty_guard.record_empty_attempt(agent, finish_reason=finish_reason, response=response, observed_generation=_has_structured)`.
   - Computes `_empty_retry_budget = _empty_guard.empty_retry_budget(agent, response)`.
   - Computes `_deterministic_empty = _empty_candidate and _empty_guard.deterministic_empty(agent)`.
2. Retry Decision:
   - If `agent._empty_content_retries < _empty_retry_budget and not _deterministic_empty`:
     - Increments `agent._empty_content_retries += 1`.
     - Calculates jittered backoff delay: `jittered_backoff(agent._empty_content_retries, base_delay=5.0, max_delay=60.0)`.
     - Appends status notice: if `_empty_retry_budget < 3`, appends `" - high-cost request, reduced retry budget"`.
     - Sleeps while listening for user interrupts and updates activity touch counters.
     - Loops to retry the same provider (`continue`).
3. Deterministic or Budget Exhaustion Decision:
   - If `_deterministic_empty` or retries hit `_empty_retry_budget`:
     - Tries activating fallback: `agent._try_activate_fallback()`.
     - If fallback activates: resets `agent._empty_content_retries = 0` and restarts iteration on the fallback route.
     - If fallback exhausted or absent: buffers accumulated streak cost (`streak_cost_usd`), appends terminal assistant message with `_empty_terminal_sentinel = True`, sets `exit_reason = "empty_response_exhausted"`, and terminates the turn.

---

## 3. Current Rust Tree: Available Modules and Data

| Rust Module / File | Present Capabilities | Missing Pricing / Cost Elements |
| :--- | :--- | :--- |
| [`rust/crates/hermes-gateway/src/provider_usage.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_usage.rs) | Token normalization (`CanonicalUsage`), JSON/SSE usage extraction (`from_response`, `from_sse_line`), prompt cache token separation (`cache_read_tokens`, `cache_write_tokens`), saturating token arithmetic. | **Zero pricing data**. No per-token rates, no rate tables, no cost calculation functions. |
| [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1641-L1685) | `MainEmptyResponsePolicy`: config parser for `enabled` and `cost_threshold_usd` matching Python goldens. `MainEmptyAttempt` and `main_empty_is_deterministic` implementing the 2-attempt streak check. | `_cost_threshold_usd: f64` field has leading underscore and is completely unused. `retry_budget: usize` is hardcoded to 3. No streak cost accumulator. |
| [`rust/crates/hermes-gateway/src/models_dev.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/models_dev.rs) | Model capability resolution (`supports_reasoning`, `supports_tools`, `supports_vision`, `context_window`, `max_output_tokens`, `model_family`) via `models.dev/api.json` cache. | **Zero pricing data**. Does not parse or expose input/output token pricing. |
| [`rust/crates/hermes-gateway/src/provider_registry.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs) | Provider profile metadata (`base_url`, `default_headers`, `fixed_temperature`, `default_max_tokens`, `auth_type`). | **Zero pricing data**. Does not contain billing rates. |
| [`rust/crates/hermes-gateway/src/managed_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs) | Local runtime catalog for GGUF/Ollama models and hardware estimation. | **Zero cloud pricing data**. |

### 3.1 The Current Empty-Response Recovery in Rust
In [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs):
- **Streaming Loop** (lines 4183-4210):
  ```rust
  empty_attempts.push(MainEmptyAttempt {
      route_index: dispatched.route_index,
      finish_reason: outcome.finish_reason,
      usage_present,
      zero_output,
      observed_generation: outcome.observed_generation,
  });
  self.capture_usage(outcome.usage);
  let deterministic =
      main_empty_is_deterministic(&empty_attempts, self.empty_response.enabled);
  if !deterministic && empty_retries < self.empty_response.retry_budget {
      empty_retries = empty_retries.saturating_add(1);
      let route = self
          .main_route(dispatched.route_index)
          .unwrap_or_else(|| self.clone());
      route
          .wait_before_empty_response_retry(empty_retries as i64)
          .await;
      continue;
  }
  ```
- **Tool-Calling Step Loop** (lines 4944-4962):
  ```rust
  empty_attempts.push(main_empty_attempt(
      dispatched.route_index,
      choice,
      &message,
      usage.as_ref(),
  ));
  self.capture_usage(usage);
  let deterministic =
      main_empty_is_deterministic(&empty_attempts, self.empty_response.enabled);
  if !deterministic && empty_retries < self.empty_response.retry_budget {
      empty_retries = empty_retries.saturating_add(1);
      let route = self
          .main_route(dispatched.route_index)
          .unwrap_or_else(|| self.clone());
      route
          .wait_before_empty_response_retry(empty_retries as i64)
          .await;
      continue;
  }
  ```

In both places, `empty_retries < self.empty_response.retry_budget` checks against `retry_budget: 3`.

---

## 4. Proof of the Dependency Gap

Why is it impossible to implement Python's per-attempt cost threshold safely in Rust today without porting the broader pricing engine?

### 4.1 Dependency 1: Missing Static Documentation Pricing Table
In Python, [`agent/usage_pricing.py:158-1035`](file:///home/eins0fx/development/hermes-agent-port/agent/usage_pricing.py#L158-L1035) defines `_OFFICIAL_DOCS_PRICING`, an 877-line frozen dictionary containing exact rates for:
- OpenAI GPT-5.6 series (`gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `-pro`, `-900k`)
- Anthropic Claude 4.8 and Claude Sonnet 5
- Google Gemini (Flash, Pro, Ultra context-tiered rates)
- MiniMax, Bedrock inference profiles, and Fireworks
Each entry specifies:
- `input_cost_per_million`
- `output_cost_per_million`
- `cache_read_cost_per_million`
- `cache_write_cost_per_million`
- `request_cost`
- `tier_threshold_tokens` and `*_above` threshold rates

None of this table has been ported to Rust.

### 4.2 Dependency 2: Missing Dynamic Pricing Endpoints
For routes not in the static snapshot (notably OpenRouter and custom OpenAI-compatible proxies), Python relies on:
- [`fetch_model_metadata`](file:///home/eins0fx/development/hermes-agent-port/agent/model_metadata.py) querying `https://openrouter.ai/api/v1/models`.
- [`fetch_endpoint_model_metadata`](file:///home/eins0fx/development/hermes-agent-port/agent/model_metadata.py) querying `{base_url}/models`.

Rust has no client or parser for these pricing metadata payloads.

### 4.3 Dependency 3: Billing Route Normalization
Python resolves billing routes via [`resolve_billing_route(model, provider, base_url)`](file:///home/eins0fx/development/hermes-agent-port/agent/usage_pricing.py#L1078-L1126):
- Normalizes `openai-api` to `openai`
- Normalizes `vertex`, `google-gemini`, `google-ai-studio` to `google`
- Normalizes Bedrock cross-region inference profiles by stripping regional prefixes (`us.anthropic...` -> `anthropic...`)
- Normalizes dot-notation versions (`4.7` -> `4-7`)
- Detects `subscription_included` routes (e.g. `openai-codex`) which price at `$0.00`

Rust has no billing route normalization layer.

### 4.4 Dependency 4: Arithmetic and Precision Requirements
Python performs pricing calculations using `decimal.Decimal` with high precision (e.g. $0.0046 sub-cent precision) and banker's rounding.
In Rust, `hermes-gateway` does not include `rust_decimal` in its `Cargo.toml`. While `f64` could approximate simple cases, token multiplication at scale (e.g. multiplying 125,000 tokens by $0.25 / 1,000,000) requires precision guarantees to avoid rounding errors around the $0.25 threshold boundary.

### 4.5 Golden Test Generator Verification
The test generator [`rust/tools/gen_main_provider_success_body_goldens.py:498-535`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_success_body_goldens.py#L498-L535) provides definitive proof of this dependency gap:
When generating golden test cases for `empty_retry_budget` and `streak_cost_usd`, the Python test generator **explicitly mocked `_estimate_attempt_cost` using `patch.object`**:
```python
# 2.4 empty_retry_budget
with patch.object(empty_guard, "_estimate_attempt_cost", return_value=None):
    budget_unknown = empty_guard.empty_retry_budget(agent, make_response())
    assert budget_unknown == 3
with patch.object(empty_guard, "_estimate_attempt_cost", return_value=Decimal("0.10")):
    budget_low_cost = empty_guard.empty_retry_budget(agent, make_response())
    assert budget_low_cost == 3
with patch.object(empty_guard, "_estimate_attempt_cost", return_value=Decimal("0.50")):
    budget_high_cost = empty_guard.empty_retry_budget(agent, make_response())
    assert budget_high_cost == 1
```
The test suite itself isolates `empty_response_guard` from `usage_pricing` because the pricing module is a heavy, external subsystem.

---

## 5. Cache and Prompt Implications

Implementing per-attempt cost calculation without understanding prompt caching semantics would lead to severe behavioral distortions:

### 5.1 Prompt Caching Dramatically Shifts Cost
Modern foundation models charge radically different rates depending on cache status:
- **Anthropic Claude 3.7 / 4.8 / 5**:
  - Base input rate: $3.00 / 1M tokens
  - Cache write rate (creation): $3.75 / 1M tokens (1.25x)
  - Cache read rate (hit): $0.30 / 1M tokens (0.10x, 90% discount)
- **OpenAI GPT-5.6**:
  - Base input rate: $2.50 / 1M tokens
  - Cache read rate: $0.25 / 1M tokens (90% discount)
  - Cache write rate: $3.125 / 1M tokens

#### Concrete Cost Divergence Scenario
Consider an empty response on a 70,000-token prompt:
1. **Uncached Case**:
   `70,000 tokens * ($3.00 / 1,000,000) = $0.210`
   Below $0.25 threshold -> Budget remains 3.
2. **Cache Write Creation Case**:
   `70,000 tokens * ($3.75 / 1,000,000) = $0.2625`
   Exceeds $0.25 threshold -> Budget drops to 1.
3. **Cache Read Hit Case**:
   `70,000 tokens * ($0.30 / 1,000,000) = $0.021`
   Well below $0.25 threshold -> Budget remains 3.

If a naive Rust implementation attempted to estimate cost from `usage.prompt_tokens()` alone using a single average rate, a cached prompt of 100k tokens ($0.03 actual cost) would be falsely calculated at $0.30, prematurely cutting the retry budget from 3 to 1 and preventing transient recovery.
Therefore, cost-aware budget reduction strictly requires token breakdown into `input_tokens`, `cache_read_tokens`, and `cache_write_tokens`, which `CanonicalUsage` in `provider_usage.rs` provides, but which cannot be evaluated without provider-specific rate tables.

### 5.2 Prompt Invariance During Empty-Response Retries
- During same-provider empty retries (attempts 1 to 3), the conversation prompt is **invariant**. Hermes does not append synthetic retry nudges or modify messages between empty attempts.
- Synthetic prompt modifications occur only at distinct recovery boundaries:
  - Prefill continuation: appends `_thinking_prefill = True` assistant message.
  - Substantive tool follow-up: appends `_EMPTY_TOOL_RESPONSE_NUDGE`.
  - Fallback activation: rewrites `Model: ...` and `Provider: ...` tail lines in cached system prompt.
  - Final exhaustion: records `_empty_terminal_sentinel = True`.
- Consequently, an empty-response retry re-sends the identical prompt bytes to the upstream provider, resulting in identical cache hit rates on subsequent attempts.

---

## 6. The Smallest Correct Seam Architecture

When the pricing subsystem is ported to Rust, where and how should the cost threshold be wired?

### 6.1 The Seam Interface
The smallest correct seam in `NativeAgentClient` requires:

```rust
// In rust/crates/hermes-gateway/src/native_agent.rs

impl NativeAgentClient {
    /// Calculate the empty-retry budget for the current attempt.
    /// Fails open to DEFAULT_EMPTY_RETRY_BUDGET (3) if guard is disabled,
    /// pricing is unknown, or cost estimation fails.
    fn empty_retry_budget(
        &self,
        route_index: usize,
        usage: Option<&crate::provider_usage::CanonicalUsage>,
    ) -> usize {
        const DEFAULT_EMPTY_RETRY_BUDGET: usize = 3;
        const REDUCED_EMPTY_RETRY_BUDGET: usize = 1;

        if !self.empty_response.enabled {
            return DEFAULT_EMPTY_RETRY_BUDGET;
        }

        let Some(usage) = usage else {
            return DEFAULT_EMPTY_RETRY_BUDGET;
        };

        let route = self.main_route(route_index).unwrap_or(self);
        let Some(cost_usd) = route.estimate_attempt_cost(usage) else {
            return DEFAULT_EMPTY_RETRY_BUDGET;
        };

        if cost_usd >= self.empty_response._cost_threshold_usd {
            REDUCED_EMPTY_RETRY_BUDGET
        } else {
            DEFAULT_EMPTY_RETRY_BUDGET
        }
    }
}
```

### 6.2 Insertion Points in the Two Chat-Completions Loops
The calculation hooks in at the exact point where deterministic empty is checked:

1. **Streaming Loop** ([`native_agent.rs:4201`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L4201)):
   ```rust
   // Replace:
   // if !deterministic && empty_retries < self.empty_response.retry_budget {
   // With:
   let budget = self.empty_retry_budget(dispatched.route_index, outcome.usage.as_ref());
   if !deterministic && empty_retries < budget {
       empty_retries = empty_retries.saturating_add(1);
       ...
   }
   ```

2. **Tool-Calling Step Loop** ([`native_agent.rs:4953`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L4953)):
   ```rust
   // Replace:
   // if !deterministic && empty_retries < self.empty_response.retry_budget {
   // With:
   let budget = self.empty_retry_budget(dispatched.route_index, usage.as_ref());
   if !deterministic && empty_retries < budget {
       empty_retries = empty_retries.saturating_add(1);
       ...
   }
   ```

3. **Streak Cost Accumulation (Optional Polish)**:
   Add an atomic or mutable `streak_cost_usd: Option<f64>` accumulator, reset to `0.0` whenever `empty_retries == 0`, and log the estimated waste when retries exhaust.

---

## 7. Recommended Explicit Fail-Open Boundary for this Checkpoint

For the current checkpoint, the gateway must strictly maintain the **explicit fail-open boundary**:

1. **Retain `retry_budget = 3` as the Effective Budget**:
   Because pricing data is not yet available, every request behaves as an unknown-pricing route. According to Python's contract:
   `cost is None -> return DEFAULT_EMPTY_RETRY_BUDGET (3)`
   Rust's current hardcoded `retry_budget: 3` in `MainEmptyResponsePolicy` is exactly compliant with this rule.
2. **Retain Config Parsing for `cost_threshold_usd`**:
   `MainEmptyResponsePolicy` and `main_empty_response_policy(value: &Value)` must continue to parse `enabled` and `cost_threshold_usd` (validating that it is finite and positive, defaulting to 0.25). This ensures configuration parity is tested and ready without modifying config schemas later.
3. **Retain Deterministic-Empty Short-Circuit**:
   `main_empty_is_deterministic(&empty_attempts, self.empty_response.enabled)` operates purely on token usage presence (`prompt_tokens > 0`, `output_tokens + reasoning_tokens == 0`) and response signatures. It does not depend on pricing data and remains fully functional today.

---

## 8. Tests Needed When Pricing Subsystem Lands

When the pricing catalog is ported to Rust, the following test matrix must be implemented:

1. **Configuration and Schema Tests**:
   - Already passing in [`native_agent.rs:5018-5088`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L5018-L5088) (`empty_response_guard_settings_match_python_goldens`).
2. **Pricing Engine Unit Tests**:
   - `test_estimate_usage_cost_uncached`: tests `input_tokens * rate`.
   - `test_estimate_usage_cost_cached`: tests `cache_read_tokens * discount_rate`.
   - `test_estimate_usage_cost_cache_write`: tests `cache_write_tokens * premium_rate`.
   - `test_estimate_usage_cost_unknown_model`: returns `None`.
3. **Dynamic Budget Unit Tests** (mirroring [`rust/tools/main-provider-success-body-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-success-body-goldens.json) Section 2.4):
   - `empty_retry_budget_unknown_cost`: pricing returns `None` -> budget = 3.
   - `empty_retry_budget_low_cost_below_threshold`: estimated cost = $0.10 (< $0.25) -> budget = 3.
   - `empty_retry_budget_high_cost_reduced`: estimated cost = $0.50 (>= $0.25) -> budget = 1.
   - `empty_retry_budget_custom_high_threshold`: estimated cost = $0.50 (< $1.00 custom threshold) -> budget = 3.
   - `empty_retry_budget_disabled_guard`: guard enabled = false -> budget = 3.
4. **Integration Server Tests**:
   - `high_cost_empty_response_retries_only_once_before_fallback`: WireMock server emits empty HTTP 200 with usage exceeding $0.25; asserts primary is called exactly 2 times (initial attempt + 1 retry = 2 total requests) before switching to fallback.
   - `low_cost_empty_response_retries_three_times_before_fallback`: WireMock server emits empty HTTP 200 with usage under $0.25; asserts primary is called 4 times (initial attempt + 3 retries = 4 total requests) before switching to fallback.
