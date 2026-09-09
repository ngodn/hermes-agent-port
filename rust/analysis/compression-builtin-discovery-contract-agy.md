# Auxiliary Compression Built-in Provider Discovery Contract and Oracle Specification

## 1. Executive Summary and Scope

This specification defines the authoritative Python contract for built-in auxiliary provider discovery as consumed by full context compression in provider auto mode (`provider: "auto"`).

Within the Hermes auxiliary architecture, provider discovery is the third and final resolution tier. It is engaged only after the runtime exhausts:
1. Tier 1: Task-specific configured fallback chain (`auxiliary.compression.fallback_chain` via `_try_configured_fallback_chain`).
2. Tier 2: Top-level main-model fallback chain (`fallback_providers` / `fallback_model` via `_try_main_fallback_chain`).
3. Tier 3: Built-in auxiliary provider discovery (`_get_provider_chain` / `_try_payment_fallback` / `_resolve_auto_route` Step 3).

Unlike the first two tiers, which operate on static, immutable user configuration parsed once at startup, Tier 3 built-in discovery depends upon process-shared mutable state (health TTL caches, credential pools, dynamic token refresh, and connection-poisoning evictions).

### Primary Source Locations
- `agent/auxiliary_client.py`:
  - `_get_provider_chain`: lines 4492-4510 (ordered detection chain definition)
  - `_AUX_UNHEALTHY_TTL_SECONDS` and caches: lines 4533-4536 (unhealthy tracking constants and maps)
  - `_AUX_UNHEALTHY_LABEL_ALIASES`: lines 4540-4547 (canonical label normalization table)
  - `_normalize_chain_label`: lines 4550-4560 (label normalization helper)
  - `_mark_provider_unhealthy`: lines 4562-4579 (unhealthy marking with TTL)
  - `_is_provider_unhealthy`: lines 4581-4595 (lazy expiration and health check)
  - `_log_skip_unhealthy`: lines 4597-4611 (rate-limited operator logging)
  - `_reset_aux_unhealthy_cache`: lines 4613-4618 (cache clearance hook)
  - `_is_payment_error`: lines 4620-4660 (status 402 and billing body classification)
  - `_try_openrouter`: lines 3345-3380 (OpenRouter candidate detection and free_only enforcement)
  - `_try_nous`: lines 3402-3465 (Nous candidate detection and cross-session rate guard)
  - `_try_custom_endpoint`: lines 4141-4190 (local/custom endpoint candidate detection)
  - `_resolve_api_key_provider`: lines 3180-3292 (provider catalog iteration and credential probe)
  - `_try_payment_fallback`: lines 5965-6014 (request-time payment fallback traversal)
  - `_resolve_auto_route` Step 3: lines 6637-6658 (startup/resolution-probe auto detection)
  - `_call_fallback_candidate_sync`: lines 5686-5860 (sync candidate execution and credential retry)
  - `_call_fallback_candidate_async`: lines 5862-5963 (async candidate execution)
  - `_refresh_provider_credentials`: lines 5409-5498 (OAuth and credential pool token refresh)
  - `_evict_cached_clients`: lines 5088-5101 (provider-level cached client eviction)
  - `_evict_cached_client_instance`: lines 5103-5136 (poisoned client instance eviction)
  - `_client_cache` and `_client_cache_key`: lines 8197-8272 (client pool structure and composite key)
  - `call_llm` (sync): lines 10296-10550, 11115-11175 (dispatch and fallback orchestration)
  - `_async_call_llm_impl`: lines 11870-11930 (async dispatch and fallback orchestration)
- `hermes_cli/auth.py`:
  - `PROVIDER_REGISTRY`: starts at line 249 (authoritative ordered provider registry)
  - `resolve_api_key_provider_credentials`: credential resolution from env and store
- `agent/nous_rate_guard.py`:
  - `nous_rate_limit_remaining`: lines 20-45 (cross-session rate limit persistence)

---

## 2. Three-Tier Compression Fallback Architecture

When full context compression executes in provider auto mode (`provider: "auto"`), the runtime evaluates candidate providers across three distinct architectural tiers:

```
[ Auto Compression Request ]
            |
            v
+-------------------------------------------------------------------+
| Tier 1: Task-Specific Fallback Chain                              |
| Source: auxiliary.compression.fallback_chain                      |
| Resolution: _try_configured_fallback_chain                        |
| Enforces: 64,000-token context floor                              |
| Scope: Model-aware skipping on rate limits / timeouts             |
+-------------------------------------------------------------------+
            | (if exhausted or unconfigured)
            v
+-------------------------------------------------------------------+
| Tier 2: Main-Model Fallback Chain                                 |
| Source: fallback_providers / fallback_model                       |
| Resolution: _try_main_fallback_chain                              |
| Enforces: 64,000-token context floor                              |
| Scope: Provider-wide skipping (skips failed provider & main)      |
+-------------------------------------------------------------------+
            | (if exhausted or unconfigured)
            v
+-------------------------------------------------------------------+
| Tier 3: Built-in Auxiliary Provider Discovery                     |
| Source: _get_provider_chain()                                     |
| Resolution: _try_payment_fallback / _resolve_auto_route Step 3    |
| Enforces: NO 64,000-token context floor (permissive pass-through) |
| State: Process-shared health TTL, pool refresh, client eviction   |
+-------------------------------------------------------------------+
```

