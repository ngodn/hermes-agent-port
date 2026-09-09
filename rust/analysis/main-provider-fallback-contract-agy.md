# Production Main-Conversation Provider Fallback Contract and Oracle Specification

## 1. Executive Summary and Scope

This specification defines the authoritative Python contract for ordinary main-conversation provider fallback (`fallback_providers` and legacy `fallback_model`) in Hermes Agent. It covers the primary turn execution path, distinguishing it completely from auxiliary compression fallback (`auxiliary.compression.fallback_chain`).

The contract reflects the exact live implementation across:
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py): Lines 365-388 (`_pool_may_recover_from_rate_limit`), 1285-1325 (`_emit_pending_fallback_notice`), 7649-7679 (`_try_activate_fallback`, `_has_pending_fallback`, `_restore_primary_runtime`, `_try_recover_primary_transport`).
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py): Lines 1743-1768 (`_sync_failover_system_message`), 3338-3388 (Nous rate-guard preflight), 3846-3859 (empty/malformed eager fallback), 4096-4110 (safety refusal fallback), 4310-4340 (content-filter stream stall rollback and fallback), 6000-6091 (classified failover for rate limit, billing, upstream rate limit, and transport), 6107-6125 (auth failover), 6811-6831 (non-retryable client error fallback), 7017-7045 (max retries exhausted fallback), 8680-8717 (outer iteration empty response fallback).
- [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py): Lines 2628-2646 (`rewrite_prompt_model_identity`, `_fallback_entry_key`), 2649-2666 (`_fallback_entry_unavailable_without_network`), 2669-2703 (`_fallback_reason_text`), 2705-3194 (`try_activate_fallback`).
- [`agent/agent_init.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_init.py): Lines 1582-1606 (`_fallback_chain` initialization), 3214-3247 (`_primary_runtime` snapshot).
- [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py): Lines 1641-1971 (`restore_primary_runtime`).
- [`hermes_cli/fallback_config.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/fallback_config.py): Lines 8-11 (`_normalized_base_url`), 14-41 (`resolve_entry_api_key`), 43-69 (`_iter_fallback_entries`), 72-77 (`_entry_identity`), 80-102 (`get_fallback_chain`).
- [`hermes_cli/runtime_provider.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/runtime_provider.py): Lines 1982-2050 (`resolve_runtime_provider`).
- [`agent/backend_identity.py`](file:///home/eins0fx/development/hermes-agent-port/agent/backend_identity.py): Lines 70-73 (`classify_failure_scope`), 87-111 (`BackendIdentity`), 161-187 (`same_deployment`), 189-205 (`should_skip_candidate`).
- [`agent/error_classifier.py`](file:///home/eins0fx/development/hermes-agent-port/agent/error_classifier.py): Lines 30-65 (`FailoverReason`), 1380-1393 (`_is_openrouter_upstream_error`).

Deterministic runtime behavior is checked by the live Python test generator [`rust/tools/gen_main_provider_fallback_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_fallback_goldens.py) against [`rust/tools/main-provider-fallback-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-fallback-goldens.json) (12 sections, 104 test cases).

---

## 2. Configuration Schema, Container Coercion, Normalization, and Merge Order

### 2.1 Container Acceptance and Extraction
Source: [`hermes_cli/fallback_config.py:43-50, 80-102`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/fallback_config.py#L43-L50)
- The system recognizes two configuration keys:
  1. `fallback_providers`: The modern canonical list format.
  2. `fallback_model`: The legacy format (originally a single dict, later widened to accept lists).
- Container coercion rules:
  - `list`: Iterated in exact source order.
  - `dict`: Coerced to a single-element list `[dict]`.
  - Non-container scalars (`str`, `int`, `float`, `bool`, `None`): Rejected and coerced to an empty list `[]`.
- Key merge order:
  - `fallback_providers` entries are evaluated first.
  - `fallback_model` entries are evaluated second and appended to the chain.
  - If both keys define the same backend route, the modern entry wins and the legacy duplicate is discarded.

### 2.2 Entry Filtering and Field Normalization
Source: [`hermes_cli/fallback_config.py:51-69`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/fallback_config.py#L51-L69)
- Within any container, items that are not dictionaries are silently ignored and dropped.
- `provider`: Must be present, non-empty, and non-whitespace. The value is stringified and stripped (`str(entry.get("provider") or "").strip()`). If empty, the entire entry is dropped.
- `model`: Must be present, non-empty, and non-whitespace. Evaluated under the same rules as `provider`.
- `base_url`: Normalized by stripping whitespace and removing trailing slashes (`value.strip().rstrip("/")`). Non-string or empty values normalize to `""`.
- Extra fields: All additional dictionary keys (`api_key`, `key_env`, `api_key_env`, `timeout`, `api_mode`, `reasoning_echo`, `extra_body`, etc.) are preserved verbatim in a shallow copy of the entry dictionary.

### 2.3 Route Identity and Sequential Deduplication
Source: [`hermes_cli/fallback_config.py:72-101`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/fallback_config.py#L72-L101)
- Route identity tuple:
  `identity = (provider.strip().lower(), model.strip().lower(), normalized_base_url.lower())`
- Deduplication semantics:
  - Sequential walk maintaining a `seen` set of identity tuples.
  - Case-insensitive across provider, model, and base URL.
  - Trailing slashes on base URLs do not create distinct routes (`https://api.openai.com/v1/` matches `https://api.openai.com/v1`).
  - First occurrence wins and preserves its original source order and attributes.
  - Sibling models on the same provider (e.g. `openrouter/llama-3-70b` vs `openrouter/claude-3-5-sonnet`) have distinct identities and are both kept.
  - Multi-endpoint pools (same provider and model with distinct explicit base URLs) have distinct identities and are both kept.

---

## 3. Entry Validation, Credential Resolution, and Local Gating

### 3.1 Credential Resolution Precedence
Source: [`hermes_cli/fallback_config.py:14-41`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/fallback_config.py#L14-L41)
For each fallback entry, API key resolution follows a strict priority cascade:
1. **Inline Secret (`api_key`)**:
   `inline = str(entry.get("api_key") or "").strip()`. If non-empty, it is returned immediately.
2. **Primary Environment Pointer (`key_env`)**:
   `key_env = str(entry.get("key_env") or "").strip()`. If non-empty, resolved via `agent.secret_scope.get_secret(key_env)` (falling back to `os.environ`).
3. **Alias Environment Pointer (`api_key_env`)**:
   If `key_env` is absent or empty, `api_key_env = str(entry.get("api_key_env") or "").strip()`. If non-empty, resolved via `get_secret(api_key_env)`.
4. **Provider Standard Fallback**:
   If neither yields a non-empty string, returns `None`. Downstream client resolution falls back to provider standard credentials (environment defaults, credential pool, or auth store).

### 3.2 Pre-Network Local Gating
Source: [`agent/chat_completion_helpers.py:2649-2666, 2758-2780`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2649-L2666)
Before issuing network requests or attempting client initialization:
- **Composite Entry Key**:
  `fb_key = (provider.strip().lower(), model.strip(), base_url.strip().rstrip("/"))`
- **Session Suppression Cache (`agent._unavailable_fallback_keys`)**:
  If `fb_key` is present in `_unavailable_fallback_keys`, the entry is skipped immediately without retry.
- **Local Credential Validation (`_fallback_entry_unavailable_without_network`)**:
  - Providers other than `nous` return `None` (presumed locally viable).
  - For `nous`, checks local `auth.json` state via `get_provider_auth_state("nous")`. If neither `access_token` nor `refresh_token` exists, returns `"nous_token_missing"`.
  - When a skip reason is returned, `fb_key` is added to `_unavailable_fallback_keys` and the chain advances recursively to the next candidate.

---

## 4. Error Classification, Trigger Reasons, and Operator Visibility

### 4.1 Fallback Trigger Points in Main Conversation Loop
Source: [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
In the ordinary main turn, fallback activation is triggered at eight distinct operational boundaries:
1. **Preflight Rate Limit Check (Lines 3344-3367)**:
   Nous Portal shared rate-limit guard detects an active reset window from a concurrent session.
2. **Eager Response Malformation (Lines 3846-3859)**:
   Empty or malformed responses (HTTP 200 with empty body/choices) are treated as proxy throttling symptoms and trigger fallback immediately.
3. **Invalid Response Retry Exhaustion (Lines 3921-3932)**:
   Repeated invalid responses exceeding `max_retries` trigger fallback before giving up.
4. **Safety Refusal (Lines 4096-4110)**:
   `finish_reason == "content_filter"` or deterministic provider safety refusal triggers fallback once (a different model/provider may not refuse).
5. **Content-Filter Stream Stall (Lines 4310-4340)**:
   Mid-stream content-filter stall terminates generation. Truncated output is rolled back to the clean turn and fallback is activated before burning standard retries.
6. **Classified Failover (Lines 6000-6091)**:
   Errors classified as `FailoverReason.rate_limit`, `FailoverReason.billing`, `FailoverReason.upstream_rate_limit`, or transport errors (`timeout`, `overloaded`) after 2 retries.
7. **Auth Failure Escalation (Lines 6107-6125)**:
   HTTP 401/403 that survives local credential refresh triggers fallback once per attempt cycle (`auth_failover_attempted`).
8. **Non-Retryable Client Errors (Lines 6811-6831)**:
   Deterministic HTTP 4xx errors (e.g. TLS failure, content policy blocked) trigger fallback before aborting.
9. **Generic Max Retries Exhaustion (Lines 7017-7045)**:
   Generic retries reaching `max_retries` attempt primary transport recovery first, then fallback before final failure.
10. **Outer Iteration Empty Response (Lines 8680-8717)**:
    Consecutive empty responses across outer tool-loop iterations trigger fallback.

### 4.2 Operator-Facing Visibility and Notice Retention
Source: [`agent/chat_completion_helpers.py:2669-2703, 3152-3166`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2669-L2703), [`run_agent.py:1285-1325`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1285-L1325)
- Every fallback activation formats a human-readable notice:
  `⚠️ Model fallback: {old_model} via {old_provider} unavailable ({reason_text}); using {fb_model} via {fb_provider}.`
- Operator-friendly translations (`_fallback_reason_text`):
  - `rate_limit` -> `"rate limit"`
  - `billing` -> `"billing or quota exhausted"`
  - `upstream_rate_limit` -> `"upstream model rate limit"`
  - `overloaded` -> `"provider overloaded"`
  - `timeout` -> `"request timeout"`
  - `auth` -> `"authentication failed"`
  - `auth_permanent` -> `"authentication permanently failed"`
  - `ssl_cert_verification` -> `"TLS certificate verification failed"`
  - `content_policy_blocked` -> `"content policy blocked the request"`
  - `model_not_found` -> `"model not found"`
  - `context_overflow` -> `"context window exceeded"`
  - `payload_too_large` -> `"request payload too large"`
  - `unknown` / `None` -> `"provider failure"`
- Durable storage: Notice is appended to `agent._pending_fallback_notice`. A successful fallback clears transient retry logs, but this one-shot notice survives and is emitted once at the next turn or status boundary.

---

## 5. Primary Credential Pool Precedence and Upstream Rate-Limit Bypass

### 5.1 Primary Pool Exhaustion Before Fallback
Source: [`run_agent.py:365-388`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L365-L388), [`agent/conversation_loop.py:6040-6056`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L6040-L6056)
When a 429 rate-limit or quota error occurs on the primary provider, Hermes decides whether to rotate credentials or activate cross-provider fallback via `_pool_may_recover_from_rate_limit(pool)`:
```python
def _pool_may_recover_from_rate_limit(pool) -> bool:
    if pool is None:
        return False
    if not pool.has_available():
        return False
    return len(pool.entries()) > 1
```
- **Single-Credential or Absent Pool**: If the pool is `None`, has 0 entries, or has exactly 1 entry, rotation has nowhere to go. Retrying the same credential repeats the 429 immediately. Hermes falls back immediately to `fallback_providers`.
- **Exhausted Pool**: If all pool entries are in cooldown (`not pool.has_available()`), rotation cannot help. Hermes falls back immediately.
- **Multi-Credential Pool with Available Keys**: If the pool has 2 or more entries and at least one is available, `_pool_may_recover_from_rate_limit` returns `True`. Fallback is NOT triggered; Hermes rotates credentials within the primary provider pool.

### 5.2 Upstream Rate-Limit Bypass
Source: [`agent/conversation_loop.py:6045-6056`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L6045-L6056), [`agent/error_classifier.py:1380-1393`](file:///home/eins0fx/development/hermes-agent-port/agent/error_classifier.py#L1380-L1393)
- Aggregators like OpenRouter return HTTP 429 when the underlying upstream provider (e.g. Anthropic, DeepSeek) is throttled, even while the user's OpenRouter API key and account quota are completely healthy.
- `agent.error_classifier` detects this pattern and classifies the error as `FailoverReason.upstream_rate_limit`.
- In `conversation_loop.py`:
  ```python
  _is_upstream = classified.reason == FailoverReason.upstream_rate_limit
  pool_may_recover = False if _is_upstream else _ra()._pool_may_recover_from_rate_limit(...)
  ```
- Result: Rotating OpenRouter keys cannot resolve upstream provider throttling. Upstream rate limits unconditionally bypass primary pool rotation and immediately trigger cross-provider fallback.

---

## 6. Upstream Rate-Limit Backoff Progression, Cooldown Escalation, and Reset-Aware Gating

### 6.1 Exponential Cooldown Escalation on Leaving Primary
Source: [`agent/chat_completion_helpers.py:2717-2738`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2717-L2738)
When fallback is activated due to `rate_limit`, `billing`, or `upstream_rate_limit`:
- Cooldown arms ONLY when leaving the primary provider:
  `(not fallback_already_active) or (primary_provider and current_provider == primary_provider)`.
- If already on a fallback candidate and chain-switching, the primary was not the source of the 429; the primary cooldown is untouched and NOT reset or extended.
- Progression formula:
  `backoff_seconds = min(60 * (2 ** backoff_count), 14400)`
  - Level 0 (first hit): 60s (1.0 min)
  - Level 1 (second consecutive hit): 120s (2.0 min)
  - Level 2: 240s (4.0 min)
  - Level 3: 480s (8.0 min)
  - Level 4: 960s (16.0 min)
  - Level 5: 1920s (32.0 min)
  - Level 6: 3840s (64.0 min)
  - Level 7: 7680s (128.0 min)
  - Level 8+: 14400s (4.0 hours, maximum cap)
- `agent._rate_limited_until` is set to `time.monotonic() + backoff_seconds`.
- `agent._rate_limit_backoff_count` is incremented by 1.

### 6.2 Non-Rate-Limit Chain Exhaustion Cooldown
Source: [`agent/chat_completion_helpers.py:2740-2755`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2740-L2755)
If the entire fallback chain exhausts and the failure was NOT a rate-limit/billing event:
- Sets `agent._rate_limited_until = max(_existing_cooldown, time.monotonic() + 5.0)` (`_FALLBACK_EXHAUSTED_COOLDOWN_S = 5.0`).
- Prevents cross-turn replay storms from immediately re-marshaling and re-failing the primary on the very next turn.

### 6.3 Reset-Aware Primary Restoration Gate
Source: [`agent/agent_runtime_helpers.py:1666-1738`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1666-L1738)
- Subscription-style providers report reset times hours or days away in credential pool metadata (`last_error_reset_at` / `next_available_at`).
- At the top of a new turn, `restore_primary_runtime` checks `pool.next_available_at()`.
- If `next_at > time.time()`, restoration is aborted, returning `False`. The session remains on the fallback provider until the reset timestamp passes, avoiding wasted turn-start failures and prompt-cache churn.

---

## 7. Chain Traversal Bounds, Candidate Skip Deduplication, and Unusable Key Suppression

### 7.1 Index Advancement and Traversal Bounds
Source: [`agent/chat_completion_helpers.py:2739, 2756-2757`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2739)
- `agent._fallback_chain`: Frozen list of normalized candidate dicts.
- `agent._fallback_index`: Cursor pointing to the next candidate to try (0-indexed).
- Activation steps:
  1. Checks `if agent._fallback_index >= len(agent._fallback_chain)` -> returns `False` (exhausted).
  2. Extracts candidate: `fb = agent._fallback_chain[agent._fallback_index]`.
  3. Increments cursor: `agent._fallback_index += 1`.
  4. If candidate is invalid or skipped, recursively invokes `_try_activate_fallback(reason)`.
- Traversal count is strictly bounded by `len(_fallback_chain)`.

### 7.2 Self-Backend Skip Deduplication via `BackendIdentity`
Source: [`agent/chat_completion_helpers.py:2782-2806`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2782-L2806), [`agent/backend_identity.py:87-205`](file:///home/eins0fx/development/hermes-agent-port/agent/backend_identity.py#L87-L205)
To prevent infinite loops or retrying the backend that just failed:
- Constructs `current_ident = BackendIdentity.build(agent.provider, agent.model, agent.base_url)`.
- Constructs `fb_ident = BackendIdentity.build(fb_provider, fb_model, fb.get("base_url"))`.
- Evaluates `should_skip_candidate(fb_ident, current_ident, FailureScope.MODEL)`:
  - **Identical Backend**: Same provider, model, and base URL -> skipped (`True`).
  - **Empty Base URLs**: Same provider and model with empty base URLs -> skipped (`True`).
  - **Sibling Model**: Same provider and base URL, but different model -> NOT skipped (`False`).
  - **Distinct Explicit Endpoints**: Same provider and model on different explicit URLs -> NOT skipped (`False`).
  - **Distinct First-Class Providers**: `xai` vs `xai-oauth` sharing same model and URL -> NOT skipped (`False`) because their credential surfaces are separate.
  - **Custom Shim Aliases**: Different custom aliases pointing to the same URL and model -> skipped (`True`).

---

## 8. Provider/Profile Mismatch Isolation and Credential Pool Rebinding

### 8.1 Detaching Primary Pool on Cross-Provider Fallback
Source: [`agent/chat_completion_helpers.py:2942-2979`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2942-L2979)
When fallback switches to a different provider (`fb_provider != primary_provider`):
- If `agent._credential_pool` is attached and its provider differs from `fb_provider`:
  - `agent._credential_pool = None`
  - `agent._credential_pool_entry_id = None`
- Prevents downstream 401/429 errors on the fallback from corrupting or quarantining primary credentials.
- If the fallback provider has its own credential pool in `auth.json`, loads and attaches `fallback_pool = load_pool(fb_provider)`.
- If the fallback targets the same provider (e.g. OpenRouter to OpenRouter with a different model), the pool is preserved.

### 8.2 Restoring Primary Pool and Fresh Selection
Source: [`agent/agent_runtime_helpers.py:1833-1927`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1833-L1927)
On turn restoration back to primary:
- The fallback provider pool is detached.
- Primary pool is reloaded via `load_pool(primary_pool_key)`.
- Re-selection: Instead of reusing the snapshot's initial API key (which may have expired or rotated during the turn), calls `pool.select()`.
- If a valid, available entry is found matching the primary provider, swaps it in via `_swap_credential(entry)`.

---

## 9. Endpoint Reconfiguration, Custom Header Preservation, and Timeout Handling

### 9.1 In-Place Agent Runtime Mutation
Source: [`agent/chat_completion_helpers.py:2928-2940, 2980-3024`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2928-L2940)
Upon activating fallback for `chat_completions`:
- Core state updated:
  - `agent.model = fb_model`
  - `agent.provider = fb_provider`
  - `agent.requested_provider = fb_provider`
  - `agent.base_url = fb_base_url`
  - `agent.api_mode = fb_api_mode`
  - `agent._reasoning_echo_flag = bool(fb.get("reasoning_echo", False))`
  - `agent._fallback_activated = True`
  - `agent._provider_fallback_active = True`
  - `agent._provider_fallback_route = (str(fb_model), str(fb_provider))`
- Caches and circuit breakers reset:
  - `agent._transport_cache.clear()`
  - `_reset_stale_streak(agent)` resets consecutive stream stall counters so the fallback provider starts with a clean slate.

### 9.2 Custom Header Preservation and Timeout Propagation
Source: [`agent/chat_completion_helpers.py:3002-3024`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L3002-L3024)
- Provider-specific headers attached to `fb_client` (`_custom_headers` or `default_headers`, e.g. User-Agent sentinels required by Kimi Coding or Qwen) are extracted.
- Stored into `agent._client_kwargs = {"api_key": ..., "base_url": ..., "default_headers": dict(fb_headers)}`.
- Request timeout: Evaluates `_fb_timeout = get_provider_request_timeout(fb_provider, fb_model)`. If specified, sets `agent._client_kwargs["timeout"] = _fb_timeout` and rebuilds the OpenAI client via `_replace_primary_openai_client(reason="fallback_timeout_apply")`.

### 9.3 Context Compressor Limits
Source: [`agent/chat_completion_helpers.py:2925-2928, 3048-3069`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2925-L2928)
- Clears `agent._config_context_length = None` so the fallback model's actual context window is resolved instead of inheriting stale limits.
- Resolves context window via `get_model_context_length(agent.model, ...)` and updates compressor via `agent.context_compressor.update_model(...)`.

### 9.4 Scoped Extra Body Management
Source: [`agent/chat_completion_helpers.py:3092-3146`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L3092-L3146)
- Keys contributed exclusively by the old provider's `custom_providers` entry are dropped from `agent.request_overrides["extra_body"]`.
- The new fallback provider's configured `extra_body` keys are merged in.
- Caller-specified overrides supplied at init are preserved untouched.

---

## 10. Request Body, Message History, and System Prompt Stability

### 10.1 System Prompt Model Identity Rewriting
Source: [`agent/chat_completion_helpers.py:2628-2639, 3150`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2628-L2639)
- To ensure self-identification queries (e.g. "what model are you?") answer accurately:
  `rewrite_prompt_model_identity(agent, fb_model, fb_provider)`
- **Last-Occurrence Rule**: The helper scans `agent._cached_system_prompt` and rewrites ONLY the final occurrence of `^Model: .*$` and `^Provider: .*$`.
- Earlier matches in prompt text (such as user text, memory blocks, or system instructions) are preserved unchanged.
- On primary restoration, `rewrite_prompt_model_identity(agent, rt["model"], rt["provider"])` reverts identity lines back to primary values.

### 10.2 In-Flight Message Synchronization
Source: [`agent/conversation_loop.py:1743-1768`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L1743-L1768)
- After fallback activation, `_sync_failover_system_message(agent, api_messages, active_system_prompt)` updates `api_messages[0]["content"]` in place.
- All prior conversation turns (`user`, `assistant`, `tool_call`, `tool_result`) remain intact in `messages`.

### 10.3 Content-Filter Stream Stall Rollback
Source: [`agent/conversation_loop.py:4327-4340`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L4327-L4340)
- If a stream was truncated mid-generation by a content filter, `truncated_response_parts` rolls back partial assistant message fragments to the last clean turn:
  `messages = agent._get_messages_up_to_last_assistant(messages)`
- Provides the fallback provider with a coherent, clean conversation boundary.

---

## 11. Tool-Loop Carryover and Multi-Turn Lifecycle Restoration

### 11.1 Within-Turn Stickiness Across Tool Rounds
- When fallback activates during round $K$ of a multi-turn tool loop:
  - Agent runtime state is mutated in place.
  - The fallback candidate responds, emitting tool calls.
  - Tools execute and results are appended to `messages`.
  - Next round $K+1$ executes using the active fallback provider.
  - **In-Turn Invariant**: Fallback STICKS across all subsequent tool rounds within the same turn.

### 11.2 Across-Turn Primary Restoration
Source: [`agent/turn_context.py:625`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py#L625), [`agent/agent_runtime_helpers.py:1641-1971`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1641-L1971)
At the start of every new user turn:
1. `turn_context.py` calls `agent._restore_primary_runtime()`.
2. Gating checks:
   - If `_fallback_activated == False`: Resets `_fallback_index = 0` (preventing stranded index) and returns `False`.
   - If `_rate_limited_until > time.monotonic()`: Returns `False`. Fallback sticks for the new turn.
   - If primary pool `next_available_at > time.time()`: Returns `False`. Fallback sticks for the new turn.
3. If gates pass, restores runtime from `agent._primary_runtime`:
   - Restores `model`, `provider`, `base_url`, `api_mode`, `api_key`, `_client_kwargs`, `request_overrides`, capabilities, and reasoning configs.
   - Clears `_transport_cache`.
   - Rebuilds OpenAI client.
   - Restores and re-selects primary credential pool.
   - Resets fallback counters: `_fallback_activated = False`, `_fallback_index = 0`, `_rate_limit_backoff_count = 0`.
   - Rewrites prompt identity back to primary.
   - Emits operator status: `✅ Primary model restored: ...`.

---

## 12. Final Error Propagation, Retry Budget Resets, and Failure Summary Structure

### 12.1 Retry Budget Reset on Fallback Activation
Source: [`agent/conversation_loop.py:6083-6090, 7038-7045`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L6083-L6090)
When `_try_activate_fallback()` succeeds:
- `retry_count = 0`: Grants the freshly activated fallback provider its full initial retry budget (`agent._api_max_retries`).
- `compression_attempts = 0`.
- `_retry.primary_recovery_attempted = False`.
- `_retry.restart_with_rebuilt_messages = True`: Breaks inner loop to rebuild request headers and messages.

### 12.2 Terminal Error Propagation When Fallback Chain Exhausts
Source: [`agent/conversation_loop.py:7046-7200`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L7046-L7200)
When all fallbacks in `_fallback_chain` are exhausted and retries expire:
1. Status buffer is flushed via `agent._flush_status_buffer()`.
2. Terminal status is emitted:
   - Billing: `❌ Billing or credits exhausted -- {_final_summary}`
   - Rate Limit: `❌ Rate limited after {max_retries} retries -- {_final_summary}`
   - General API: `❌ API failed after {max_retries} retries -- {_final_summary}`
3. The turn terminates and returns the standardized failure dictionary:
   ```json
   {
     "completed": false,
     "failed": true,
     "error": "<error summary string>",
     "api_calls": 5,
     "final_response": "API call failed after 5 retries: <error summary string>"
   }
   ```

---

## 13. Safe Chat-Completions Scope vs Deferred Transports Matrix

| Subsystem / Feature | Classification | Status for Rust Port | Architectural Rationale |
| :--- | :--- | :--- | :--- |
| `fallback_providers` list parsing | In-Scope | Ready Now | Pure config parsing and route normalization. |
| Legacy `fallback_model` dict/list | In-Scope | Ready Now | Appended after modern list with dedup. |
| Static `api_key` and `key_env` | In-Scope | Ready Now | Direct string secret or env var lookup. |
| Primary pool exhaustion before fallback | In-Scope | Ready Now | `_pool_may_recover_from_rate_limit` gates fallback. |
| Upstream rate-limit bypass | In-Scope | Ready Now | Aggregator 429 bypasses pool and fails over. |
| In-place client and header swap | In-Scope | Ready Now | OpenAI-compatible client swap and custom headers. |
| Tool-loop within-turn stickiness | In-Scope | Ready Now | Retained across tool rounds in same turn. |
| Cooldown-gated per-turn restoration | In-Scope | Ready Now | Cooldown and reset timestamp gate primary return. |
| OAuth token refresh (Anthropic/Codex/Nous) | Out-of-Scope | Deferred | Requires async OAuth exchange and file token sync. |
| Native Anthropic Messages protocol (`/v1/messages`) | Out-of-Scope | Deferred | Proprietary JSON wire structure and streaming chunks. |
| Bedrock Converse / Codex Responses API | Out-of-Scope | Deferred | AWS SDK / OpenAI Responses API wire protocols. |
| Dynamic Python plugins & custom compressors | Out-of-Scope | Deferred | Requires in-process Python interpreter hooks. |

---

## 14. Oracle Verification Results and Explicit Caveats

### 14.1 Oracle Verification Results
Execution of [`rust/tools/gen_main_provider_fallback_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_fallback_goldens.py) verifies byte-for-byte deterministic parity across **12 sections and 104 test cases**:
- Section 1 (`container_and_chain_parsing`): 23 test cases.
- Section 2 (`entry_validation_and_credentials`): 11 test cases.
- Section 3 (`trigger_reasons_and_explanations`): 18 test cases.
- Section 4 (`primary_pool_before_fallback_ordering`): 9 test cases.
- Section 5 (`upstream_rate_limit_and_cooldown_escalation`): 12 test cases.
- Section 6 (`chain_traversal_bounds_and_skip_dedup`): 8 test cases.
- Section 7 (`provider_mismatch_isolation_and_pool_rebinding`): 3 test cases.
- Section 8 (`endpoint_and_runtime_reconfiguration`): 1 test case.
- Section 9 (`request_body_and_system_prompt_stability`): 2 test cases.
- Section 10 (`tool_loop_carryover_and_turn_lifecycle`): 3 test cases.
- Section 11 (`final_error_propagation_and_budgets`): 2 test cases.
- Section 12 (`chat_completions_safe_port_matrix`): 10 test cases.

### 14.2 Explicit Caveats
The oracle proves the logical contracts and transitions in the Python codebase, with the following explicit caveats:
1. **Network Client Mocking**: During live oracle execution, `resolve_provider_client` and `get_model_context_length` are mocked to avoid real HTTP requests and unbounded network timeouts against external provider APIs. The oracle proves configuration parsing, identity construction, candidate skipping, in-place state mutation, and restoration logic, but does not prove live socket I/O against remote vendor servers.
2. **Context Compressor Internal Truncation**: Updating the context compressor updates model window limits, but this oracle exercises main provider fallback, not auxiliary token counting or LCM compression algorithms.
3. **Multi-Process File Locks**: `auth.json` file locking across multiple concurrent CLI/daemon processes is owned by the credential pool storage engine and is outside this single-process fallback contract.

---

## 15. Prioritized Minimum Viable Native Contract for Rust

To achieve full feature parity for ordinary main-conversation fallback in the next Rust checkpoint, the native implementation should proceed in five prioritized steps:

1. **Fallback Config Parser (`hermes-core` / `hermes-gateway`)**:
   - Port `get_fallback_chain(user_config)`: Accept both `fallback_providers` and legacy `fallback_model`.
   - Coerce dict to single-element array, reject scalars.
   - Normalize provider and model (trim, lower), normalize base URL (trim trailing slash).
   - Deduplicate sequentially using `(provider, model, base_url)` identity.
2. **Primary Pool Before Fallback Gate (`hermes-gateway::credential_pool`)**:
   - Implement `pool_may_recover_from_rate_limit(pool) -> bool`: Return `true` only if `pool.entries().len() > 1 && pool.has_available()`.
   - If `error.reason == FailoverReason::UpstreamRateLimit`, force `pool_may_recover = false`.
3. **Single-Turn Runtime Fallback State (`hermes-gateway::native_agent`)**:
   - Maintain `fallback_chain: Vec<FallbackRoute>`, `fallback_index: usize`, and `fallback_active: bool`.
   - On eligible failover (e.g. 429 after pool exhaustion, 402 billing, upstream 429, or transport timeout), advance `fallback_index`.
   - Skip candidates matching current backend identity using `BackendIdentity::should_skip_candidate`.
   - Rebind HTTP client and base URL in place. Reset turn `retry_count = 0`.
4. **Tool-Loop Stickiness and Last-Occurrence Prompt Rewriting**:
   - Retain the active fallback route across subsequent tool rounds within the same turn.
   - Rewrite only the last occurrence of `^Model: .*$` and `^Provider: .*$` in the system prompt.
5. **Per-Turn Primary Restoration**:
   - At the beginning of each turn, if `fallback_active == true`, check `rate_limited_until <= now` and pool `next_available_at <= now`.
   - When cooldown has elapsed, restore primary route and reset `fallback_active = false` and `fallback_index = 0`.
