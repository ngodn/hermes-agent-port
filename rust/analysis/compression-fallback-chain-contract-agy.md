# Auxiliary Compression Fallback Chain Contract and Oracle Specification

## 1. Overview and Scope

This document specifies the authoritative Python runtime contract for `auxiliary.compression.fallback_chain` in Hermes Agent. It covers the ordinary summary request failure path in `agent/auxiliary_client.py` and `agent/context_compressor.py`, as well as the related stall-interrupted retry path in `agent/conversation_compression.py`.

The goal of this specification is to provide the exact behavioral requirements, type coercions, skipping rules, and edge-case contracts needed to implement a bounded Rust configuration and client model in `hermes-gateway`, backed by the deterministic test corpus in `rust/tools/compression-fallback-chain-goldens.json`.

### Multi-Tier Fallback Hierarchy for Compression Summaries

The full summary generation pipeline executes a layered fallback strategy:

1. Level 0 (Primary Auxiliary Route): Configured under `auxiliary.compression` (or inheriting main runtime if `provider: "auto"`).
2. Level 1 (Same-Provider Transient Retries inside `call_llm`): Retries up to `auxiliary.transient_retries` (default 2) for 5xx, socket errors, and HTTP 408.
   - Critical exception: `_should_skip_same_provider_retry("compression", err)` skips same-provider retries on full-budget timeouts to avoid stalling the session.
3. Level 2 (Parameter-Stripped Retries inside `call_llm`): Strips rejected sampling parameters (temperature), resets uncertified output token caps, and refreshes expired provider tokens.
4. Level 3 (Configured Task Fallback Chain): Traverses `auxiliary.compression.fallback_chain` via `_try_configured_fallback_chain`. Each candidate is checked against failure-scoped route identity skipping and the 64,000-token context window floor.
5. Level 4 (Secondary Fallbacks inside `call_llm`):
   - For `provider: "auto"`: Walks top-level `fallback_providers` via `_try_main_fallback_chain`, followed by the built-in auto-discovery chain (OpenRouter -> Nous -> Custom -> Codex -> API-key catalog).
   - For explicit aux providers: Walks `_try_main_agent_model_fallback`, attempting the user's primary chat model before abandoning the request.
6. Level 5 (High-Level Summary Model Fallback in `ContextCompressor._generate_summary`): If all `call_llm` layers fail or raise, the compressor catches the exception. If `summary_model` was configured and differs from `model`, it executes an immediate one-shot retry directly on the main conversation model.
7. Stall Detection Fallback (`conversation_compression.py`): When an auxiliary request hangs without emitting tokens and is aborted by the progress fence, it never reaches the `call_llm` exception handler. The stall handler calls `resolve_compression_fallback_route()`, extracts the first structurally complete entry from `auxiliary.compression.fallback_chain`, and pins it via `pin_summary_route(route)` for one bounded retry.

---

## 2. Configuration Schema and Coercion Rules

Configuration for the fallback chain is located at `auxiliary.compression.fallback_chain` in `config.yaml`.

### Container Acceptance and Ordering

- Source: `agent/auxiliary_client.py:6221-6224` and `agent/conversation_compression.py:1300-1305`
- The container must be a list (or YAML sequence). If missing, null, or of non-list type (dict, string, integer, boolean), the fallback chain evaluates to an empty list.
- Entries are parsed and evaluated strictly in source order (index 0, 1, 2, ...).
- Non-object entries (strings, numbers, booleans, lists, null) are silently rejected and skipped.

### Field Parsing and Normalization

Each entry dictionary supports the following fields:

1. `provider` (Required):
   - Must be a non-empty string after trimming whitespace.
   - If missing, null, not a string, or empty/whitespace-only, the entry is rejected.
   - Normalized to lowercase for routing comparisons (`_norm_provider`).

2. `model` (Optional):
   - Target model identifier string.
   - Whitespace trimmed; empty string normalized to `None`.
   - In `_resolve_fallback_entry` (`agent/auxiliary_client.py:6340-6342`) and `resolve_compression_fallback_route` (`agent/conversation_compression.py:1310-1315`), an entry without a non-empty model cannot resolve to a client. It is skipped during resolution and recorded as tried.
   - In primary task config, `"auto"` is normalized to `None`. In fallback entries, a literal `"auto"` model passes to `resolve_provider_client`.

