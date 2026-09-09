# Auxiliary Compression Main-Model Fallback Chain Contract and Oracle Specification

## 1. Executive Summary and Scope

This specification defines the authoritative Python contract for the top-level main-model fallback chain (`fallback_providers` and legacy `fallback_model`), as consumed by auxiliary compression in provider auto mode (`provider: "auto"`).

The contract covers two distinct operational phases:
1. Candidate Resolution Phase: Parsing config containers, normalizing route entries, deduplicating composite identities, applying provider skip rules (failed provider, main provider, auto sentinel, unhealthy cache), enforcing the 64,000-token compression context window floor, and resolving the first viable client.
2. Request Execution Phase: Invocation inside sync and async `call_llm`, per-entry timeout evaluation, error classification, single-candidate execution limits, and failure propagation vs built-in discovery handoff.

### Primary Source Locations
- `hermes_cli/fallback_config.py`:
  - `get_fallback_chain`: lines 80-102 (merging `fallback_providers` and `fallback_model`)
  - `_iter_fallback_entries`: lines 43-69 (container parsing and field normalization)
  - `_entry_identity`: lines 72-77 (composite route identity for deduplication)
  - `_normalized_base_url`: lines 8-11 (base URL trailing slash trimming)
  - `resolve_entry_api_key`: lines 14-41 (inline key vs `key_env` vs `api_key_env`)
- `agent/auxiliary_client.py`:
  - `_try_main_fallback_chain`: lines 6363-6444 (traversal and resolution for aux tasks)
  - `_resolve_auto_route`: lines 6465-6658, specifically lines 6632-6635 (startup/initial client resolution)
  - `call_llm` (sync): lines 10296-10550 and 11116-11165 (request-time fallback dispatch)
  - `call_llm` (async): lines 11870-11930 (async request-time fallback dispatch)
  - `_task_minimum_context_length`: lines 6121-6139 (64K floor check for compression)
  - `_candidate_context_window`: lines 6142-6182 (context window resolution)
  - `_resolve_fallback_entry`: lines 6337-6360 (central router client resolution)
  - `_fallback_entry_api_key`: lines 6325-6335 (entry secret bridge)
  - `_fallback_entry_timeout`: lines 5558-5577 (per-entry timeout resolution)
  - `_fallback_destination`: lines 5635-5656 (route metadata resolution)
  - `_call_fallback_candidate_sync`: lines 5686-5860 (candidate invocation and error handling)
  - `_call_fallback_candidate_async`: lines 5862-5963 (async candidate invocation)
  - `_is_provider_unhealthy` / `_aux_unhealthy_until`: lines 4581-4595 (unhealthy provider cache)
  - `_read_main_provider`: lines 3561-3584 (main provider configuration lookup)
- `agent/model_metadata.py`:
  - `MINIMUM_CONTEXT_LENGTH`: line 465 (`64_000` tokens)
  - `get_model_context_length`: lines 3063-3096 (model context length resolver)

---

## 2. Configuration Schema, Containers, and Source Order

### Container Acceptance
Source: `hermes_cli/fallback_config.py:43-50, 80-102`
- Modern key: `fallback_providers`
- Legacy key: `fallback_model`
- Container types:
  - `list`: Iterated in source order.
  - `dict`: Coerced to a single-element list `[dict]`.
  - Non-container types (`str`, `int`, `float`, `bool`, `None`): Ignored, returning an empty list.
- Key precedence and concatenation:
  - `fallback_providers` is parsed first.
  - `fallback_model` is parsed second and appended.
  - Entries are retained in source order subject to deduplication.

### Entry Filtering and Normalization
Source: `hermes_cli/fallback_config.py:51-69`
- Non-dict items within a list are silently ignored and skipped.
- `provider`: The truthy value is stringified and stripped. Missing, null, falsey, or whitespace-only values drop the entry. Truthy non-string scalars such as `123` and `True` are retained as `"123"` and `"True"`.
- `model`: The truthy value is stringified and stripped under the same rule as `provider`.
- `base_url`: Optional strings are stripped and lose trailing slashes (`.rstrip("/")`). A non-string value is retained by the shallow config copy because normalization only overwrites non-empty strings. Later route resolution stringifies a truthy retained value and treats a falsey value as absent.
- Other attributes: Extra fields (`timeout`, `api_key`, `key_env`, `api_key_env`, `api_mode`, `transport`, `extra_body`, etc.) are preserved in a shallow dict copy.

