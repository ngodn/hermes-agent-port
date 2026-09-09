# Auxiliary Compression Fallback Chain: Adversarial Production and Parity Review

## 1. Executive Summary and Committability Verdict

This report presents an adversarial production and parity review of the uncommitted auxiliary compression fallback-chain implementation across the following files in `hermes-gateway`:
- [compression_auxiliary.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs)
- [main.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)
- [native_agent.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs)

The implementation introduces typed configuration parsing, candidate resolution, and failover routing for `auxiliary.compression.fallback_chain`. Several architectural primitives are implemented with high precision:
- Route order separation correctly distinguishes explicit auxiliary mode (`[Primary Aux, Fallback 0, Fallback 1, ..., Main Agent Model]`) from auto mode (`[Main Agent Model, Fallback 0, Fallback 1, ...]`).
- Strict recursion prevention is guaranteed by clearing `compression_routes` on auxiliary request clients.
- Usage accounting isolates auxiliary token usage in `UsageBucket::Auxiliary` and persists it to `session_db`.
- Independent per-entry timeouts strictly adhere to Python's `_coerce_positive_timeout` contract without imposing the 300.0s task floor.

However, the review identified critical request control leakage, behavioral parity divergence in runtime fallback traversal, contract schema omissions, and substantial test gaps.

### Committability Verdict: NOT SAFE TO COMMIT IN CURRENT STATE