3. `base_url` (Optional):
   - Endpoint URL string.
   - Whitespace trimmed; empty string normalized to `None`.

4. `api_key` (Optional):
   - Explicit inline API key secret.
   - Whitespace trimmed; empty string normalized to `None`.

5. `key_env` / `api_key_env` (Optional):
   - Environment variable name holding the API key.
   - Whitespace trimmed; empty string normalized to `None`.
   - `key_env` takes precedence over `api_key_env`.

6. `api_mode` / `transport` (Optional):
   - Wire protocol format override.
   - `api_mode` takes precedence over `transport` when both are present.
   - Normalized using `_canonical_api_mode` (`hermes_cli/config.py:1583-1617`):
     - `"openai"`, `"openai_chat"`, `"openai-chat"`, `"chat-completions"`, `"chatcompletions"` -> `"chat_completions"`
     - `"responses"`, `"openai_responses"`, `"openai-responses"` -> `"codex_responses"`
     - `"anthropic"`, `"anthropic-messages"`, `"messages"` -> `"anthropic_messages"`
     - `"bedrock"`, `"bedrock-converse"` -> `"bedrock_converse"`
   - Case-insensitive comparison; unrecognized custom strings pass through unchanged.
   - Empty or omitted values evaluate to `None`.

7. `timeout` (Optional, Independent per-entry timeout):
   - Source: `_coerce_positive_timeout` in `agent/auxiliary_client.py:5546-5555`.
   - Coerces positive `int` and `float` values to `float` (e.g. `45` -> `45.0`, `60.5` -> `60.5`).
   - Rejects boolean values (`True`/`False`), non-positive values (`0`, `-10`), strings (`"45"`), null, lists, and dicts, returning `None`.
   - Independent Timeout Contract: The ordinary Python summary fallback path does NOT apply the 300.0s compression timeout floor to per-entry timeouts. If an entry specifies `timeout: 45`, the fallback request executes with a 45s timeout (`agent/auxiliary_client.py:5720-5727`). If an entry omits `timeout` or provides an invalid value, `_fallback_entry_timeout` returns `None`, and the request keeps the task-level timeout (which has the 300.0s floor).

8. `extra_body` (Optional):
   - Vendor-specific payload extensions dictionary.
   - Must be a dictionary / object. Non-dict values evaluate to an empty dictionary.

9. `reasoning_effort` (Optional):
   - Thinking effort shorthand.
   - Parsed by `hermes_constants.parse_reasoning_effort`:
     - `"none"`, `False`, `"false"`, `"disabled"` -> `{"enabled": false}`
     - `"low"` -> `{"enabled": true, "effort": "low"}`
     - `"medium"` -> `{"enabled": true, "effort": "medium"}`
     - `"high"` -> `{"enabled": true, "effort": "high"}`
     - `None`, `True`, or unrecognized strings -> `None`

10. `max_output_tokens` (Optional):
    - Integer output token cap.
    - Positive integer coerced to `u64`. Booleans, zero, and negative values are rejected (`None`).

---

## 3. Credential Resolution Precedence

Credential resolution for fallback entries follows `hermes_cli.fallback_config.resolve_entry_api_key`:

1. Inline `api_key`: If non-empty after trimming, it is selected immediately.
2. `key_env`: If present and non-empty, looks up the secret using `agent.secret_scope.get_secret(key_env)`. When profile multiplexing is disabled, `get_secret` reads from `os.environ`.
3. `api_key_env`: Used if `key_env` is absent or empty.
4. If neither produces a non-empty string, returns `None`. The downstream provider resolver will then attempt provider-specific environment variables or fail if unauthenticated.

Direct key resolution does not consult provider catalog profiles or rotate credential pools. Live secret strings are never logged or exported to golden fixtures.

---

## 4. Route Identity and Failure-Scoped Candidate Skipping

The identity and skipping logic is centralized in `agent/backend_identity.py` (`BackendIdentity`, `FailureScope`, `should_skip_candidate`, `classify_failure_scope`).

### Failure Scope Classification

When an auxiliary request fails, the failure reason determines which identity axis is invalidated:

- `FailureScope.MODEL`: Applies to model-specific runtime errors:
  - `"timeout"`
  - `"connection error"`
  - `"rate limit"` (HTTP 429)
  - `"model incompatible with route"` (HTTP 400)
  - `"invalid provider response"` (HTTP 200 with empty body or malformed payload)
  - Any unrecognized failure reason string defaults conservatively to `FailureScope.MODEL`.
- `FailureScope.CREDENTIAL`: Applies to provider-wide authentication or billing failures:
  - `"auth error"` (HTTP 401)
  - `"payment error"` (HTTP 402)
  - Any failure where `failed_model` is `None`.
- `FailureScope.ENDPOINT`: Applies to host/network transport reachability failures (DNS resolution failure, connection refused).

### Skipping Predicate (`should_skip_candidate`)

Given candidate identity `(provider_c, model_c, base_url_c)` and failed identity `(provider_f, model_f, base_url_f)`:

1. Under `FailureScope.MODEL`:
   - Checks `same_deployment`:
     - Both providers must match (case-insensitive normalized).
     - Both models must match (case-insensitive normalized).
     - If both sides declare distinct explicit `base_url` values, they represent distinct pool endpoints and are NOT skipped.
   - Sibling Model Preservation: If `provider_c == provider_f` but `model_c != model_f`, the candidate is NOT skipped. This ensures a fallback chain listing multiple models under the same provider (e.g. NVIDIA NIM or OpenRouter) can try sibling models if the primary model times out or hits a rate limit.

2. Under `FailureScope.CREDENTIAL`:
   - Checks `same_credential_surface`:
     - If both sides declare a provider label, candidate is skipped whenever `provider_c == provider_f`.
     - Models are not compared. All candidate entries under the failed provider are skipped because they share the broken credential surface.

3. Under `FailureScope.ENDPOINT`:
   - Checks `same_endpoint`:
     - If both sides declare `base_url`, candidate is skipped when `base_url_c == base_url_f`.
     - Otherwise, skipped if `provider_c == provider_f`.

---

## 5. Minimum Context Window Filtering

Context compression history transcripts are large. Calling a candidate with an inadequate context window will fail with prompt overflow.

- Constant: `MINIMUM_CONTEXT_LENGTH = 64_000` tokens (`agent/model_metadata.py:10`).
- Scope: In `agent/auxiliary_client.py:6121-6140` (`_task_minimum_context_length`), only `task == "compression"` carries an explicit minimum floor (64,000). Other tasks return `None`.
- Filter Rule in `_try_configured_fallback_chain` (`agent/auxiliary_client.py:6274-6287`):
  - Resolves candidate context window via `_candidate_context_window(provider, model, base_url, api_key)`.
  - If context length is known and `< 64_000`, the candidate is skipped and chain traversal continues.
  - If context length is unknown (`None`) or `>= 64_000`, the candidate is accepted.

---

## 6. Secondary Fallbacks and Chain Exhaustion

When all entries in `auxiliary.compression.fallback_chain` are invalid, skipped, or fail during execution:

1. Exhaustion in `call_llm` (`agent/auxiliary_client.py:11115-11134`):
   - For `provider: "auto"`: Falls through to `_try_main_fallback_chain` (reading top-level `fallback_providers`), and then `_try_payment_fallback` (the built-in discovery chain).
   - For explicit aux provider: Falls through to `_try_main_agent_model_fallback`. This attempts the main conversation model using the same failure-scoped skipping logic (skipping main model if it matches the failed deployment, or skipping the provider if auth failed).
2. Exhaustion in `ContextCompressor._generate_summary` (`agent/context_compressor.py:5641-5791`):
   - If `call_llm` raises an exception:
     - Permanent `"no llm provider configured"` RuntimeError: enters 300s cooldown without retry.
     - Any other failure (model not found, timeout, decode error, streaming close, empty content, truncated summary, or general error):
       - If `self.summary_model` was set, differs from `self.model`, and `not self._summary_model_fallen_back`:
         - Triggers `_fallback_to_main_for_compression`.
         - Marks `_summary_model_fallen_back = True`.
         - Resets `self.summary_model = ""`.
         - Clears failure cooldown.
         - Recursively calls `_generate_summary` on the main conversation model.
     - If `summary_model` was already main model or already fell back: enters failure cooldown (escalating 60s -> 300s -> 900s for timeout errors, flat 60s for other transient errors).