### Composite Route Identity and Deduplication
Source: `hermes_cli/fallback_config.py:72-99`
- Route identity tuple:
  `identity = (provider.strip().lower(), model.strip().lower(), normalized_base_url.lower())`
- Deduplication rule:
  - Evaluated sequentially across `fallback_providers` and then `fallback_model`.
  - First occurrence of an identity is kept; subsequent occurrences are discarded.
  - Case-insensitive on provider, model, and normalized base URL.
  - Deduplication preserves source order. Earlier entries in `fallback_providers` take precedence over matching entries in `fallback_model`.
  - Distinct models on the same provider are distinct routes and are both retained.
  - Distinct base URLs on the same provider and model are distinct routes and are both retained.

---

## 3. Credential and Transport Resolution

### Credential Resolution Hierarchy
Source: `hermes_cli/fallback_config.py:14-41`, `agent/auxiliary_client.py:6325-6335`
For each fallback entry, API key resolution follows strict precedence:
1. Inline Secret: `entry.get("api_key")`. If present and non-empty after stripping whitespace, returned immediately.
2. Primary Env Pointer: `entry.get("key_env")`. If present and non-empty, resolved via `agent.secret_scope.get_secret(key_env)`.
3. Alias Env Pointer: `entry.get("api_key_env")`. If `key_env` is absent/empty and `api_key_env` is present, resolved via `agent.secret_scope.get_secret(api_key_env)`.
4. Fallthrough to Provider Standard: If neither produces a non-empty secret, returns `None`. `resolve_provider_client` falls back to the provider default credential resolution (environment variables, credential pools, auth store).

### Transport and API Mode Aliases
Source: `agent/auxiliary_client.py:6345, 5596-5633`
- Wire protocol format override accepts `api_mode` and `transport`.
- Precedence: `entry.get("api_mode")` takes precedence over `entry.get("transport")`.
- When resolved client is constructed, `_FallbackDestination` is attached to `client._hermes_fallback_destination`.
- When `api_mode` is omitted, `_complete_fallback_destination` checks if endpoint speaks Anthropic messages, else resolves via runtime provider catalog.

---

## 4. Skip Rules and Health Gating in `_try_main_fallback_chain`

### Skip Set Construction
Source: `agent/auxiliary_client.py:6388-6405`
```python
failed_norm = (failed_provider or "").strip().lower()
main_norm = (_read_main_provider() or "").strip().lower()
skip = {p for p in (failed_norm, main_norm, "auto") if p}
```
Candidates are evaluated against `skip`:
- `fb_norm = fb_provider.lower()`
- If `fb_norm in skip`: Candidate is skipped.

### Critical Behavioral Contracts: Provider-Level vs Model-Level Skipping
- Contrast with `_try_configured_fallback_chain`:
  `_try_configured_fallback_chain` receives `failed_model` and uses `should_skip_candidate(..., failure_scope)` to allow sibling models under the same provider on model-scoped failures (rate limits, timeouts).
- `_try_main_fallback_chain` is Provider-Wide Only:
  `_try_main_fallback_chain` takes only `failed_provider`. The skip set check `fb_norm in skip` checks ONLY the provider identifier.
  Therefore, ANY entry in `fallback_providers` sharing the failed provider or the main provider is skipped wholesale, regardless of whether it names a different sibling model.
- Why Main Provider is Skipped:
  In provider auto mode, Step 1 of auxiliary resolution already attempted the main provider and model. Reaching the fallback chain implies the main provider failed, is depleted, or has no available client. Retrying the main provider in this chain is considered redundant.