The implementation is **NOT SAFE TO COMMIT** until the following conditions are met:
1. **Resolve Request Control Leakage (Blocking)**: Prevent uncertified fallback candidates from inheriting task-level `reasoning_config` (`{"enabled": false}`) and vendor-specific task `extra_body` in [main.rs:420-434](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L420-L434) and [main.rs:625-639](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L625-L639).
2. **Reconcile Runtime Multi-Candidate Traversal (Important)**: Resolve the fundamental parity divergence where Rust attempts every configured fallback sequentially at runtime ([native_agent.rs:1284-1323](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1284-L1323)), whereas authoritative Python only attempts at most one fallback candidate before invoking the main-model safety net. If multi-candidate failover is retained, bound cumulative latency to avoid severe session stalls.
3. **Restore Missing Schema Field `extra_body` (Important)**: Add `pub extra_body: serde_json::Map<String, serde_json::Value>` to `FallbackChainEntry` in [compression_auxiliary.rs:37-49](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L37-L49) and parse it in `FallbackChainEntry::from_value` to honor the schema specification and golden test fixtures.
4. **Close Golden Corpus Test Gaps (Important)**: Validate `corpus["timeout_coercion"]`, `corpus["api_mode_normalization"]`, and `corpus["credential_resolution"]` against [rust/tools/compression-fallback-chain-goldens.json](file:///home/eins0fx/development/hermes-agent-port/rust/tools/compression-fallback-chain-goldens.json) in cargo tests.

---

## 2. Ranked Findings by Severity

### 2.1 Blocking Findings

#### Finding B-1: Request Control Leakage into Fallback Routes via Default Fallback to Task Controls
- **Severity**: Blocking
- **Affected Files and Lines**:
  - [main.rs:420-427](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L420-L427) (`CompressionRouteConfig::reasoning_config`)
  - [main.rs:429-434](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L429-L434) (`CompressionRouteConfig::task_extra_body`)
  - [main.rs:625-639](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L625-L639) (`build_native_compression_client`)
- **Authoritative Python Reference**:
  - [agent/auxiliary_client.py:9020-9048](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L9020-L9048) (`_compression_fast_lane_controls`)
- **Detailed Description**:
  In `main.rs`, when building a fallback candidate client, `CompressionRouteConfig::reasoning_config` falls back to the task-level `task.reasoning_config` whenever the entry itself omits `reasoning_effort`:
  ```rust
  fn reasoning_config(&self) -> Option<serde_json::Value> {
      match self {
          Self::Primary(config) => config.reasoning_config.clone(),
          Self::Fallback { entry, task } => entry
              .reasoning_config
              .clone()
              .or_else(|| task.reasoning_config.clone()),
      }
  }
  ```
  During client initialization ([main.rs:631-633](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L631-L633)), this inherited value is inserted into the request payload's `extra_body`:
  ```rust
  if let Some(reasoning) = route.reasoning_config() {
      extra_body.entry("reasoning").or_insert(reasoning);
  }
  ```
  Similarly, `route.task_extra_body()` ([main.rs:429-434](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L429-L434)) unconditionally injects the primary auxiliary task's `extra_body` map into the fallback client.
- **Parity Divergence**:
  In authoritative Python (`agent/auxiliary_client.py:9020-9048`), fast-lane controls (`reasoning_effort: false`) are certified exclusively for an exact provider/model route. If the task declared fast-lane controls but the candidate route is uncertified, Python explicitly deletes the `"reasoning"` key from the request payload:
  ```python
  elif _compression_config_claims_fast_lane(leak_guard_config):
      body.pop("reasoning", None)
  ```
  Python never forwards the primary route's `reasoning: {"enabled": false}` to fallback candidates.
- **Production Failure Mode**:
  Suppose a user configures `auxiliary.compression` with `provider: deepseek`, `model: deepseek-chat`, and `reasoning_effort: false`, with a fallback entry pointing to an Anthropic model or a standard OpenAI endpoint. In Rust, the fallback client inherits `reasoning: {"enabled": false}` and sends it in the JSON body. Upstream providers and proxies that do not support OpenAI-style reasoning configurations reject the request with HTTP 400 Bad Request, causing the fallback attempt to fail immediately.

---

### 2.2 Important Findings

#### Finding I-1: Runtime Multi-Candidate Failover Diverges from Python Parity and Poses Severe Turn Latency Risk
- **Severity**: Important
- **Affected Files and Lines**:
  - [native_agent.rs:1284-1323](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1284-L1323) (`summarize_history_with_memory`)
- **Authoritative Python Reference**:
  - [agent/auxiliary_client.py:11116-11165](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L11116-L11165) (`call_llm`)
  - [agent/context_compressor.py:5641-5791](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L5641-L5791) (`_generate_summary`)
  - [agent/conversation_compression.py:1278-1339](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L1278-L1339) (`resolve_compression_fallback_route`)
- **Detailed Description**:
  In `native_agent.rs:1284-1323`, `summarize_history_with_memory` iterates through all routes in `self.compression_routes.fallbacks` in a runtime loop. If the primary route fails, candidate 0 is executed. If candidate 0 fails, candidate 1 is executed; if candidate 1 fails, candidate 2 is executed; only after every fallback fails is the main conversation model attempted.
- **Parity Divergence**:
  Trying more than one runtime fallback candidate does NOT match actual Python behavior:
  1. In `call_llm` (`agent/auxiliary_client.py:11117-11145`), `_try_configured_fallback_chain` scans the fallback list at resolution time to select at most ONE viable candidate.
  2. `_call_fallback_candidate_sync` executes that single candidate.
  3. If that candidate fails at runtime with a non-auth error (timeout, connection error, HTTP 429, HTTP 500, invalid JSON), the error raises immediately out of `call_llm`. `call_llm` does NOT loop back to candidate 1 or candidate 2.
  4. In `ContextCompressor._generate_summary` (`agent/context_compressor.py:5747-5791`), catching this exception triggers `_fallback_to_main_for_compression`, retrying once on the MAIN model. It never resumes `fallback_chain`.
  5. In the stall-detection recovery path (`conversation_compression.py:1288-1292`), `resolve_compression_fallback_route` selects only the first structurally complete entry for a single bounded retry.
- **Production Failure Mode**:
  If a user configures 3 fallback candidates, each with a 300s timeout (or default unconfigured timeout):
  Under Rust's implementation, a network partition or provider outage triggers Primary (300s) + Fallback 0 (300s) + Fallback 1 (300s) + Fallback 2 (300s) + Main (300s). The user turn is blocked for up to 1,500 seconds (25 minutes). In Python, an auxiliary request that fails times out at most once on the primary and once on a single fallback before falling back to main or entering cooldown (Python issue #62452).

#### Finding I-2: Schema Discrepancy: Omission of `extra_body` from `FallbackChainEntry`
- **Severity**: Important
- **Affected Files and Lines**:
  - [compression_auxiliary.rs:37-49](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L37-L49) (`FallbackChainEntry`)
  - [compression_auxiliary.rs:53-83](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L53-L83) (`FallbackChainEntry::from_value`)
  - [rust/analysis/compression-fallback-chain-config-claude.md:75-80](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/compression-fallback-chain-config-claude.md#L75-L80)
  - [rust/tools/compression-fallback-chain-goldens.json:13](file:///home/eins0fx/development/hermes-agent-port/rust/tools/compression-fallback-chain-goldens.json#L13)
- **Detailed Description**:
  The AGY contract specification (`compression-fallback-chain-contract-agy.md:237`), the Claude implementation report (`compression-fallback-chain-config-claude.md:75`), and the golden fixtures (`compression-fallback-chain-goldens.json:13`) all specify that `FallbackChainEntry` must carry `pub extra_body: serde_json::Map<String, serde_json::Value>`.
  Claude's report specifically claimed that `extra_body` was implemented on `FallbackChainEntry`.
  However, in `compression_auxiliary.rs:37-49`, `extra_body` is completely absent from `FallbackChainEntry`. `FallbackChainEntry::from_value` does not parse `extra_body` from the entry JSON.
- **Production Impact**:
  Any candidate-specific vendor parameters declared under `auxiliary.compression.fallback_chain[].extra_body` in `config.yaml` are dropped during deserialization. Furthermore, [main.rs:630](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L630) erroneously substitutes the task-level `extra_body` instead.

#### Finding I-3: Complete Absence of Failure-Scoped Candidate Skipping
- **Severity**: Important
- **Affected Files and Lines**:
  - [main.rs:835-862](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L835-L862)
  - [native_agent.rs:1284-1323](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1284-L1323)
- **Authoritative Python Reference**:
  - [agent/backend_identity.py:35-120](file:///home/eins0fx/development/hermes-agent-port/agent/backend_identity.py#L35-L120) (`should_skip_candidate`)
  - [agent/auxiliary_client.py:6238-6264](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6238-L6264) (`_try_configured_fallback_chain`)
- **Detailed Description**:
  In Python, candidate evaluation uses `should_skip_candidate`:
  - `FailureScope.MODEL`: skips exact matching deployments (`provider`, `model`, `base_url`), but permits sibling models under the same provider.
  - `FailureScope.CREDENTIAL`: skips all candidate entries under the failed provider when an authentication (401) or billing (402) error occurs, because all models sharing that account are broken.
  In Rust, there is no implementation of failure-scoped skipping. All fallback entries that build at startup are placed into `fallbacks`.
- **Production Impact**:
  If a primary OpenRouter route fails with HTTP 401 (invalid API key), Rust will still attempt any OpenRouter candidates present in `fallback_chain`, repeating identical failing requests and wasting time before reaching another provider or the main model.

#### Finding I-4: Absence of 64,000-Token Minimum Context Window Floor for Candidates
- **Severity**: Important
- **Affected Files and Lines**:
  - [main.rs:835-862](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L835-L862)
  - [automatic_compression.rs:44](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/automatic_compression.rs#L44)
- **Authoritative Python Reference**:
  - [agent/auxiliary_client.py:6274-6287](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L6274-L6287) (`_candidate_context_window`)
- **Detailed Description**:
  In Python, compression transcripts are large, so candidates with known context windows below 64,000 tokens are skipped during chain resolution (`_task_minimum_context_length("compression") == 64_000`).
  In Rust, although `automatic_compression.rs:44` defines `MINIMUM_CONTEXT_LENGTH: u64 = 64_000`, `build_native_compression_client` does not inspect profile or catalog context lengths for fallback candidates.
- **Production Impact**:
  If a user configures an 8k or 16k context utility model in `fallback_chain`, Rust attempts the summary call on that model, failing with prompt overflow when summarizing longer transcripts.

#### Finding I-5: Golden Corpus Test Coverage Gaps (Sections 2 through 6 Untested)
- **Severity**: Important
- **Affected Files and Lines**:
  - [compression_auxiliary.rs:487-586](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L487-L586) (`fallback_entries_match_source_executed_python_corpus`)
  - [rust/tools/compression-fallback-chain-goldens.json](file:///home/eins0fx/development/hermes-agent-port/rust/tools/compression-fallback-chain-goldens.json)
- **Detailed Description**:
  The test `fallback_entries_match_source_executed_python_corpus` only iterates over `corpus["entry_acceptance_and_coercion"]`. The remaining 5 sections in `compression-fallback-chain-goldens.json` are completely unverified by cargo tests:
  - `timeout_coercion` (22 test cases): only ad-hoc test in [compression_auxiliary.rs:434](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L434)
  - `api_mode_normalization` (20 test cases): only 2 cases in [compression_auxiliary.rs:458](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L458)
  - `credential_resolution` (12 test cases): only 3 cases in [compression_auxiliary.rs:470](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L470)
  - `failed_route_skipping` (17 test cases): completely untested
  - `chain_traversal_and_exhaustion` (9 test cases): completely untested
  Furthermore, `fallback_entries_match_source_executed_python_corpus` explicitly skips asserting `extra_body` because the field was omitted from `FallbackChainEntry`.

---

### 2.3 Optional / Minor Findings

#### Finding O-1: Unredacted Error Logging in Startup Warnings
- **Severity**: Optional / Minor
- **Affected Files and Lines**:
  - [main.rs:829-832](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L829-L832)
  - [main.rs:855-860](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L855-L860)
- **Detailed Description**:
  In `main.rs`, startup route failure warnings log `%error` directly without calling `crate::compression_redact::redact(&error.to_string())`. In contrast, runtime summary errors in `native_agent.rs:1310` explicitly redact error strings before logging.
- **Recommendation**:
  Wrap `%error` with `crate::compression_redact::redact(&error.to_string())` for uniform defensive secret hygiene.

#### Finding O-2: Speculative and Fragile Fallback Fast-Lane Output Cap
- **Severity**: Optional / Minor
- **Affected Files and Lines**:
  - [compression_auxiliary.rs:111-126](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L111-L126) (`certified_output_cap`)
  - [main.rs:613-616](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L613-L616)
- **Detailed Description**:
  `certified_output_cap` applies a hard output token cap to fallback clients if certified as non-reasoning. However, in full summary requests, hard token caps frequently result in `finish_reason == "length"`, causing summaries to be rejected as partial. While technically functional, capping fallback summaries should be used cautiously.

---

## 3. Comprehensive Domain Analysis Across Required Audit Dimensions

### 3.1 Route Order: Explicit vs Auto Mode
The route ordering logic in [main.rs:863-869](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L863-L869) and [native_agent.rs:1254-1280](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1254-L1280) correctly differentiates explicit auxiliary mode from auto mode:
- In explicit auxiliary mode (`!main_first`), the planned route sequence is `[Primary Aux, Fallback 0, Fallback 1, ..., Main Agent Model]`.
- In auto mode (`main_first`), the planned route sequence is `[Main Agent Model, Fallback 0, Fallback 1, ...]`.
- `main_first` is derived from `!compression_policy.needs_separate_client`. Pure auto mode sets `needs_separate_client = false`, which correctly routes the main model first. Auto mode with an explicit auxiliary model sets `needs_separate_client = true`, which correctly routes the explicit auxiliary model first.

### 3.2 Primary-Unavailable Behavior
In [main.rs:815-834](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L815-L834), when the primary auxiliary route cannot be constructed at startup (due to invalid credentials, missing base URL, or unsupported API mode), `build_native_compression_client` returns an error or `None`.
- `primary_compression` remains `None`.
- The gateway logs a warning and proceeds to build `compression_fallbacks`.
- At runtime in [native_agent.rs:1256-1262](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1256-L1262), `self.compression_routes.primary` is skipped, and execution starts directly with the first configured fallback.
- If both the primary auxiliary and all fallback routes fail to build, `compression_routes` defaults to empty, and the runtime falls back cleanly to `PlannedRoute::Main`.
This matches Python's `_try_configured_fallback_for_unavailable_client` (`agent/auxiliary_client.py:6303-6323`).

### 3.3 Invalid Entries
The deserialization and validation pipeline rejects invalid entries cleanly:
- Non-list containers for `fallback_chain` evaluate to an empty list ([compression_auxiliary.rs:194](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L194)).
- Non-object elements (strings, numbers, booleans, lists) return `None` and are skipped ([compression_auxiliary.rs:53](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L53)).
- Missing or whitespace-only providers return `None` and are skipped ([compression_auxiliary.rs:56-59](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L56-L59)).
- Missing models parse successfully into `FallbackChainEntry` (`model: None`), but are skipped during client construction with a debug log ([main.rs:473-475](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L473-L475)).
- Invalid timeouts (strings, booleans, non-positive values) reject to `None` ([compression_auxiliary.rs:341-351](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L341-L351)), falling back to the task-level timeout.
- Unsupported API modes (`anthropic_messages`, `codex_responses`, `bedrock_converse`) error out at build time ([main.rs:576-579](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L576-L579)) and are skipped with a warning log without crashing the gateway.

### 3.4 Provider, Model, Base URL, and Credential Resolution
The hierarchy in [main.rs:480-604](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L480-L604) correctly mirrors established resolver patterns:
- Model: Candidate `model` -> Profile `default_aux_model` -> Named custom provider -> `main_model`. Literal `"auto"` is preserved on fallback entries.
- Base URL: Candidate `base_url` -> Named custom provider -> Profile endpoint/env -> Main model base URL (if same provider).
- Credentials: Candidate inline `api_key` -> `key_env` / `api_key_env` -> Named custom provider -> Main model key (if same provider) -> Profile key -> Environment URL lookup -> Local probe bypass (`"no-key-required"`).
- `FallbackChainEntry::direct_api_key` ([compression_auxiliary.rs:131-158](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L131-L158)) trims Python whitespace and prioritizes `key_env` over `api_key_env`.

### 3.5 Per-Entry Timeouts
The helper `entry_timeout_seconds` in [compression_auxiliary.rs:341-351](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L341-L351) strictly implements Python's `_coerce_positive_timeout`:
- Rejects booleans and non-numeric types.
- Rejects string numbers (e.g. `"45"`).
- Rejects zero and negative numbers.
- Does not apply the 300.0s compression timeout floor.
- In [main.rs:413-418](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L413-L418), `route.timeout()` yields the entry's unfloored timeout when declared, or falls back to `task.timeout` (which has the 300s floor) when omitted.

### 3.6 Request Control Leakage
As detailed in Finding B-1, [main.rs:420-434](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L420-L434) and [main.rs:625-639](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L625-L639) leak task-level `reasoning_config` and `extra_body` into fallback candidates. Conversely, `tools`, `tool_choice`, `parallel_tool_calls`, `max_tokens`, and `temperature` are cleanly stripped and re-applied in [native_agent.rs:938-953](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L938-L953).

### 3.7 Usage Accounting
Usage accounting is fully verified:
- In [native_agent.rs:1212-1223](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1212-L1223) (`summarize_history_on`), each route attempt runs on an isolated client clone with `usage_bucket = UsageBucket::Auxiliary` and calls `begin_auxiliary_usage()`.
- Provider response usage is captured in `auxiliary_summary_request` ([native_agent.rs:976-980](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L976-L980)).
- Successful and failed summaries commit auxiliary usage to `database` under the active `session_id`.
- Main model turn usage is never polluted by auxiliary compression attempts.

### 3.8 Main-Model Safety Fallback
The main model safety fallback is guaranteed:
- In explicit auxiliary mode, `PlannedRoute::Main` is appended as the terminal element of `routes` ([native_agent.rs:1270](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1270)).
- In auto mode, `PlannedRoute::Main` is the leading element of `routes` ([native_agent.rs:1272](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1272)).
- If all auxiliary routes return unusable summaries or fail, the main model executes with standard prompt and temperature settings.

### 3.9 Recursion Prevention
Recursion is strictly prevented:
- `summarize_history_on` ([native_agent.rs:1211](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1211)) sets `summary_client.compression_routes = Default::default()`.
- Inside `summarize_history_on`, `full_summary_request` calls `auxiliary_summary_request`, which performs direct HTTP requests without calling `summarize_history_with_memory`.
- No nested fallback chains can be triggered.

### 3.10 Boundedness
The route list is compiled once into a linear `Vec<PlannedRoute>` of length at most `fallbacks.len() + 2`. No dynamic loops or retries are executed per candidate. Total execution time is strictly bounded by the sum of individual route timeouts.

### 3.11 Secret Redaction
- Runtime error logging in [native_agent.rs:1310](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1310) scrubs errors with `crate::compression_redact::redact`.
- Summary text is redacted before storage in [native_agent.rs:1005](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1005) and [native_agent.rs:1587](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1587).
- Startup warnings in [main.rs:829](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L829) and [main.rs:855](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L855) should also redact `%error` (Finding O-1).

### 3.12 Prompt-Cache Stability
The prompt string is built once in [native_agent.rs:1244](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1244) via `compression_prompt::build_with_memory`. The exact same prompt reference is reused across all fallback candidate attempts. No transient timestamps or attempt-specific tokens are inserted, preserving upstream prefix-caching benefits across candidates sharing provider infrastructure.

### 3.13 Runtime Multi-Candidate Traversal Parity
As established in Finding I-1, trying more than one runtime fallback candidate does NOT match authoritative Python behavior. Python attempts at most one fallback candidate per failure before either falling back to the main agent model or aborting to prevent multi-minute session hangs.

### 3.14 Compile and Test Gaps
The current test suite compiles and passes 1,803 tests (`cargo test --manifest-path rust/Cargo.toml -p hermes-gateway`). However, as detailed in Finding I-5, sections 2 through 6 of `rust/tools/compression-fallback-chain-goldens.json` are not wired into automated tests.

### 3.15 Speculative or Dead Interface
- `FallbackChainEntry` omitted `extra_body` (Finding I-2), leaving candidate-specific extra body declarations unsupported.
- `certified_output_cap` on fallback candidates (Finding O-2) is speculative for full summary requests, where hard token limits frequently cause `finish_reason == "length"` aborts.

---

## 4. Production Commit Readiness Gate

### Commit Readiness Checklist

Before committing this branch to `main`, the following changes must be completed and verified:

- [ ] **Fix Request Control Leakage (Blocking / Finding B-1)**:
  - In [main.rs:420-427](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L420-L427), remove the `or_else(|| task.reasoning_config.clone())` fallback for `Self::Fallback`. An entry that omits `reasoning_effort` must evaluate to `None`.
  - In [main.rs:631-633](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L631-L633), only inject `"reasoning"` into `extra_body` when the fallback entry itself explicitly declared it or is certified as a fast-lane route.
  - In [main.rs:429-434](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L429-L434), do not copy task-level `extra_body` into fallback candidates.
- [ ] **Reconcile Runtime Traversal with Parity and Latency Bounds (Important / Finding I-1)**:
  - In [native_agent.rs:1254-1323](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1254-L1323), align runtime fallback execution with Python's single-candidate execution contract (try at most one fallback candidate before `PlannedRoute::Main`); OR
  - If multi-candidate failover is retained as an explicit design choice, enforce an aggregate session failover timeout (e.g. max 300s across all fallbacks) to prevent multi-minute session stalls.
- [ ] **Add `extra_body` to `FallbackChainEntry` (Important / Finding I-2)**:
  - Add `pub extra_body: serde_json::Map<String, serde_json::Value>` to `FallbackChainEntry` in [compression_auxiliary.rs:37-49](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L37-L49).
  - Parse `extra_body` in `FallbackChainEntry::from_value` ([compression_auxiliary.rs:53-83](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L53-L83)).
  - Forward candidate `extra_body` in `CompressionRouteConfig` and `main.rs:630`.
- [ ] **Wire Remaining Golden Corpus Sections into Tests (Important / Finding I-5)**:
  - In [compression_auxiliary.rs:487-586](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_auxiliary.rs#L487-L586), add test loops asserting `timeout_coercion`, `api_mode_normalization`, and `credential_resolution` against `compression-fallback-chain-goldens.json`.
  - Assert that parsed entries in `entry_acceptance_and_coercion` match expected `extra_body` mappings.
- [ ] **Sanitize Startup Warning Logs (Optional / Finding O-1)**:
  - In [main.rs:829](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L829) and [main.rs:855](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L855), wrap `%error` in `crate::compression_redact::redact(&error.to_string())`.