---

## 7. Edge-Case Matrix

| Scenario | Input / State | Authoritative Python Behavior | Rust Parity Requirement |
|---|---|---|---|
| Non-list container | `auxiliary.compression.fallback_chain: "none"` | Evaluates to empty list, no fallback | Ignore non-array values, default to empty `Vec` |
| Non-object entry | `fallback_chain: ["openrouter/llama-3"]` | Skipped silently in loop | Reject non-object elements |
| Missing provider | `fallback_chain: [{"model": "gpt-4o"}]` | Rejected, `provider` is required | Filter out entries without non-empty `provider` |
| Whitespace provider | `fallback_chain: [{"provider": "   "}]` | Rejected as empty provider | Trim whitespace, reject if empty |
| Omitted model | `fallback_chain: [{"provider": "openrouter"}]` | Accepted into config; resolution returns `(None, None)` | Preserve `model: None`; resolution skips entry |
| Model `"auto"` | `fallback_chain: [{"provider": "p", "model": "auto"}]` | Kept as `"auto"` in entry dict; passed to router | Preserve string `"auto"` or normalize per module rules |
| Transport alias | `{"transport": "responses"}` | Read via `entry.get("api_mode") or entry.get("transport")` | Support `transport` as alias for `api_mode` |
| API mode precedence | `{"api_mode": "anthropic", "transport": "responses"}` | `api_mode` wins (`"anthropic_messages"`) | Read `api_mode` before falling back to `transport` |
| Canonical API mode | `{"api_mode": "chat-completions"}` | Canonicalized to `"chat_completions"` | Map via `normalize_api_mode` helper |
| Positive integer timeout | `{"timeout": 45}` | Coerced to `45.0s`, no 300s floor applied | Store `Some(Duration::from_secs_f64(45.0))` |
| Fractional timeout | `{"timeout": 60.5}` | Coerced to `60.5s`, no floor applied | Store `Some(Duration::from_secs_f64(60.5))` |
| String timeout | `{"timeout": "45"}` | Rejected by `_coerce_positive_timeout` (`None`) | Python rejects strings; Rust can parse or reject |
| Boolean timeout | `{"timeout": True}` | Rejected by `_coerce_positive_timeout` (`None`) | Reject boolean values |
| Non-positive timeout | `{"timeout": 0}` or `{"timeout": -10}` | Rejected (`None`) | Reject values `<= 0.0` |
| Omitted timeout | `{"provider": "openrouter"}` | Resolves to `None`, preserves task-level timeout | Store `timeout: None`, fallback to task timeout |
| Secret precedence | `{"api_key": "sk-1", "key_env": "VAR"}` | Inline `api_key` wins over `key_env` | Check `api_key` before resolving `key_env` |
| Env key alias | `{"api_key_env": "VAR"}` | Read when `key_env` is absent | Check `key_env` then `api_key_env` |
| Model failure skip | Failed `(openrouter, m1)`, candidate `(openrouter, m2)` | Sibling model is NOT skipped | Match `(provider, model)`; allow sibling models |
| Credential failure skip | Failed `(openrouter, m1)` on 401, candidate `(openrouter, m2)` | All candidates on provider skipped | Match `provider` only; skip entire provider |
| Small context window | Candidate context window 8,192 < 64,000 | Skipped with warning log | Skip candidates below 64,000 tokens |
| Unknown context window | Candidate context window is `None` | Passed through to request | Allow unknown context models to attempt call |
| Total chain exhaustion | All candidates skipped or fail | Falls to secondary main agent fallback | Return `None`; trigger main model fallback |

---

## 8. Recommended Bounded Rust Checkpoint

For the configuration-model lane in `rust/crates/hermes-gateway/src/compression_auxiliary.rs`, implement a dedicated, typed representation for ordered fallback chain entries:

### Proposed Data Structures

