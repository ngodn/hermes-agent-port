# Compression Auxiliary Client Routing and Fallback Contract

## 1. Overview and Scope

This report maps the exact Python implementation contracts for auxiliary client selection, parameter resolution, multi-tier fallback, and usage accounting during full context compression summaries. It documents the current gaps in native Rust (`hermes-gateway`), identifies the narrowest Rust architectural seams, and outlines parity traps for two-endpoint live testing.

Compression boundary calculations, message pruning, compaction persistence, and micro-compaction mechanics are excluded except where strictly necessary to explain the summary LLM request.

---

## 2. Configuration Keys and Precedence

### Modern `auxiliary.compression` Keys
In [`cli-config.yaml.example:873-877`](file:///home/eins0fx/development/hermes-agent-port/cli-config.yaml.example#L873-L877) and [`agent/auxiliary_client.py:8887-8929`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8887-L8929), configuration for the compression auxiliary route is stored under the `auxiliary.compression` dictionary:

- `provider`: Provider identifier (`"auto"`, `"openrouter"`, `"nous"`, `"gemini"`, `"openai"`, `"custom"`, etc.).
- `model`: Target model string (`"google/gemini-2.5-flash"`, etc.). Empty string or `"auto"` is normalized to `None`.
- `base_url`: Optional HTTP/HTTPS endpoint URL.
- `api_key`: Optional explicit secret string.
- `key_env` / `api_key_env`: Environment variable name supplying the API key when `api_key` is not inlined.
- `api_mode` (or `transport`): Wire protocol format override (`"chat_completions"`, `"codex_responses"`, or `"anthropic_messages"`).
- `timeout`: Per-request timeout in seconds (default: 120s; floored to 300s at runtime for compression).
- `max_concurrency`: Positive integer capping concurrent in-flight compression calls via a synchronization semaphore.
- `reasoning_effort`: Thinking depth shorthand (`"none"`, `"false"`, `"low"`, `"medium"`, `"high"`, etc.).
- `max_output_tokens`: Integer cap used only when certified by the non-reasoning fast lane.
- `extra_body`: Dictionary of vendor-specific request payload extensions.
- `fallback_chain`: Ordered list of fallback candidate dictionaries (`provider`, `model`, optional `base_url`, `api_key`, `key_env`, `api_mode`, `timeout`).
- `context_length`: Explicit integer override for the auxiliary model context window, bypassing live or catalog lookups ([`agent/agent_init.py:2416`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_init.py#L2416)).

### Legacy Summary Settings and Migration
Older configurations placed summary model settings inside the `compression:` block ([`hermes_cli/config_migrations.py:250-290`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config_migrations.py#L250-L290)):
- `compression.summary_model`
- `compression.summary_provider`
- `compression.summary_base_url`

Migration version 17 (`_migrate_to_17`) removes these keys from `compression:` on disk and writes them to `auxiliary.compression.{model, provider, base_url}` if not already set. In [`hermes_cli/doctor.py:553-598`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/doctor.py#L553-L598), `collect_deprecated_config_keys()` reports them under `_DEPRECATED_COMPRESSION_SUMMARY_KEYS` as deprecated keys requiring migration.

### Resolution Precedence
The runtime determines the effective route in [`agent/auxiliary_client.py:8696-8871`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8696-L8871) (`_resolve_task_provider_model`):

1. Explicit call arguments: Call-site parameters (`provider`, `model`, `base_url`, `api_key`) or pinned retry overrides from [`_SUMMARY_ROUTE_PIN`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L82-L84).
2. Task configuration: Keys under `auxiliary.compression.*` loaded via `_get_auxiliary_task_config("compression")`.
3. Main agent runtime fallback: When provider is `"auto"`, empty, or unset, [`_resolve_auto_route()`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6465-L6658) adopts the main conversation model and provider from `main_runtime`.
4. Discovery chain: If the main runtime client cannot be established or is marked unhealthy, the built-in auto-discovery chain runs (OpenRouter -> Nous -> Custom -> Codex -> API-key catalog).

### Absence of Environment Variable Bridging
Unlike `vision` (`AUXILIARY_VISION_*`) and `approval` (`AUXILIARY_APPROVAL_*`), `compression` has no environment variable bridging in `cli.py` or `gateway/run.py` ([`tests/agent/test_auxiliary_config_bridge.py:33,155-163`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_auxiliary_config_bridge.py#L33)). Context compression configuration is read strictly from `config.yaml` and the live runtime snapshot.

---

## 3. Parameter and Contract Resolution

### Provider Resolution
- Handled in [`agent/auxiliary_client.py:8696-8871`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8696-L8871) (`_resolve_task_provider_model`) and [`6798-7050`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6798-L7050) (`resolve_provider_client`).
- Normalized through `_normalize_aux_provider` (e.g. `"codex"` maps to `"openai-codex"`, `"kimi"` to `"kimi-coding"`).
- Convenience direct alias: `provider: openai` expands via `_expand_direct_api_alias` to `provider: "custom"` with base URL `https://api.openai.com/v1`.
- Virtual ensemble unwrap: `provider: moa` unrolls to the preset aggregator slot provider via `_resolve_moa_aggregator`.
- Endpoint preservation: If a first-class provider has an explicit base URL (e.g. `anthropic`, `nous`, `openai-codex`), `_preserve_provider_with_base_url` retains the provider identity so custom headers, OAuth, and transport wrappers continue to operate. An unrecognized provider with a base URL collapses to `"custom"`.

### Model Resolution
- Evaluated in [`agent/auxiliary_client.py:8751-8756`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8751-L8756): Literal string `"auto"` (case-insensitive) is normalized to `None`.
- Precedence: Explicit `model` argument > `auxiliary.compression.model` > provider default aux model (`ProviderProfile.default_aux_model`) > main model (`_read_main_model_for_aux()` / `main_runtime["model"]`).
- For `"auto"` provider, `_resolve_auto_route` deliberately delays model pre-filling so the selected provider default or active main model is paired dynamically without cross-provider model pollution.

### Base URL Resolution
- Explicit `base_url` argument > `auxiliary.compression.base_url` > provider default URL.
- If an explicit provider (not `"auto"`) is supplied without a base URL, but `auxiliary.compression.base_url` is configured for that same provider, the configured base URL is preserved ([`agent/auxiliary_client.py:8836-8847`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8836-L8847)).

### API Mode Resolution
- Explicit `api_mode` argument > `auxiliary.compression.api_mode` > auto-detected transport.
- When `api_mode == "codex_responses"` or `api.openai.com` hosts a codex model, the client is wrapped in [`CodexAuxiliaryClient`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6929-L6966) to translate chat completion calls to the Responses API.
- When `api_mode == "anthropic_messages"` or the base URL contains `/anthropic`, the client is wrapped in [`AnthropicAuxiliaryClient`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6967-L6971).

### Credential Resolution
- Order: Explicit `api_key` argument > `auxiliary.compression.api_key` > `auxiliary.compression.key_env` (scoped via `_scoped_key_env`) > credential pool entry (`_peek_pool_entry`) > provider environment variables > `main_runtime["api_key"]`.
- When inheriting the main runtime, `explicit_api_key` pins to `main_runtime["api_key"]` ([`agent/auxiliary_client.py:6596-6600`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6596-L6600)) so the auxiliary call reuses the verified working key instead of rotating to an exhausted pool key.

### Header Resolution
- In [`agent/auxiliary_client.py:6731-6750`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6731-L6750), client-level default headers are applied based on the host:
  - OpenRouter: `build_or_headers()` (`HTTP-Referer`, `X-Title`).
  - GitHub Copilot: `copilot_request_headers(is_agent_turn=True, is_vision=False)`.
  - Kimi / Moonshot: `User-Agent: claude-code/0.1.0`.
  - NVIDIA NIM: `build_nvidia_nim_headers()`.
  - Codex: Cloudflare bypass headers via `_codex_cloudflare_headers()`.
- Request-level headers from `extra_headers` are merged at dispatch.

### Temperature Resolution
- [`agent/context_compressor.py:5501-5521`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5501-L5521) does not specify `temperature` in `call_kwargs`; it defaults to `temperature=None`.
- In [`agent/auxiliary_client.py:9351-9368`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L9351-L9368) (`_build_call_kwargs`):
  - Kimi models (`_is_kimi_model`): Returns `OMIT_TEMPERATURE`, dropping the field entirely so the gateway manages sampling server-side.
  - Arcee Trinity Thinking (`_is_arcee_trinity_thinking`): Returns fixed `0.5`.
  - Opus 4.7+ (`_forbids_sampling_params`): Drops temperature to `None`.
  - Other models: Temperature defaults to `None`, omitting the parameter so the provider uses its own default.
- If a provider rejects temperature with an unsupported parameter error, [`agent/auxiliary_client.py:10725-10756`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10725-L10756) strips `temperature` and retries once.

### Reasoning Resolution
- Shorthand `auxiliary.compression.reasoning_effort` is parsed by `parse_reasoning_effort` into `extra_body["reasoning"] = {"enabled": ..., "effort": ...}` in [`agent/auxiliary_client.py:9082-9126`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L9082-L9126).
- An explicit `extra_body.reasoning` in config takes precedence over the shorthand.

### Timeout Floor Resolution
- Default auxiliary timeout is 30.0s ([`agent/auxiliary_client.py:8873`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8873)).
- In [`agent/auxiliary_client.py:9065-9080`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L9065-L9080) (`_effective_aux_timeout`):
  - For `task == "compression"`, if the caller did not pass an explicit per-call timeout, the effective timeout is floored: `max(_get_task_timeout("compression"), 300.0)`.
  - A higher configured timeout (e.g. 600s) is preserved unchanged.
  - If the caller passes an explicit timeout (e.g. pinned fallback route in [`agent/context_compressor.py:95`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L95)), the floor is bypassed to respect the caller budget.

### Output-Cap and Fast Lane Resolution
- Standard compression calls deliberately omit `max_tokens` ([`agent/context_compressor.py:5511-5520`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5511-L5520)). The token target (`summary_budget`) is guidance in the prompt text only. Capping output risks truncation, CoT exhaustion on thinking models, and infinite compaction loops.
- Fast lane exception ([`agent/auxiliary_client.py:8971-9049`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8971-L9049), `resolve_compression_fast_lane`):
  - Certifies only if:
    1. Provider and model are explicit (neither is empty or `"auto"`).
    2. Actual provider and model match the requested/configured route.
    3. `reasoning_effort` explicitly disables reasoning (`"none"`, `"false"`, `"disabled"`, or boolean `false`). Provider default or empty effort is NOT certified.
    4. `max_output_tokens` is a positive integer.
  - If certified, `max_tokens` is set to `max_output_tokens` and reasoning is pinned to `{"enabled": false, "effort": "none"}`. The parameter name is mapped via `auxiliary_max_tokens_param(cap, model=final_model)` (`max_tokens` or `max_completion_tokens`).
  - If not certified, the request remains completely uncapped.

---

## 4. Primary Client Reuse versus Auxiliary Client Construction

### Client Identity and Caching Model
The Python implementation never reuses the primary conversation client object (`agent.client`).

- Primary agent client: Constructed in [`agent/agent_init.py:1554`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_init.py#L1554) with `max_retries=0` ([`agent/agent_runtime_helpers.py:2897`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L2897)). It is managed by the outer conversation loop.
- Auxiliary client: Managed by [`agent/auxiliary_client.py:8554-8677`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8554-L8677) (`_get_cached_client`). It maintains a dedicated cache dictionary `_client_cache` protected by `_client_cache_lock`.
- Cache key structure ([`agent/auxiliary_client.py:8234-8272`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8234-L8272)):
  `(provider, async_mode, base_url, api_key_key, api_mode, runtime_key, is_vision, task_key, pool_hint, model_key)`
- Cache bounds: Capped at `_CLIENT_CACHE_MAX_SIZE = 64` entries using FIFO eviction.
- Event loop binding: For async clients, `_get_cached_client` verifies that the cached client event loop matches the current running open loop; stale loops force client eviction and reconstruction.

### Isolation Rationale
As documented in [`agent/agent_runtime_helpers.py:2893-2896`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L2893-L2896), `auxiliary_client` builds its own clients and preserves SDK retries (`max_retries > 0`) because auxiliary calls are not wrapped by the outer conversation turn loop. Sharing client instances would cross-contaminate HTTP connection pools, event loops, and retry policies.

---

## 5. Fallback Ordering and Permitted Error Classes

Python uses a multi-layered fallback hierarchy for compression summaries:

```
[ContextCompressor._generate_summary]
       │
       ▼
[auxiliary_client.call_llm]
  ├── Rung 1: Same-provider transient retry (5xx, connection, 408)
  │           (Skipped for compression if full-budget timeout)
  ├── Rung 2: Parameter retries (temperature, max_tokens, auth refresh)
  └── Rung 3: Multi-provider fallback chain (should_fallback && capacity)
              ├── fallback_chain (auxiliary.compression.fallback_chain)
              ├── main fallback chain (if auto: fallback_providers)
              ├── discovery chain (if auto: openrouter -> nous -> custom -> codex)
              └── main agent model safety net (if explicit aux provider)
       │ (if all auxiliary_client candidates fail or raise)
       ▼
[ContextCompressor exception handler]
  ├── Branch 1: "no llm provider configured" -> 300s cooldown, abort
  └── Branch 2: summary_model != main_model -> One-shot fallback to main model
                (re-executes _generate_summary with summary_model="")
       │ (if main model also fails)
       ▼
[Transient Cooldown & Abort] (60s / 300s / 900s ladder)
       │
       ▼
[Stall Detection in conversation_compression.py]
  └── Host progress timeout -> pin_summary_route(fallback_chain[0]) retry
```

### Low-Level Retries inside `call_llm` ([`agent/auxiliary_client.py:10643-11186`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10643-L11186))
1. Same-provider transient retry: Up to `auxiliary.transient_retries` (default 2 retries, 3 attempts) for connection drops, resets, 5xx, or 408.
   - Critical exception: [`_should_skip_same_provider_retry("compression", err)`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10682) returns True on full-budget timeouts, skipping the same-provider retry to avoid stalling the session.
2. Parameter stripped retries:
   - Temperature rejection: Retries once without temperature ([`10725-10756`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10725-L10756)).
   - Max tokens / ZAI 1210 error: Retries once without output caps ([`10800-10822`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10800-L10822)).
   - Stale Nous catalog model (404): Refreshes recommendation from Portal and retries once ([`10824-10853`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10824-L10853)).
   - Nous 401 / expired token: Refreshes credentials and retries once ([`10855-10926`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10855-L10926)).
   - Recoverable credential pool: Marks key exhausted and rotates pool key ([`10962-11021`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10962-L11021)).

### Multi-Provider Fallback Eligibility
Governed by [`agent/auxiliary_client.py:11048-11078`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L11048-L11078):

```python
should_fallback = (
    _is_auth_error(first_err)
    or _is_payment_error(first_err)
    or _is_connection_error(first_err)
    or _is_rate_limit_error(first_err)
    or _is_model_incompatible_error(first_err)
    or _is_invalid_aux_response_error(first_err)
)
is_capacity_error = (
    _is_payment_error(first_err)
    or _is_connection_error(first_err)
    or _is_rate_limit_error(first_err)
    or _is_model_incompatible_error(first_err)
    or _is_invalid_aux_response_error(first_err)
)
if should_fallback and (is_auto or is_capacity_error):
    ...
```

- Permitted fallback errors:
  - If provider is `"auto"`: Auth (401), payment/quota (402), timeout/connection, rate limit (429), model incompatibility (400), invalid response (empty content/missing choices).
  - If provider is explicit: Only capacity/infrastructure failures permit fallback (`is_capacity_error`). Auth errors (401) on an explicit provider fail closed and do not fall back.
- Non-permitted errors:
  - Client request format errors, schema validation errors, prompt rejections, or permanent configuration errors.
  - Auth failure on explicit provider.

### Candidate Context Window Filtering
In [`agent/auxiliary_client.py:6121-6140, 6274-6287`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6121-L6140):
- For `task == "compression"`, candidates must have context window >= `MINIMUM_CONTEXT_LENGTH` (64,000 tokens).
- Candidates with known context < 64K are skipped during fallback chain traversal. Candidates with unknown context (`None`) pass through.

### High-Level Summary Model Fallback in `ContextCompressor`
In [`agent/context_compressor.py:5641-5791`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5641-L5791):
- If `self.summary_model` was configured, differs from `self.model`, and `not self._summary_model_fallen_back`:
  - Triggers on model not found (404, 503), timeout (408, 429, 502, 504), JSON decode error, streaming premature close, empty content, truncated summary, or general unexpected errors.
  - Calls `self._fallback_to_main_for_compression(e, reason)` ([`5131-5161`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5131-L5161)):
    - Marks `_summary_model_fallen_back = True`.
    - Resets `self.summary_model = ""`.
    - Clears failure cooldown.
    - Recursively calls `self._generate_summary(...)` to execute immediately on the main conversation model.
- If the error is a permanent `"no llm provider configured"` RuntimeError ([`5653-5664`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5653-L5664)), it enters a 300s cooldown without main model retry.

### Stall Fallback via Pinned Route
In [`agent/conversation_compression.py:1278-1400`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L1278-L1400):
- When an auxiliary call stalls (connection open, zero tokens moving) and is aborted by the progress fence, it never raises into `call_llm`'s exception handler.
- `resolve_compression_fallback_route()` selects the first valid entry in `auxiliary.compression.fallback_chain`.
- `pin_summary_route(route)` installs the route into ContextVar `_SUMMARY_ROUTE_PIN`.
- Compression re-executes with the pinned route kwargs and the fallback entry's specific timeout.

---

## 6. Usage Attribution and Route Identity

### Route Telemetry
- `call_llm` accepts `route_info: Optional[Dict[str, str]]` ([`agent/auxiliary_client.py:10315`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10315)).
- After client resolution or fallback, `_record_route_info` records `route_info["provider"]` and `route_info["model"]` reflecting the concrete backend that answered ([`10573-10575, 11136-11138`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10573-L10575)).
- In [`agent/context_compressor.py:5550-5569`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5550-L5569), the compressor extracts `_aux_route` and calls `_record_aux_compression_call` to persist duration, provider, model, and phase timings.
- `_set_relay_auxiliary_route` registers the active route for Relay diagnostics.

### Session DB Accounting
- Handled in [`agent/aux_accounting.py:71-139`](file:///home/eins0fx/development/hermes-agent-port/agent/aux_accounting.py#L71-L139) (`record_aux_usage`):
- ContextVar `_accounting` provides `(session_db, session_id)` published at turn start by the agent loop.
- Called from the response validation chokepoint `_validate_llm_response`.
- Tokens are normalized via `normalize_usage` (input, output, cache read, cache write, reasoning tokens).
- Cost is estimated via `estimate_usage_cost(model, usage, provider, base_url)`.
- Persisted to `session_db.record_auxiliary_usage(session_id, "compression", model=response.model, billing_provider=provider, billing_base_url=base_url, ...)`:
  - Model identity comes directly from `response.model` (accurate after fallbacks).
  - Task identity is hardcoded to `"compression"`.
  - Main turn token accumulators are unaffected.

---

## 7. Prompt-Cache Behavior and Tool-Free Contract

### Structure of the Request
In [`agent/context_compressor.py:5501-5521`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5501-L5521):
- `messages = [{"role": "user", "content": prompt}]`
- `tools = None` (omitted from `call_kwargs`)
- `system` message is not included.

### Why Summary Calls Must Remain Tool-Free
1. Window Headroom: Main agent tool definitions can exceed 20,000 to 30,000 tokens ([`agent/context_compressor.py:3757`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L3757)). Omitting tools ensures the auxiliary context window is reserved entirely for the history transcript.
2. Hallucination Prevention: Summarizer models provided with tool schemas frequently attempt to invoke tools (e.g. calling `bash` or `read_file` to verify state) rather than emitting plain markdown summary text.
3. Wire Compatibility: Many auxiliary and fast models (or local backends) do not support function calling or enforce strict tool validation that fails on freeform summary tasks.
4. Cache Invalidation Isolation: The compression prompt is an episodic, non-repeating transcript snapshot. It does not share the main conversation prefix. Sending tools would pollute the prompt cache without any reuse benefit.

---

## 8. Startup Validation and Refusal Behavior

Defined in [`agent/conversation_compression.py:2574-2795`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L2574-L2795) (`check_compression_model_feasibility`):

### Provider Unavailability
- Tests whether `get_text_auxiliary_client("compression", main_runtime=...)` produces a valid client.
- If unavailable, it checks `_try_configured_fallback_for_unavailable_client("compression", ...)`.
- If still unavailable, startup does NOT crash or refuse. It emits an operational warning (`"⚠ Configured auxiliary compression provider '...' is unavailable..."`) and records `agent._compression_warning`. Middle turns will be dropped without summary if compaction triggers.

### Hard 64K Floor Refusal
In [`agent/conversation_compression.py:2667-2683`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L2667-L2683):
- Resolves `aux_context = get_model_context_length(aux_model, ...)`.
- If `aux_context < MINIMUM_CONTEXT_LENGTH` (64,000 tokens):
  - Raises `ValueError` immediately:
    ```
    ValueError: Auxiliary compression model {aux_model} has a context window of {aux_context} tokens, which is below the minimum 64,000 required by Hermes Agent...
    ```
  - Refuses startup.

### Threshold Auto-Lowering
In [`agent/conversation_compression.py:2685-2720`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L2685-L2720):
- If `aux_context >= 64,000` but `aux_context < agent.context_compressor.threshold_tokens`:
  - Does not fail startup.
  - Automatically lowers the live session threshold: `agent.context_compressor.threshold_tokens = aux_context`.
  - Recalculates `tail_token_budget = int(new_threshold * summary_target_ratio)`.
  - Adjusts `threshold_percent = new_threshold / main_ctx`.
  - Logs a warning advising the user to configure a larger model or lower the threshold in `config.yaml`.

---

## 9. Rust Implementation Seams and Recommendations

### Verified Current Rust State
In `rust/crates/hermes-gateway`:
1. Self-Cloning Primary Client ([`native_agent.rs:1068-1070`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1068-L1070)):
   ```rust
   let mut summary_client = self.clone();
   summary_client.usage_bucket = UsageBucket::Auxiliary;
   summary_client.begin_auxiliary_usage();
   ```
   `summarize_history` and `micro_compact_after_turn` simply clone `self` (`NativeAgentClient`), directing the summary request to the primary model, endpoint, and credentials.
2. Flat Failure Cooldown ([`native_agent.rs:1252-1274`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1252-L1274)):
   When `summarize_history` errors or returns empty, Rust writes a flat 600s cooldown to `SessionDb` and fails open without any model fallback.
3. Missing Config Parsing ([`automatic_compression.rs:48-90`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/automatic_compression.rs#L48-L90)):
   `AutomaticCompressionPolicy` only parses keys from the `compression:` block. It has no fields for `auxiliary.compression`.
4. Startup Construction ([`main.rs:428-504`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L428-L504)):
   Constructs a single `NativeAgentClient` for the primary conversation turn. No auxiliary client is constructed or attached.

### Recommended Narrow Seams for Rust Port

```
[config_loader / config_types]
         │
         ▼
[auxiliary_config::AuxiliaryTaskConfig]  <- Parse auxiliary.compression
         │
         ▼
[provider_resolver::resolve_auxiliary_client]
  ├── Primary endpoint inheritance
  ├── Credential resolution (profiles, env, pool)
  └── Transport wrapper (OpenAI / Responses / Anthropic)
         │
         ▼
[NativeAgentClient]
  ├── primary_client: Arc<HttpClient>
  └── auxiliary_compression: Option<Arc<AuxiliaryCompressionClient>>
         │
         ▼
[summarize_history]
  ├── Try auxiliary_compression
  └── On failure: One-shot fallback to self (primary client)
```

1. Configuration Seam:
   - Extend configuration models to parse `auxiliary.compression` (`provider`, `model`, `base_url`, `api_key`, `key_env`, `api_mode`, `timeout`, `reasoning_effort`, `max_output_tokens`, `fallback_chain`).
2. Resolver Seam (`provider_resolver.rs` or `auxiliary_resolver.rs`):
   - Create a resolver function that maps `auxiliary.compression` config + `main_runtime` into a resolved HTTP client configuration.
   - Implement `_resolve_auto_route` precedence: primary runtime -> fallback chain -> provider discovery.
3. Client Seam (`native_agent.rs`):
   - Add `auxiliary_compression_client: Option<Arc<NativeAgentClient>>` (or a dedicated `AuxiliaryHttpClient`) to `NativeAgentClient`.
   - Update `summarize_history`:
     - If `auxiliary_compression_client` is present:
       - Execute request against auxiliary client.
       - If auxiliary call fails: log warning, mark fallback used, and execute one-shot retry against `self` (the main model).
     - If no auxiliary client is configured: execute directly on `self`.
4. Request Sanitation Seam (`auxiliary_summary_request` in `native_agent.rs:781-807`):
   - Maintain current stripping of `tools`, `tool_choice`, `parallel_tool_calls`.
   - Temperature: Apply `summary_temperature(model)` (omit for Kimi/unspecified, 0.5 for Arcee Trinity Thinking).
   - Output cap: Do NOT send output cap unless certified by fast lane.
   - Timeout: Enforce 300s minimum floor on the auxiliary request.
5. Usage Attribution Seam:
   - Ensure `record_compression_usage` passes the actual answering route model/provider to `SessionDb::record_auxiliary_usage` with task `"compression"`.

---

## 10. Directly Relevant Tests and Live Two-Endpoint Parity Traps

### Authoritative Python Tests
- [`tests/agent/test_auxiliary_client.py:4213-4350`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_auxiliary_client.py#L4213-L4350): `TestCompressionFallbackContextFilter` verifies that `fallback_chain` skips candidates with context window < 64K and verifies `_task_minimum_context_length("compression") == 64_000`.
- [`tests/agent/test_auxiliary_client.py:4293-4318`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_auxiliary_client.py#L4293-L4318): Tests that model-specific failures skip only the exact failed `(provider, model)` pair, allowing sibling models under the same provider to be tried.
- [`tests/agent/test_auxiliary_compression_timeout_floor.py:74-114`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_auxiliary_compression_timeout_floor.py#L74-L114): `TestCompressionTimeoutFloorSync` verifies that compression timeouts below 300s are elevated to 300s while explicit caller timeouts and non-compression tasks are not floored.
- [`tests/agent/test_auxiliary_config_bridge.py:155-163`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_auxiliary_config_bridge.py#L155-L163): Verifies that `compression` configuration is not bridged to process environment variables.
- [`tests/agent/test_auxiliary_main_first.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_auxiliary_main_first.py): Verifies that `"auto"` mode resolves to the user's main provider and model before attempting external fallbacks.

### Current Rust Tests in `native_agent.rs`
- `test_same_turn_compression_executes_in_place` ([line 3038](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L3038)): Tests in-place compaction.
- `test_same_turn_compression_aborts_on_empty_summary` ([line 3130](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L3130)): Verifies flat cooldown on empty summary response.
- `test_same_turn_compression_aborts_on_summary_error` ([line 3193](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L3193)): Verifies flat cooldown when the summary request fails.

### Parity Traps for a Live Two-Endpoint HTTP Test
When standing up a live two-endpoint integration test (e.g. primary model on Endpoint A, auxiliary compression model on Endpoint B):

1. Tool Schema Leakage:
   - Trap: Reusing the primary turn request payload passes `tools` and `tool_choice` to Endpoint B.
   - Result: Endpoint B burns context window on tool schemas, risks tool call hallucination, or rejects the request with HTTP 400.
   - Contract: Summary requests to Endpoint B must contain only a single user message (`messages: [{"role": "user", "content": prompt}]`) and no tool definitions.

2. Output Cap Injection on Reasoning Models:
   - Trap: Sending `max_tokens` or `max_completion_tokens` to Endpoint B when using a reasoning model.
   - Result: Reasoning models consume the token budget on internal chain-of-thought, returning truncated text (`finish_reason == "length"`) or empty content, failing compression.
   - Contract: Omit output caps completely unless explicitly certified by the fast lane.

3. Premature Timeout on Large History:
   - Trap: Using a default 30s or 60s HTTP client timeout on Endpoint B.
   - Result: Summarizing a large context window easily takes 60s to 120s on slower models, triggering a client timeout.
   - Contract: Enforce the 300s timeout floor on Endpoint B requests.

4. Client Isolation and Connection Poisoning:
   - Trap: Sharing a single `reqwest::Client` connection pool or mutating client headers between Endpoint A and Endpoint B.
   - Result: A connection timeout or socket reset on Endpoint B drops keepalive connections or poisons state for Endpoint A.
   - Contract: Endpoint A and Endpoint B must maintain separate client instances and independent timeout configurations.

5. Silent Lack of Fallback:
   - Trap: If Endpoint B returns HTTP 500, 429, or 404, failing open immediately and writing a 600s cooldown.
   - Result: Loses the summary and degrades conversation quality even though Endpoint A (the primary model) was healthy.
   - Contract: One-shot fallback must immediately re-dispatch the summary prompt to Endpoint A before entering cooldown.

6. Billing and Route Attribution Misclassification:
   - Trap: Recording Endpoint B token usage against the main chat model in `session_db`.
   - Result: Skews token metrics and analytics.
   - Contract: Record token usage under task `"compression"` with Endpoint B's actual model and provider identity.