### Hand-off Conditions
1. Normal Startup Route Resolution (`_resolve_auto_route`):
   - Step 1 attempts the main agent provider. If unavailable, proceeds to Step 2.
   - Step 2 checks Tier 1 (`_try_configured_fallback_chain`). If unavailable, checks Tier 2 (`_try_main_fallback_chain`). If both are absent or return no client, hands off to Step 3.
   - Step 3 traverses `_get_provider_chain()` to resolve the initial client.
2. Request-Time Failure Fallback (`call_llm` lines 11116-11134 and `_async_call_llm_impl` lines 11870-11890):
   - When a request fails with a payment error (HTTP 402 or quota keywords) or connection drop:
   - Evaluates Tier 1 (`_try_configured_fallback_chain`).
   - If Tier 1 returns `None`, evaluates Tier 2 (`_try_main_fallback_chain`).
   - If Tier 2 returns `None`, invokes Tier 3: `_try_payment_fallback(resolved_provider, task, reason=reason)`.

---

## 3. Exact Provider Discovery Chain, Ordering, and Rationale

### Call-Time Construction
Source: `agent/auxiliary_client.py:4492-4510`
The discovery chain is constructed dynamically at invocation time inside `_get_provider_chain()` rather than as a module-level constant:
```python
def _get_provider_chain() -> List[tuple]:
    return [
        ("openrouter", _try_openrouter),
        ("nous", _try_nous),
        ("local/custom", _try_custom_endpoint),
        ("api-key", _resolve_api_key_provider),
    ]
```

### Exact Ordering Rationale
1. `("openrouter", _try_openrouter)`:
   First priority. It uses the configured auxiliary model or the built-in free model when credentials exist.
2. `("nous", _try_nous)`:
   Second priority. Nous Research Portal provides direct inference with native models. It is checked after OpenRouter because Nous Portal usage is subject to tight shared rate limits (guarded by `agent.nous_rate_guard`).
3. `("local/custom", _try_custom_endpoint)`:
   Third priority. User-configured custom OpenAI-compatible or Anthropic-compatible endpoints (e.g. vLLM, Ollama, LM Studio, or private reverse proxies). Placed third so that cloud aggregation is preferred over local servers unless local is explicitly configured as primary.
4. `("api-key", _resolve_api_key_provider)`:
   Fourth priority. The broad catalog of direct API-key providers (OpenAI, Anthropic, DeepSeek, Mistral, Together, Fireworks, etc.). Evaluated last because it iterates through the full provider registry to find the first key with available credentials.

### Deliberate Exclusion of `openai-codex`
Source: `agent/auxiliary_client.py:4498-4504`
- The `openai-codex` provider (ChatGPT OAuth Codex endpoint) is **deliberately omitted** from `_get_provider_chain`.
- **Architectural Rationale**: The ChatGPT backend Codex endpoint (`chatgpt.com/backend-api/codex/responses`) accepts only an undocumented, frequently shifting allow-list of model IDs (e.g. `gpt-5.3-codex-spark`). Falling back to Codex with guessed or standard open models results in cryptic 400/404 HTTP errors.
- Codex is permitted **only** when:
  a. Explicitly configured as the user's primary main provider (`Step 1` of `_resolve_auto_route`), or
  b. Explicitly requested with a designated model via `auxiliary.compression.provider: "openai-codex"`.
- It is never probed during dynamic auto-discovery.

### Provider Counts
- Discovery Tiers: 4 try callables in `_get_provider_chain()`.
- API-Key Catalog: 71 `auth_type == "api_key"` registry entries, including aliases, in stable insertion order. The complete registry currently has 79 keys representing 51 unique provider IDs.

---

## 4. Configuration and Credential Gates per Discovery Candidate

Each discovery candidate enforces specific credential and configuration gates before returning a client:

### Candidate 1: OpenRouter (`_try_openrouter`)
Source: `agent/auxiliary_client.py:3345-3380`
- Gate 1: `free_only` Policy Check
  - Reads `auxiliary.free_only` (default `False`).
  - If `free_only` is enabled and the model is not a free SKU (does not end with `:free` and does not start with `stealth/`), OpenRouter is skipped immediately (`return None, None`). It does NOT mark the provider unhealthy in this case.
  - If not `free_only`, logs a one-time warning (`_warn_paid_lane_once`) if engaging a paid model.