- Auto Provider Skipped:
  Entries with `provider: "auto"` in the fallback chain are rejected by `skip` because `"auto"` is a routing sentinel, not a physical backend.

### Unhealthy Cache Gating
Source: `agent/auxiliary_client.py:6406-6409`, `4581-4595`
- Before resolving a candidate, `_is_provider_unhealthy(fb_norm)` checks `_aux_unhealthy_until.get(fb_norm)`.
- If `time.time() < expires_at`, the provider is skipped with status `(unhealthy)`.
- If the expiration timestamp has passed, the entry is lazily evicted and the candidate is admitted.

---

## 5. Compression Context Window Filtering (64,000-Token Floor)

### Task Minimum Context Floor
Source: `agent/auxiliary_client.py:6121-6139, 6416-6429`, `agent/model_metadata.py:465`
- `_task_minimum_context_length(task)` returns:
  - `64_000` when `task == "compression"` (`MINIMUM_CONTEXT_LENGTH`).
  - `None` when `task` is any other string (`"vision"`, `"title_generation"`, `"session_search"`, etc.) or `None`.

### Candidate Screening Contract
- For `task == "compression"`, candidate context is probed via `_candidate_context_window(fb_provider, resolved_model or fb_model, ...)`.
- If `fb_ctx is not None and fb_ctx < 64_000`:
  Candidate is skipped: `tried.append(f"{label} (context too small: {fb_ctx}<{min_ctx})")`. The chain advances to the next candidate.
- If `fb_ctx is None`:
  Candidate passes through. Probe failures or unrecognized models are treated permissively to avoid blocking viable custom endpoints.
- If `fb_ctx >= 64_000`:
  Candidate passes screening.
- Non-compression tasks:
  Because `min_ctx is None`, no context screening is applied; small-context models (such as 8K models for vision or titling) are accepted.

---

## 6. Client Resolution Failures and Per-Entry Timeout Behavior

### Client Resolution Failures
Source: `agent/auxiliary_client.py:6410-6415`
- Candidate client resolution invokes `_resolve_fallback_entry(entry)`.
- If `resolve_provider_client` returns `(None, None)` or raises an exception, the candidate is recorded as failed and skipped.
- The loop continues to evaluate subsequent candidates in the chain.

### Per-Entry Timeout at Final Request: Runtime Reality vs Documented Intent
Source: `agent/auxiliary_client.py:5558-5577, 5720-5727, 6435, 11121, 11875`
A critical disparity exists between the documented design intent and current runtime behavior:
- Documented Design / Config Intent:
  Config entries under `fallback_providers` may specify a `timeout` field (e.g. `timeout: 45`). Comment in `_call_fallback_candidate_sync` states:
  "effective_timeout is the task-level deadline; a configured-chain candidate with its own timeout entry gets that instead..."
- Current Runtime Implementation:
  1. `_try_main_fallback_chain` returns:
     `return fb_client, resolved_model or fb_model, fb_provider` (line 6435).
     Notice that the third element is `fb_provider` (e.g. `"openrouter"`), NOT the label `fallback_providers[i](openrouter)`.
  2. In `call_llm`, `fb_label` is assigned `fb_provider`.
  3. In `_call_fallback_candidate_sync`, timeout is queried via:
     `fb_timeout = _fallback_entry_timeout(task, fb_label)` (line 5720).
  4. In `_fallback_entry_timeout(task, fb_label)` (lines 5525-5544):
     `m = re.match(r"fallback_chain\[(\d+)\]", fb_label)`
     The regex ONLY matches labels starting with `fallback_chain[` (minted by `_try_configured_fallback_chain`).
     It does NOT match bare provider names (`"openrouter"`), nor would it match `fallback_providers[i]`.
  5. Furthermore, `_fallback_chain_entry` only reads `auxiliary.<task>.fallback_chain`. It never inspects `fallback_providers` or `fallback_model`.
  6. Therefore, `_fallback_entry_timeout` ALWAYS returns `None` for main fallback chain candidates.
  7. Final Request Consequence:
     `effective_timeout` is NEVER overridden by a `timeout` field in `fallback_providers` or `fallback_model`. The final request executes with the ambient task-level timeout (which for compression is floored at 300.0s).