```rust
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FallbackChainEntry {
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub key_env: Option<String>,
    pub api_mode: Option<String>,
    pub timeout: Option<std::time::Duration>,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub reasoning_config: Option<serde_json::Value>,
    pub max_output_tokens: Option<u64>,
}

impl FallbackChainEntry {
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let provider = text(object.get("provider"))?.to_lowercase();
        if provider.is_empty() {
            return None;
        }

        let model = text(object.get("model"));
        let base_url = text(object.get("base_url"));
        let api_key = text(object.get("api_key"));
        let key_env = object
            .get("key_env")
            .filter(|v| crate::python_value::truthy(v))
            .or_else(|| {
                object
                    .get("api_key_env")
                    .filter(|v| crate::python_value::truthy(v))
            })
            .and_then(|v| text(Some(v)));

        let api_mode = text(object.get("api_mode"))
            .or_else(|| text(object.get("transport")))
            .map(normalize_api_mode);

        let timeout = entry_timeout_seconds(object.get("timeout"));

        let extra_body = object
            .get("extra_body")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();

        let reasoning_config = object
            .get("reasoning_effort")
            .filter(|v| !v.is_null() && v.as_str() != Some(""))
            .and_then(crate::reasoning_effort::parse_value);

        let max_output_tokens = positive_integer(object.get("max_output_tokens"));

        Some(Self {
            provider,
            model,
            base_url,
            api_key,
            key_env,
            api_mode,
            timeout,
            extra_body,
            reasoning_config,
            max_output_tokens,
        })
    }

    pub fn direct_api_key(
        &self,
        dotenv: &std::collections::HashMap<String, String>,
        mut environment: impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        self.api_key.clone().or_else(|| {
            self.key_env.as_ref().and_then(|name| {
                environment(name)
                    .or_else(|| dotenv.get(name).cloned())
                    .map(|value| {
                        value
                            .trim_matches(crate::python_value::python_whitespace)
                            .to_owned()
                    })
                    .filter(|value| !value.is_empty())
            })
        })
    }
}
```

### Extending `Config`

Add `pub fallback_chain: Vec<FallbackChainEntry>` to `Config`. In `Config::from_value`:
- Read `section.get("fallback_chain").and_then(Value::as_array)`.
- Map each item with `FallbackChainEntry::from_value`.
- Retain valid entries in source order.

### Independent Timeout Parsing

Use a dedicated helper for entry timeouts that avoids the 300.0s floor:

```rust
fn entry_timeout_seconds(value: Option<&serde_json::Value>) -> Option<std::time::Duration> {
    let value = value?;
    if value.is_boolean() {
        return None;
    }
    let parsed = match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s
            .trim_matches(crate::python_value::python_whitespace)
            .parse::<f64>()
            .ok(),
        _ => None,
    }?;
    if parsed.is_finite() && parsed > 0.0 {
        Some(std::time::Duration::from_secs_f64(parsed))
    } else {
        None
    }
}
```

---

## 9. Explicit Deferrals (Rust Native vs Deferred)

### Safe for Current Native Rust Implementation
1. Parsing `auxiliary.compression.fallback_chain` entries in source order into typed `FallbackChainEntry` structs.
2. Direct `api_key` and `key_env` / `api_key_env` credential resolution using environment and dotenv mappings.
3. Chat completions endpoints (OpenAI wire protocol).
4. Independent unfloored per-entry timeouts.
5. In-memory failure-scoped skipping (`FailureScope::Model` vs `FailureScope::Credential`).
6. One-shot fallback to the main conversation model (`NativeAgentClient` self) when auxiliary compression fails.

### Must Remain Deferred
1. Responses API Adapter (`codex_responses`): Native Rust does not currently have a Responses API client adapter; non-chat-completion endpoints cannot be dialed directly.
2. Anthropic Messages Transport Adapter (`anthropic_messages`): Native Rust summary calls use OpenAI-compatible chat completions payloads. Native Anthropic Messages translation remains deferred.
3. Bedrock Converse Transport Adapter (`bedrock_converse`): AWS Bedrock transport formatting is deferred.
4. OAuth Token Refresh Mechanisms: Dynamic credential refreshes for Nous, OpenAI Codex OAuth, and xAI OAuth are complex multi-step flows managed by Python sidecars or external auth pools.
5. Credential Pool Rotation: Dynamic key leasing and health marking across SQLite auth pools (`_peek_pool_entry`, `_mark_provider_unhealthy`) remain deferred to startup or dedicated pool workers.
6. Dynamic Context Window Network Probing: Querying remote provider catalogs (e.g. Nous Portal or dynamic model lists) at runtime. Rust should rely on static catalog lookups or permissive passthrough for unknown context lengths.