- Gate 2: Credential Pool Probe
  - Calls `_select_pool_entry("openrouter")`. If a pool exists with a valid runtime API key, returns client using pool key and base URL.
  - If pool exists but has no usable keys (exhausted), falls through to env var.
- Gate 3: Scoped Environment Variable Probe
  - Checks `_scoped_key_env("OPENROUTER_API_KEY")`.
  - If missing/empty: marks `openrouter` as unhealthy with a **60-second TTL** (`_mark_provider_unhealthy("openrouter", ttl=60)`), and returns `(None, None)`.
  - If present: constructs OpenAI client with `OPENROUTER_BASE_URL` (`https://openrouter.ai/api/v1`) and required headers (`HTTP-Referer`, `X-Title`).

### Candidate 2: Nous Portal (`_try_nous`)
Source: `agent/auxiliary_client.py:3402-3465`
- Gate 1: Cross-Session Rate Limit Guard
  - Calls `nous_rate_limit_remaining()`.
  - If `remaining > 0` (another session recorded a 429 rate limit): marks `nous` unhealthy with dynamic TTL `ttl=remaining`, logs skip, and returns `(None, None)`.
- Gate 2: Auth File and Runtime Token Resolution
  - Probes `_read_nous_auth()` (reading `~/.hermes/auth.json`) and `_resolve_nous_runtime_api(force_refresh=False)`.
  - If both runtime API and auth store are absent: logs warning, marks `nous` unhealthy with **60-second TTL** (`_mark_provider_unhealthy("nous", ttl=60)`), and returns `(None, None)`.
  - If present: sets global `auxiliary_is_nous = True` and constructs a client pointing to the resolved inference base URL. The built-in fallback is `https://inference-api.nousresearch.com/v1`.

### Candidate 3: Local/Custom Endpoint (`_try_custom_endpoint`)
Source: `agent/auxiliary_client.py:4141-4190`
- Gate 1: Runtime Configuration Resolution
  - Calls `_resolve_custom_runtime()`, returning `(custom_base, custom_key, custom_mode)`.
  - If `custom_base` or `custom_key` is missing or empty, returns `(None, None)`.
- Gate 2: Codex Endpoint Rejection
  - If `custom_base` starts with `_CODEX_AUX_BASE_URL` (`https://chatgpt.com/backend-api/codex`), returns `(None, None)`. Custom discovery refuses to hijack the Codex endpoint without explicit Codex wiring.
- Gate 3: Wire Protocol Routing
  - Supports `custom_mode`:
    - `"codex_responses"`: wraps client in `CodexAuxiliaryClient`.
    - `"anthropic_messages"`: attempts `build_anthropic_client`; if SDK missing, falls back to OpenAI wire.
    - Default/Unspecified: creates standard OpenAI client with query parameter extraction and custom headers.

### Candidate 4: API-Key Catalog (`_resolve_api_key_provider`)
Source: `agent/auxiliary_client.py:3180-3292`
- Iterates the 71 API-key entries in `hermes_cli.auth.PROVIDER_REGISTRY`.
- Gate 1: Auth Type Gate
  - Checks `pconfig.auth_type == "api_key"`. Non-API-key providers (e.g. OAuth-only) are skipped.
- Gate 2: Health Gate
  - Checks `_is_provider_unhealthy(provider_id)`. If unhealthy, skipped.
- Gate 3: Anthropic Explicit Configuration Gate
  - If `provider_id == "anthropic"`: checks `is_provider_explicitly_configured("anthropic")`.
  - If Anthropic was NOT explicitly configured by the user, it is skipped! This prevents ambient Claude Code credentials from being silently consumed as an auxiliary fallback.
- Gate 4: Credential Probe
  - Checks credential pool via `_select_pool_entry(provider_id)`.
  - If no pool, checks `resolve_api_key_provider_credentials(provider_id)`.
  - If no API key is found, advances to the next provider.
- Gate 5: Auxiliary Model Resolution
  - Probes `_get_aux_model_for_provider(provider_id)`. If no model is configured or known for the provider, advances to the next provider.
- Winning Candidate:
  - First provider satisfying all gates is selected. Construct either `GeminiNativeClient` (for native Gemini base URLs) or OpenAI-compatible client with vendor-specific headers (Kimi, Copilot, NVIDIA NIM).

---

## 5. Model Choice Hierarchy and Provider Alias Normalization

### Model Selection Hierarchy by Candidate
1. OpenRouter:
   - Hierarchy: Caller-supplied model -> `auxiliary.openrouter_model` in `config.yaml` -> default constant `_OPENROUTER_MODEL` (`"nvidia/nemotron-3-ultra-550b-a55b:free"`).