---

## 7. Execution Candidate Budget and Failure Propagation

### Exactly How Many Candidates May Execute?
Source: `agent/auxiliary_client.py:11116-11165, 11870-11930, 5686-5860`
The runtime execution budget for `_try_main_fallback_chain` in `call_llm` is bounded to at most ONE candidate:
1. Candidate Selection:
   `_try_main_fallback_chain` loops over the parsed chain and returns the FIRST valid, reachable, context-screened candidate. It returns a single client tuple.
2. Candidate Execution:
   `call_llm` calls `_call_fallback_candidate_sync` (or `_call_fallback_candidate_async`) with that single candidate.
3. Outcome Branches:
   - Branch A (Success): The candidate returns a valid response. Done. Total candidates executed: 1.
   - Branch B (Non-Auth Error: 408 Timeout, 429 Rate Limit, 500/502/503 Server Error, Connection Error, 400 Bad Request):
     Inside `_call_fallback_candidate_sync`:
     `if not _is_auth_error(fb_err): raise` (line 5777 / 5848).
     The exception is re-raised immediately.
     There is NO enclosing try/except block around this call in `call_llm`.
     The error propagates immediately out of `call_llm`.
     Total fallback candidates executed: 1.
     Subsequent candidates in `fallback_providers` executed: 0.
     Built-in discovery candidates executed: 0.
   - Branch C (Auth Error: 401 Unauthorized):
     Inside `_call_fallback_candidate_sync`:
     Credential refresh is attempted. If refresh succeeds, the candidate is retried once. If refresh fails or retried request still returns 401:
     The provider is marked unhealthy in `_aux_unhealthy_until`, and `_call_fallback_candidate_sync` returns `None`.
     Inside `call_llm`:
     `fb_client, fb_model, fb_label = _try_payment_fallback(resolved_provider, task, reason="stale fallback credential")`
     `call_llm` falls directly to built-in discovery (`_try_payment_fallback` -> OpenRouter -> Nous -> Custom -> Codex -> catalog).
     It does NOT resume `_try_main_fallback_chain`. It does NOT try candidate #2 from `fallback_providers`.
     Total fallback candidates executed: 1.
     Subsequent `fallback_providers` candidates executed: 0.
     Built-in discovery candidates executed: at most 1.
   - Branch D (Chain Exhaustion during Resolution):
     If no candidate in `fallback_providers` resolves or passes screening, `_try_main_fallback_chain` returns `(None, None, "")`.
     `call_llm` falls to `_try_payment_fallback` (built-in discovery).
     Total fallback candidates executed: 0.

### Distinguishing Current Runtime Behavior from Comments or Desired Behavior
- Comment in `_try_main_fallback_chain` (lines 6370-6374):
  "provider: auto auxiliary tasks should respect the user's declared main fallback policy before dropping into Hermes' built-in discovery chain."
  Comment in `call_llm` (lines 11110-11114):
  "Fallback order: 1. User-configured fallback_chain... 2. For auto: top-level main fallback_providers... 3. For auto: built-in auxiliary discovery chain."
- Runtime Reality:
  - While `_try_main_fallback_chain` loops during *resolution* to find the first candidate, the *request execution* layer does not provide chain traversal.
  - If candidate #1 fails with any non-auth runtime error (rate limit, server 500, network drop), it immediately crashes the auxiliary call rather than attempting candidate #2 or falling to built-in discovery.
  - If candidate #1 fails with an auth error, it skips all remaining candidates in `fallback_providers` and drops directly to built-in auto-discovery.
  - Any `timeout` defined in a `fallback_providers` entry is ignored at request time, inheriting the 300.0s compression floor instead.

---

## 8. Verification and Oracle Goldens

### Generator Implementation
The oracle generator is located at:
`rust/tools/gen_compression_main_fallback_chain_goldens.py`
and generates golden output at:
`rust/tools/compression-main-fallback-chain-goldens.json`