2. Nous:
   - Hierarchy: If not in probe mode, queries `/api/nous/recommended-models` via `get_nous_recommended_aux_model(vision=False)`. If unreachable or in probe mode, falls back to `_NOUS_MODEL` (`"google/gemini-3.6-flash"`).
3. Local/Custom Endpoint:
   - Hierarchy: Reads main model via `_read_main_model_for_aux()`. If absent, defaults to `"gpt-4o-mini"`.
4. API-Key Catalog:
   - Hierarchy: Calls `_get_aux_model_for_provider(provider_id)`. If absent, defaults to the provider's default model in the catalog registry.

### Provider Label Normalization Table
Source: `agent/auxiliary_client.py:4540-4547, 4550-4560`
Health tracking and skip logic rely on canonical labels. `_AUX_UNHEALTHY_LABEL_ALIASES` defines the explicit mappings:

| Input Alias / Identifier | Canonical Chain Label | Notes |
| :--- | :--- | :--- |
| `"openrouter"` | `"openrouter"` | Matches `_get_provider_chain` |
| `"nous"` | `"nous"` | Matches `_get_provider_chain` |
| `"custom"` | `"local/custom"` | Normalizes short alias to chain label |
| `"local/custom"` | `"local/custom"` | Matches `_get_provider_chain` |
| `"openai-codex"` | `"openai-codex"` | Used for health tracking |
| `"codex"` | `"openai-codex"` | Normalizes short alias to canonical |
| Catalog provider (e.g. `"deepseek"`) | `"<provider_id>"` (lowercased) | Preserved as lowercased string |

In `_normalize_chain_label(provider)`:
- Returns `""` if provider is empty or None.
- Lowercases and strips whitespace.
- Maps through `_AUX_UNHEALTHY_LABEL_ALIASES.get(p, p)`.

---

## 6. Runtime Discrepancy vs Docstrings: Failed Provider Skipping in Auto Mode

A significant runtime discrepancy exists between the documented intent and the actual Python implementation in `_try_payment_fallback`.

### Documented Intent
Docstring in `agent/auxiliary_client.py:5970-5973`:
> "Try alternative providers after a payment/credit or connection error. Iterates the standard auto-detection chain, skipping the provider that failed."
Comment at lines 5980-5981:
> "# Also skip Step-1 main-provider path if it maps to the same backend. (e.g. main_provider="openrouter" -> skip "openrouter" in chain)"

### Actual Runtime Implementation
Source: `agent/auxiliary_client.py:5978-5991`
```python
    # Normalise the failed provider label for matching.
    skip = failed_provider.lower().strip()
    # Also skip Step-1 main-provider path if it maps to the same backend.
    # (e.g. main_provider="openrouter" -> skip "openrouter" in chain)
    main_provider = _read_main_provider()
    skip_labels = {skip}
    if main_provider and main_provider.lower() in skip:
        skip_labels.add(main_provider.lower())
    # Map common resolved_provider values back to chain labels.
    _alias_to_label = {"openrouter": "openrouter", "nous": "nous",
                       "openai-codex": "openai-codex", "codex": "openai-codex",
                       "custom": "local/custom", "local/custom": "local/custom"}
    skip_chain_labels = {_alias_to_label.get(s, s) for s in skip_labels}
```

### The Discrepancy Breakdown
1. The `in skip` Substring Bug:
   Notice line 5984: `if main_provider and main_provider.lower() in skip:`.
   The author intended `if main_provider: skip_labels.add(main_provider.lower())`.
   Instead, the code checks if `main_provider.lower()` is a substring of `skip`.
2. When Auto Mode Fails:
   In auto mode, `call_llm` sets `resolved_provider = "auto"`.
   When `call_llm` calls `_try_payment_fallback(resolved_provider, task, reason=reason)`, the argument `failed_provider` is `"auto"`.
   Consequently, `skip = "auto"`.
   Now evaluate line 5984:
   If `main_provider = "openrouter"`, is `"openrouter" in "auto"`? **False!**
   Therefore, `main_provider` is **NOT** added to `skip_labels`.
   Furthermore, `"auto"` is not in `_alias_to_label`, so `skip_chain_labels` contains only `{"auto"}`.
3. Behavioral Impact:
   - On Transient / Connection Errors:
     Transient connection drops or timeouts do NOT call `_mark_provider_unhealthy`.
     When `_try_payment_fallback` iterates `_get_provider_chain()`, candidate 1 is OpenRouter.
     Since `skip_chain_labels` is `{"auto"}` and OpenRouter is not unhealthy, **OpenRouter is immediately re-selected and re-attempted**, even though OpenRouter was the exact provider that just failed in Step 1!
   - On Payment Errors (HTTP 402):
     In `call_llm`, HTTP 402 explicitly calls `_mark_provider_unhealthy("openrouter")` before invoking `_try_payment_fallback`.
     In this case, OpenRouter IS skipped, but **only because of the unhealthy cache**, NOT because of `skip_chain_labels`.

---

## 7. Contrast: 64,000-Token Context Floor in Configured Chains vs Discovery

Full compression requests require substantial context windows to compact large message histories without truncation.

### Configured Chains Enforce the 64,000-Token Floor
Source: `agent/auxiliary_client.py:6121-6139, 6275-6280, 6416-6425`
In Tier 1 (`_try_configured_fallback_chain`) and Tier 2 (`_try_main_fallback_chain`):
```python
min_ctx = _task_minimum_context_length(task)  # Returns 64_000 for task == "compression"
fb_ctx = _candidate_context_window(fb_provider, resolved_model or fb_model, ...)
if fb_ctx is not None and fb_ctx < min_ctx:
    # Candidate is SKIPPED with "context too small: {fb_ctx} < 64000"
    continue
```
Any configured fallback model with fewer than 64,000 tokens (e.g. an 8K or 16K model) is strictly screened out.

### Built-in Discovery Omits Context Screening
Source: `agent/auxiliary_client.py:5965-6014` and lines 6637-6658
In Tier 3 built-in discovery (`_try_payment_fallback` and `_resolve_auto_route` Step 3):
- Neither `_task_minimum_context_length` nor `_candidate_context_window` is called.
- The loop simply calls `client, model = try_fn()`.
- If `client is not None`, the candidate is immediately accepted, regardless of its context window size!
- If a discovered candidate has an 8K or 32K context window, discovery accepts it. If the compression payload exceeds that limit, the request will fail at runtime on the provider endpoint rather than being screened out beforehand.

---

## 8. Wire Protocol and Endpoint Transport Behaviors

Candidates resolved through built-in discovery dispatch over four underlying transport protocols:

1. Standard OpenAI Chat Completions:
   - Used by: OpenRouter (`/api/v1/chat/completions`), Nous Portal (`/v1/chat/completions`), standard custom endpoints, and most catalog providers (DeepSeek, Mistral, Together, Groq, Fireworks).
   - Invocations call `client.chat.completions.create(**kwargs)`.
2. Codex Responses Protocol:
   - Used when a custom endpoint configures `api_mode: "codex_responses"`.
   - Wrapped in `CodexAuxiliaryClient`. Translates `chat.completions.create()` into SSE streaming requests against `responses.stream()`.
3. Anthropic Messages Protocol:
   - Used when a custom endpoint specifies `api_mode: "anthropic_messages"` or points to an Anthropic host, or when `anthropic` is selected from the API-key catalog.
   - Wrapped in `AnthropicAuxiliaryClient`. Translates OpenAI messages and schemas into Anthropic `/v1/messages` format.
4. Native Gemini Adapter:
   - Used when `gemini` is resolved from the API-key catalog with a native Google base URL.
   - Dispatches through `GeminiNativeClient` (`agent.gemini_native_adapter`).

### Custom Header Injection
Source: `agent/auxiliary_client.py:3227-3247, 4156-4162`
Built-in discovery preserves and injects vendor-specific headers to prevent gateway rejection:
- Kimi (`api.kimi.com`): `User-Agent: claude-code/0.1.0`
- GitHub Copilot (`githubcopilot.com`): `copilot_default_headers()` (`Editor-Version`, `Openai-Intent`)
- NVIDIA NIM (`integrate.api.nvidia.com`): `build_nvidia_nim_headers()`
- OpenRouter: `HTTP-Referer`, `X-Title`, and `X-OpenRouter-Categories`
- User overrides: `_apply_user_default_headers()` overrides standard SDK headers on custom endpoints.

---

## 9. Unhealthy Cache State Machine, TTL Rules, and Mutation Triggers

The auxiliary client maintains a process-wide in-memory cache to quarantine depleted or misconfigured providers without repeatedly burning network round trips.

### Data Structures
Source: `agent/auxiliary_client.py:4533-4536`
- `_aux_unhealthy_until: Dict[str, float]`: Maps canonical chain label to UTC epoch timestamp when the quarantine expires.
- `_aux_unhealthy_logged_at: Dict[str, float]`: Maps canonical label to the timestamp of the last log emission, enforcing a 60-second logging throttle.
- `_AUX_UNHEALTHY_TTL_SECONDS = 600` (10 minutes).

### State Transitions
```
                [ Healthy / Uncached ]
                          |
     +--------------------+--------------------+
     | 402 / Quota error  | Missing creds      | 429 Rate Limit
     | (call_llm)         | (_try_openrouter / | (Nous guard)
     |                    |  _try_nous)        |
     v                    v                    v
[ TTL = 600s ]       [ TTL = 60s ]       [ TTL = remaining ]
     |                    |                    |
     +--------------------+--------------------+
                          |
                          v
                 [ In Cache: Skipped ]
                          |
                          | (time.time() >= expires_at)
                          v
                 [ Lazily Evicted ]
                          |
                          v
                [ Healthy / Uncached ]
```