### Execution Commands
```bash
# Generate goldens
.venv/bin/python rust/tools/gen_compression_main_fallback_chain_goldens.py

# Verify parity with --check
.venv/bin/python rust/tools/gen_compression_main_fallback_chain_goldens.py --check
```

### Case Counts by Section
Total deterministic test cases: 76
- Section 1 (`container_and_entry_parsing`): 27 cases
  - Exercises `get_fallback_chain` and `_iter_fallback_entries` across list, dict, scalar rejection, null rejection, field filtering, whitespace trimming, base URL normalization, and extra field preservation.
- Section 2 (`chain_deduplication_and_identity`): 7 cases
  - Exercises composite route identity `_entry_identity`, case-insensitivity, trailing slash normalization, modern vs legacy key deduplication, and source order preservation.
- Section 3 (`credential_and_transport_resolution`): 11 cases
  - Exercises inline `api_key` vs `key_env` vs `api_key_env` precedence, secret scope lookups, `api_mode` vs `transport` aliases, and `_FallbackDestination` attachment.
- Section 4 (`provider_skipping_and_health_rules`): 8 cases
  - Exercises `failed_provider` skipping, `main_provider` skipping, `auto` provider skipping, provider-wide sibling model rejection, and `_aux_unhealthy_until` TTL gating.
- Section 5 (`compression_context_window_filtering_64k`): 10 cases
  - Exercises `_task_minimum_context_length` (64K for compression, `None` for others), 8K/32K rejection, 64K boundary acceptance, 128K acceptance, unknown context pass-through, and permissive non-compression behavior.
- Section 6 (`client_resolution_and_timeout_behavior`): 8 cases
  - Exercises resolver `None` and exception advancement, `_fallback_entry_timeout` label mismatch (`openrouter` and `fallback_providers[i]`), contrast with `fallback_chain[i]`, and final request timeout retention (300.0s).
- Section 7 (`candidate_execution_budget_and_failure_propagation`): 5 cases
  - Exercises candidate execution success (1 run), non-auth 429 re-raise (0 subsequent, 0 discovery), non-auth 500 re-raise, non-auth timeout re-raise, and 401 unrefreshable auth drop to built-in discovery.

---

## 9. Parity Requirements for Bounded Native Rust Checkpoint

For a native Rust implementation in `hermes-gateway` to maintain parity with this contract:

1. Configuration Container and Normalization:
   - Accept either a single object or an array of objects under `fallback_providers` and `fallback_model`. Reject non-object scalars and nulls.
   - Drop entries missing either `provider` or `model` (after trimming whitespace).
   - Normalize `base_url` by trimming leading/trailing whitespace and stripping trailing slashes.
2. Route Deduplication:
   - Compute route identity as lowercase `(provider, model, normalized_base_url)`.
   - Iterate `fallback_providers` in order, then `fallback_model` in order. Skip any entry whose identity was already seen.
3. Candidate Resolution Phase:
   - Build skip set: lowercase `failed_provider`, lowercase `main_provider`, and `"auto"`.
   - Skip any candidate whose lowercase provider is in the skip set. Do NOT attempt sibling models under the same provider.
   - Skip any candidate whose provider is currently marked unhealthy in the cache.
   - For compression summaries, check model context window against the 64,000-token floor. If known and < 64,000, skip candidate and continue. If unknown or >= 64,000, accept.
   - Select the first candidate that passes all filters and has available credentials.
4. Per-Entry Timeout:
   - Do NOT apply entry-level `timeout` from `fallback_providers` to auxiliary requests. Maintain the task-level timeout (floored at 300.0s for compression).
5. Execution Budget and Failure Semantics:
   - Attempt at most ONE candidate from the main fallback chain.
   - If that candidate fails with a non-auth error (timeout, connection, 429, 5xx), re-raise immediately; do not attempt subsequent fallback candidates and do not fall to built-in discovery.
   - If that candidate fails with an unrefreshable auth error (401), mark provider unhealthy and fall directly to built-in discovery.