### TTL Mutation Rules
1. Payment / Quota Exhaustion (`call_llm` lines 11110-11115):
   - Trigger: `_is_payment_error(first_err)` is True (HTTP 402 or quota keywords).
   - Action: `_mark_provider_unhealthy(resolved_provider)`.
   - Duration: Default **600 seconds** (10 minutes).
2. Missing Credentials during Discovery:
   - `_try_openrouter`: If `OPENROUTER_API_KEY` is missing/empty, calls `_mark_provider_unhealthy("openrouter", ttl=60)`. Duration: **60 seconds**.
   - `_try_nous`: If no Nous auth or runtime API found, calls `_mark_provider_unhealthy("nous", ttl=60)`. Duration: **60 seconds**.
3. Dynamic Rate Limit (Nous Portal):
   - `_try_nous`: If `nous_rate_limit_remaining()` reports active rate limit, calls `_mark_provider_unhealthy("nous", ttl=_remaining)`. Duration: **Dynamic remaining seconds**.
4. Unrefreshable Stale Credential (`_call_fallback_candidate_sync` lines 5853-5858):
   - Trigger: Candidate returns 401 Unauthorized and `_refresh_provider_credentials()` fails.
   - Action: `_mark_provider_unhealthy(fb_provider or fb_label)`.
   - Duration: Default **600 seconds**.

### Error Type Isolation
- Transient network drops, connection resets, 500/502/503/504 errors, and standard 429 rate limits (outside Nous guard) do **NOT** mark providers unhealthy in `call_llm`.
- Lazy Eviction: `_is_provider_unhealthy(label)` compares `time.time() >= expires_at`. If expired, it removes the entry from `_aux_unhealthy_until` and `_aux_unhealthy_logged_at` on read.

---

## 10. Operational Paths: Startup Resolution Probe vs Request-Failure Fallback

The discovery machinery operates in two completely distinct operational modes:

### Path A: Startup / Configuration Resolution Probe
- Code Path: `_resolve_auto_route` Step 3 (lines 6637-6658).
- Context: Called when initializing the agent or when resolving an auto client before any prompt is sent.
- Behavior:
  - Invokes `_get_provider_chain()` callables.
  - Active probe flag is set (`_aux_probe_active()`).
  - Candidate try functions return stubs (`_AuxProbeClientStub`) or lightweight clients without connecting to the remote API.
  - **ZERO Model I/O**: No HTTP requests are sent to the model endpoints. No prompt tokens are transmitted.
  - Returns `(client, model, label)`.

### Path B: Request-Failure Fallback
- Code Path: `call_llm` lines 11124 and 11151; `_async_call_llm_impl` lines 11878 and 11910.
- Context: A live compression request failed on the main provider, and Tier 1 and Tier 2 fallback chains returned no viable client.
- Behavior:
  - Calls `_try_payment_fallback(resolved_provider, task, reason=reason)`.
  - Filters out skipped labels and unhealthy cache entries.
  - Instantiates a full, live client.
  - Hands the client to `_call_fallback_candidate_sync` or `_async`.
  - **Executes Real Model I/O**: Formats messages, sends HTTP payload to endpoint, and awaits completion.

---

## 11. Execution Budget: Maximum Discovery Candidates Performing Model I/O

A critical architectural invariant in `agent/auxiliary_client.py` is the strict cap on how many discovered fallback candidates are permitted to execute real model requests.

### Execution Trace in `call_llm`
Source: `agent/auxiliary_client.py:11135-11165`

```python
fb_client, fb_model, fb_label = _try_payment_fallback(resolved_provider, task, reason=reason)
if fb_client is not None:
    # Attempt Candidate 1
    fb_resp = _call_fallback_candidate_sync(fb_client, fb_model, fb_label, ...)
    if fb_resp is not None:
        return fb_resp

    # Candidate 1 returned None (only occurs on unrefreshable 401 auth error)
    # Walk discovery chain a second time
    fb_client, fb_model, fb_label = _try_payment_fallback(resolved_provider, task, reason="stale fallback credential")
    if fb_client is not None:
        # Attempt Candidate 2
        fb_resp = _call_fallback_candidate_sync(fb_client, fb_model, fb_label, ...)
        if fb_resp is not None:
            return fb_resp

# All fallbacks exhausted; raise original error
```

### Strict Candidate Budget Rules
1. Non-Authentication Failures (500, 502, 503, 504, Timeout, 429):
   - Inside `_call_fallback_candidate_sync` (lines 5775-5777):
     ```python
     except Exception as fb_err:
         if not _is_auth_error(fb_err):
             raise
     ```
   - If Candidate 1 fails with a non-auth error, it **immediately re-raises**.
   - The exception bubbles out of `call_llm`. Candidate 2 is **never** probed or invoked.
   - **Maximum Model I/O Candidates = 1**.
2. Authentication Failures (HTTP 401):
   - If Candidate 1 fails with 401:
     - Attempts token refresh via `_refresh_provider_credentials`.
     - If refresh succeeds: retries Candidate 1.
     - If refresh fails: marks Candidate 1 unhealthy (`_mark_provider_unhealthy`) and returns `None`.
   - `call_llm` catches `None`, calls `_try_payment_fallback` a second time, resolving Candidate 2 (since Candidate 1 is now marked unhealthy).
   - Executes Candidate 2 via `_call_fallback_candidate_sync`.
   - If Candidate 2 succeeds: returns response.
   - If Candidate 2 fails (auth or non-auth) or returns `None`: `call_llm` **stops**. There is no third attempt. It logs a warning and re-raises the original main provider error.
   - **Maximum Model I/O Candidates = 2**.

**Absolute Invariant**: The maximum number of discovery candidates performing model I/O is **1** for non-auth errors, and at most **2** under unrefreshable auth errors. Under no circumstances does the runtime iterate through candidate 3, 4, etc.

---

## 12. Cached-Client Lifecycle and Credential Refresh Loop

### Client Cache Architecture
Source: `agent/auxiliary_client.py:8197-8284, 8585-8675`
- Storage: `_client_cache: Dict[tuple, tuple]` protected by `_client_cache_lock = threading.Lock()`.
- Maximum Size: `_CLIENT_CACHE_MAX_SIZE = 64` entries. FIFO eviction when size limit is exceeded.
- Value Tuple: `(client, default_model, bound_loop)`.

### Composite Cache Key Structure
Source: `agent/auxiliary_client.py:8234-8272`
Cache key is an immutable 10-element tuple:
```python
cache_key = (
    provider,             # str: canonical provider name
    async_mode,           # bool: True for async client, False for sync
    base_url or "",       # str: normalized base URL
    api_key_key,          # tuple: ("api-key-digest", blake2b_digest) or CallableDiscriminator
    api_mode or "",       # str: wire protocol override ("chat_completions", etc.)
    runtime_key,          # tuple: hashed main runtime fields if provider == "auto" else ()
    is_vision,            # bool: vision task flag
    task_key,             # tuple: (task, prefers_fast_model) if provider == "auto" else ""
    pool_hint,            # str: active pool entry id if pool backed
    model_key,            # str: resolved model name
)
```

### Event Loop Invalidation (Async)
Source: `agent/auxiliary_client.py:8607-8627`
- For async clients, `_get_cached_client` verifies that `cached_loop is current_loop` and `not cached_loop.is_closed()`.
- If the event loop has closed or changed (e.g. across async tasks), the cached client is forcefully closed, evicted from `_client_cache`, and rebuilt.

### Eviction on Credential Refresh
Source: `agent/auxiliary_client.py:5088-5101, 5409-5498`
- When an OAuth or pool token is refreshed in `_refresh_provider_credentials(provider)`:
  - Calls `_evict_cached_clients(normalized_provider)`.
  - Scans `_client_cache` under lock, closes matching clients, and purges entries.
  - Ensures the next request constructs a fresh client with the updated token.

### Instance Eviction on Transport Poisoning
Source: `agent/auxiliary_client.py:5103-5136`
- When a timeout, socket reset, or stream death occurs, `_evict_cached_client_instance(target)` locates the specific client instance in `_client_cache` and removes it, preventing poisoned HTTP/2 connections from corrupting subsequent requests.

---

## 13. Process-Shared Mutable State vs Frozen Per-Conversation Plan

A fundamental architectural distinction separates conversation-level execution from process-shared discovery state:

| Dimension | Frozen Per-Conversation Plan | Process-Shared Mutable State |
| :--- | :--- | :--- |
| **Components** | Task fallback chain (`auxiliary.compression.fallback_chain`), Main fallback chain (`fallback_providers`), Task timeout (300s compression ceiling), Conversation history. | Unhealthy cache (`_aux_unhealthy_until`), Client cache (`_client_cache`), Credential pools and OAuth refresh tokens, Nous rate guard state. |
| **Mutability** | **Immutable**: resolved and frozen at startup or conversation initiation; cloned across turns. | **Mutable**: updated continuously across all concurrent gateway sessions and conversations. |
| **Lifetime** | Scoped to the individual conversation lifecycle or turn. | Scoped to the entire OS process lifespan. |
| **Cross-Session Isolation** | Completely isolated; one conversation's prompts and context do not leak to another. | Shared across all sessions. If Session A receives a 402 from OpenRouter, Session B immediately skips OpenRouter. |
| **Thread Safety** | Thread-safe by virtue of immutability. | Requires synchronization (`_client_cache_lock`, atomic dictionary updates, thread-safe timestamp queries). |

---

## 14. Full-Parity Design Requirements for `hermes-gateway`

This section is a design recommendation derived from the contract, not a claim
that all prerequisite native credential and transport managers already exist.
An incremental checkpoint may safely implement the static API-key,
OpenAI-compatible subset if it records the exclusions explicitly and preserves
the ordering, health, prompt, and execution-budget invariants for that subset.

To achieve complete behavioral parity with the Python contract, the native Rust implementation in `hermes-gateway` must satisfy the following architectural requirements:

### 1. Dynamic Seam Interface (`CompressionDiscovery`)
As established in `compression-builtin-discovery-rust-seam-claude.md`, built-in discovery cannot be a static `Vec<NativeAgentClient>`.
- `CompressionRoutes` must own an `Option<Arc<dyn CompressionDiscovery>>`.
- The trait must provide:
  - `fn next_ready(&self, skip: &DiscoverySkip, now: Instant) -> Option<NativeAgentClient>;`
  - `fn record_outcome(&self, identity: &BackendIdentity, outcome: DiscoveryOutcome, now: Instant);`

### 2. Process-Shared Health State
- Health tracking must live in a process-shared structure behind `Arc<RwLock<...>>` or `Arc<Mutex<...>>`.
- Must store monotonic expiration timestamps (`std::time::Instant`).
- Default TTL: 600 seconds for payment/quota exhaustion; 60 seconds for missing credentials.
- Lazy eviction on lookup.

### 3. Exact Discovery Ordering
- Must evaluate providers in exact priority order:
  1. OpenRouter
  2. Nous Portal
  3. Custom Endpoint
  4. API-Key Catalog (iterating 71 API-key registry keys, including aliases, in stable order)
- Must strictly exclude `openai-codex` from discovery traversal.

### 4. Permissive Context Window Policy
- Unlike configured fallback tiers (which enforce the 64,000-token minimum), the discovery tier must **NOT** reject candidates based on context window length. It must allow discovered candidates to attempt the request.

### 5. Execution Budget Enforcer
- The request runner must enforce the strict execution budget:
  - Maximum 1 candidate for non-auth errors (timeout, 5xx, 429). Immediate failure propagation without falling back to candidate 2.
  - Maximum 2 candidates only when candidate 1 encounters an unrefreshable 401 auth failure.

### 6. Client Reuse and Eviction
- Cache built clients by composite identity `(provider, base_url, api_key_hash, model)`.
- Evict cached client instances on transport error or token refresh.

---

## 15. Verification Commands and Oracle Artifacts Summary

### Oracle Artifacts
1. Specification Report:
   `rust/analysis/compression-builtin-discovery-contract-agy.md` (this file).
2. Deterministic Golden Generator:
   `rust/tools/gen_compression_builtin_discovery_goldens.py`.
3. Golden JSON Dataset:
   `rust/tools/compression-builtin-discovery-goldens.json`.

### Quantitative Metrics
- Built-in Discovery Tiers: 4 (`openrouter`, `nous`, `local/custom`, `api-key`).
- API-Key Catalog Entries: 71 API-key registry keys, including aliases.
- Golden Test Cases: 97 deterministic cases covering:
  - Provider chain and order: 23 cases
  - OpenRouter gates: 13 cases
  - Nous gates: 6 cases
  - Custom endpoint gates: 8 cases
  - API-key catalog discovery: 14 cases
  - Context-window contrast: 12 cases
  - Health mutation and TTL: 8 cases
  - Credential refresh and eviction: 8 cases
  - Startup versus runtime budget: 5 cases

### Validation Commands
To execute the generator and verify deterministic output parity:
```bash
# 1. Regenerate golden JSON from source logic
.venv/bin/python3 rust/tools/gen_compression_builtin_discovery_goldens.py

# 2. Validate checked-in golden JSON against generator logic
.venv/bin/python3 rust/tools/gen_compression_builtin_discovery_goldens.py --check

# 3. Verify zero em-dash characters exist across all authored files
python3 -c "
import sys
files = [
    'rust/analysis/compression-builtin-discovery-contract-agy.md',
    'rust/tools/gen_compression_builtin_discovery_goldens.py',
    'rust/tools/compression-builtin-discovery-goldens.json'
]
failed = False
for f in files:
    with open(f, 'r', encoding='utf-8') as fp:
        content = fp.read()
        if '\u2014' in content:
            print(f'ERROR: Em-dash found in {f}')
            failed = True
if not failed:
    print('OK: All files contain zero em-dash characters.')
    sys.exit(0)
sys.exit(1)
"
```
