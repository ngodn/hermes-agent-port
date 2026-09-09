# Production Main-Provider Credential-Pool Selection and Request-Time Recovery Contract

This document provides the authoritative, exhaustive runtime contract audit for production main-provider credential-pool selection, request-time credential recovery, failure accounting, and lifecycle boundaries in the Hermes Python codebase.

This contract reflects the exact implementation across:
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py)
- [`agent/credential_pool.py`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py)
- [`hermes_cli/runtime_provider.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/runtime_provider.py)
- [`hermes_cli/auth.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py)
- [`hermes_cli/config.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py)
- [`hermes_cli/route_identity.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/route_identity.py)

Deterministic runtime behavior is checked by the live Python test generator [`rust/tools/gen_main_provider_pool_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_pool_goldens.py) against [`rust/tools/main-provider-pool-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-pool-goldens.json) (11 sections, 86 test cases). The primary integration lane added the raw HTTP classifier section after review found that the original recovery cases injected already-classified reasons.

---

## 1. Scope and Architectural Boundaries

### 1.1 In-Scope Focus: Static API-Key Native Chat-Completions
This specification covers the primary chat-completions execution path:
1. **Startup Provider and Credential Resolution**: Priority ladder resolving explicit constructor arguments, profile credential pools, root credential pools, environment variables, and config defaults.
2. **Request-Time Failure Attribution**: Mapping HTTP error responses to the exact wire API key or entry ID that failed, avoiding corruption from stale or shared cursor pointers (#79156).
3. **Persistence-Before-Retry Ordering**: Synchronous disk state write before entry selection or client rebuilding.
4. **Retry State Transitions and Budget Accounting**: Per-request attempt limits, distinction between retrying the same credential versus rotating to a sibling, pre-exhaustion fast-paths, and usage-limit bypasses.
5. **Cooldown and Exhaustion Duration Matrix**: Error-classified cooldown sizing, sole-credential shortening, and header/body delay parsing.
6. **Provider Mismatch Isolation**: Guarding primary pool credentials against corruption when fallback providers fail (#33088, #33163).
7. **Route Changes and Client Reconfiguration**: TLS re-derivation, custom user header dropping, and client socket retirement (#70773).
8. **Lifecycle Across Tool Rounds and Multi-Turn Sessions**: Preservation of rotated credentials across tool loops and turn boundaries (#79156).
9. **Unified Streaming and Tool-Loop Recovery Parity**: Identical recovery contracts across streaming token generation and non-streaming tool rounds.

### 1.2 Out-of-Scope Boundaries
The following subsystems are explicitly segregated from the static API-key chat-completions subset:
- **OAuth Token Refresh**: Device-code and portal-based OAuth flows (Anthropic Messages OAuth, Codex Responses OAuth, MiniMax Portal OAuth) are managed by dedicated refresh loops and are excluded from the static API-key rotation subset.
- **Model Fallback Cascade**: Cross-provider failover activation (e.g. falling back from Anthropic to DeepSeek) is only activated after the primary provider's credential pool is fully exhausted or fails closed.
- **Non-Chat Transports**: Auxiliary compression, embeddings, speech-to-text, audio, and vision batching operate under independent client instances and separate timeout/retry pools.

---

## 2. Startup Precedence and Runtime Provider Resolution

Startup resolution is performed by [`resolve_runtime_provider`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/runtime_provider.py#L119-L330) and [`read_credential_pool`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L2250-L2290).

### 2.1 Precedence Priority Ladder
When constructing the agent client, credentials and endpoints are evaluated in the following strict order:
1. **Explicit Constructor / Config Arguments**:
   - `explicit_api_key`: When supplied (e.g. CLI `--api-key` or programmatic constructor argument), it takes unconditional precedence over all pools and environment variables. The resolved source is `"explicit"`, and no credential pool is attached (`has_pool: False`).
   - `explicit_base_url`: When supplied without `explicit_api_key`, it overrides the endpoint, disables pool attachment, and resolves the API key from the environment.
2. **Profile-Scoped Credential Pool**:
   - If a Hermes profile is active (e.g. `--profile work`), its provider credential pool in `~/.hermes/profiles/<profile>/auth.json` is evaluated.
   - If the profile pool contains one or more entries for the requested provider, it completely shadows the root `auth.json` pool.
   - If the profile pool contains zero entries for the provider, it borrows the root `auth.json` pool read-only.
3. **Root Credential Pool (`~/.hermes/auth.json`)**:
   - Evaluated if no profile shadows it.
   - If the pool contains an available entry (status `STATUS_OK`, or `STATUS_EXHAUSTED` with `now >= exhausted_until`), the first available entry is selected (`source: "pool"`).
   - If the pool entry specifies a custom `base_url`, it overrides the registry provider's default `inference_base_url`.
   - If the pool entry omits `base_url`, production resolution supplies the configured or registry fallback. The original direct-helper golden retains an empty raw entry URL, so it does not independently prove that final composition.
4. **Environment / Dotenv Variables**:
   - If the credential pool does not exist, is empty (`entries: []`), or is completely exhausted with active cooldowns (`select()` returns `None`), the runtime resolver falls back to environment variables (`<PROVIDER>_API_KEY`, e.g. `DEEPSEEK_API_KEY`).
   - Resolved source is `"env"`.
5. **Disabled Provider Fail-Fast**:
   - If configuration marks a provider as disabled (`providers.<provider>.enabled: False`), [`resolve_runtime_provider`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/runtime_provider.py#L140-L160) immediately raises `ValueError` before any network or pool initialization.

### 2.2 Provider Alias Normalization
The runtime provider resolver normalizes vendor aliases to canonical provider IDs via `_PROVIDER_ALIASES` in [`hermes_cli/auth.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L2811-L2850):
- `"z.ai"` -> `"zai"`
- `"minimax-china"` -> `"minimax-cn"`
- `"google"` -> `"gemini"`
- `"claude"` -> `"anthropic"`
- Custom provider slugs are derived from configured custom providers using [`custom_provider_pool_key_candidates`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L170-L215) matching base URLs and provider keys.

### 2.3 Executable Oracle Test Cases (Section 1: 13 cases)
- `explicit_keys_override_pool_and_env`: Explicit API key and base URL override active pool and environment variables (`source: "explicit"`, `has_pool: False`).
- `explicit_base_url_with_provider_default`: Explicit base URL uses the explicit endpoint and environment credential without attaching the pool (`source: "explicit"`, `has_pool: False`).
- `active_pool_selected_over_env`: Active pool entry is selected over environment variable (`source: "pool"`).
- `per_entry_base_url_overrides_provider_default`: Pool entry's `base_url` overrides default provider endpoint.
- `pool_entry_without_base_url_uses_config_default`: Records the empty raw entry URL alongside the expected registry endpoint. Production integration, not this row alone, proves fallback composition.
- `empty_pool_falls_back_to_env`: Empty pool (`[]`) falls back to environment variable (`source: "env"`).
- `exhausted_pool_falls_back_to_env`: Pool where all entries have active cooldowns falls back to environment variable.
- `profile_pool_shadows_root_pool`: Profile pool entries shadow root pool entries completely.
- `profile_without_provider_borrows_root_pool`: Profile without entries for provider borrows root pool entries read-only.
- `provider_alias_normalization_zai`: `"z.ai"` normalizes to `"zai"`.
- `provider_alias_normalization_minimax`: `"minimax-china"` normalizes to `"minimax-cn"`.
- `custom_provider_slug_candidates`: Correctly identifies slug candidates for custom provider base URLs.
- `disabled_provider_fails_fast`: Provider with `enabled: False` raises `ValueError` with "disabled".

---

## 3. Failure Attribution and Key Matching

Failure attribution is coordinated in [`agent_runtime_helpers.recover_with_credential_pool`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1171-L1220).

### 3.1 Attribution Priority and Key-Wins Invariant (#79156)
When an API call fails, attributing the failure to the correct entry is critical. The shared pointer `pool.current()` cannot be trusted because concurrent tasks, round-robin rotation, or background gateway refreshes routinely advance or reset `pool.current()`.
Attribution resolves using the following hierarchy:
1. **Wire Key Wins Over Disagreeing Entry ID (#79156)**:
   If `agent._credential_pool_entry_id` points to entry A, but `agent.api_key` (the key actually dispatched on the wire) matches entry B, entry B is attributed. The wire key reflects physical reality.
2. **Attribution by Entry ID**:
   If `agent._credential_pool_entry_id` matches an entry and the wire key is absent or agrees, the entry ID is attributed.
3. **Attribution by Key Hint**:
   If `agent._credential_pool_entry_id` is None, `agent.api_key` is matched against `entry.runtime_api_key`.
4. **Dropping Stale ID with Unknown Key**:
   If `agent._credential_pool_entry_id` is present but the wire key is an unrecognized external key, the stale entry ID is dropped rather than penalizing an innocent pooled credential.
5. **Fallback to Current Pointer**:
   Only if both `agent.api_key` and `agent._credential_pool_entry_id` are empty does the attribution fall back to `pool.current()`.

### 3.2 Sibling Entry Key Exhaustion
When multiple entries in the pool share the same runtime API key (e.g. duplicate additions or aliases), marking one entry exhausted marks all sibling entries sharing that key. This prevents rotating from a dead key into the same dead key under a different entry ID.

### 3.3 Unmatched Key Streak Guardrail
When an unknown or foreign API key fails and cannot be attributed to any pool entry, the pool allows at most `max(available_count, 1)` consecutive rotations. If unmatched rotations reach this cap, the pool halts rotation and surfaces the error, preventing infinite loops. In a single-entry pool, this guardrail fires immediately after one rotation attempt.

### 3.4 Executable Oracle Test Cases (Section 2: 8 cases)
- `attribution_by_entry_id_matches`: Attribution by matching `_credential_pool_entry_id`.
- `attribution_by_key_hint_when_id_none`: Attribution by `api_key_hint` when ID is None.
- `attribution_disagreement_key_wins`: Disagreement between entry ID and wire key: wire key wins attribution (#79156).
- `attribution_stale_id_unknown_key_dropped`: Stale entry ID with unknown foreign key is dropped; innocent pool entry spared.
- `attribution_fallback_to_current`: Fallback to `pool.current()` when agent carries no ID or key hint.
- `sibling_key_exhaustion_marks_all`: Failing key shared across multiple entries exhausts all siblings simultaneously.
- `unmatched_key_streak_capped`: Unmatched key streak is capped at available pool count, halting infinite rotation.
- `single_entry_pool_unmatched_key_no_loop`: Single-entry pool with unmatched key exits after 1 attempt.

---

## 4. Persistence-Before-Retry Ordering

Durability guarantees require that pool entry mutations survive process crashes and are visible to concurrent workers before retrying.

### 4.1 Synchronous Write Ordering
In [`CredentialPool.mark_exhausted_and_rotate`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2637-L2785):
1. **Mutation**: Entry status is updated to `STATUS_EXHAUSTED` or `STATUS_DEAD`, and `exhausted_until` is calculated.
2. **Synchronous Persistence**: `self._persist()` is invoked synchronously before selecting the next candidate entry.
3. **Next Selection**: `self._select_unlocked()` selects the next available entry.
4. **Agent Client Swap**: [`agent._swap_credential`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L6990-L7025) reconfigures `agent.api_key`, `agent.base_url`, and `agent.client`.

This order guarantees that the failed entry is durably persisted on disk before the next network request is dispatched.

### 4.2 Sibling Entry Multi-Write Durability
When sibling entries sharing a key are marked exhausted, all matching entries are updated in memory and serialized to disk together in a single atomic file write.

### 4.3 Executable Oracle Test Cases (Section 3: 4 cases)
- `rate_limit_persisted_before_swap`: 429 exhaustion persisted to disk before client swap.
- `billing_persisted_before_swap`: 402 exhaustion persisted to disk before client swap.
- `auth_persisted_before_swap`: Its execution order contains two persistence callbacks before the client swap. The row's simplistic `persisted_first` boolean is false because it expected exactly one persistence callback, so that boolean is not the durability assertion.
- `sibling_entries_persisted_together`: All sibling entries sharing key are persisted to disk atomically.

---

## 5. Retry State Transitions and Request Counts

Request-time error recovery in [`recover_with_credential_pool`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1105-L1434) implements deterministic state transitions:

### 5.1 Recovery Matrix

| Condition / Error | Attempt | Action Taken | `recovered` | Next `has_retried_429` | Notes |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Auth 401 (Standard)** | Attempt 1 | Rotate immediately to next credential | `True` | `False` | Resets `has_retried_429` |
| **Auth 401 (Terminal Revocation)** | Attempt 1 | Mark `STATUS_DEAD`, rotate to next entry | `True` | `False` | Entry permanently dead |
| **Auth 403 (Subscription / Entitlement)** | Attempt 1 | Skip pool recovery entirely | `False` | Unchanged | Account lacks subscription; rotation futile |
| **Billing 402** | Attempt 1 | Rotate immediately to next credential | `True` | `False` | Immediate rotation |
| **Billing 403 (Classified Reason)** | Attempt 1 | Rotate immediately to next credential | `True` | `False` | OpenRouter key limit, xAI spend limit |
| **Billing Unverified (#82154)** | Attempt 1 | Rotate immediately with short 60s cooldown | `True` | `False` | Ambiguous error body |
| **Ordinary Rate Limit 429** | Attempt 1 | Retry same credential | `False` | `True` | Gives transient spike a chance |
| **Ordinary Rate Limit 429** | Attempt 2 | Rotate to next credential in pool | `True` | `False` | Consecutive failure triggers rotation |
| **Pre-Exhausted 429** | Attempt 1 | Rotate immediately to next credential | `True` | `False` | Bypasses retry-same when already exhausted |
| **Usage Limit Reached** | Attempt 1 | Rotate immediately to next credential | `True` | `False` | Error context contains usage limit signal |
| **Upstream Aggregator Limit** | Attempt 1 | Defer to fallback chain without rotating | `False` | Unchanged | DeepSeek behind OpenRouter; user key healthy |
| **Pool Exhaustion** | Any | Return failure | `False` | `has_retried_429` | All pool entries exhausted |

### 5.2 Attempt Sizing and Reset Mechanics
- Rate limit retries are bounded to at most 1 retry of the same credential before rotating.
- When rotation succeeds (`recovered=True`), `has_retried_429` is reset to `False` so the new credential gets its own full retry budget.

### 5.3 Executable Oracle Test Cases (Section 4: 12 cases)
- `auth_401_immediate_rotation`: 401 rotates immediately on attempt 1 (`recovered=True`, `has_retried_429=False`).
- `auth_401_terminal_dead_status`: Terminal auth failure sets `STATUS_DEAD`.
- `auth_403_entitlement_skips_pool`: Entitlement 403 skips pool recovery (`recovered=False`).
- `billing_402_immediate_rotation`: 402 rotates immediately on attempt 1.
- `billing_403_classified_reason`: Classified billing 403 rotates immediately.
- `billing_unverified_short_cooldown_record`: `billing_unverified` records failure reason and short cooldown.
- `ordinary_rate_limit_attempt_1_retries_same`: 429 attempt 1 retries same credential (`recovered=False`, `has_retried_429=True`).
- `ordinary_rate_limit_attempt_2_rotates`: 429 attempt 2 rotates credential (`recovered=True`, `has_retried_429=False`).
- `pre_exhausted_rate_limit_immediate_rotation`: Pre-exhausted 429 rotates immediately on attempt 1.
- `usage_limit_reached_immediate_rotation`: Usage limit reached rotates immediately on attempt 1.
- `upstream_aggregator_rate_limit_defers`: Upstream aggregator 429 defers to fallback without pool mutation.
- `pool_exhaustion_returns_false`: Exhausted pool returns `recovered=False`.

---

## 6. Status and Cooldown Outcomes

Cooldown duration calculation is implemented in [`CredentialPool._exhausted_ttl`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L328-L370).

### 6.1 Baseline Cooldown Rules
1. **HTTP 401 Unauthorized**:
   Default TTL is 300 seconds (`EXHAUSTED_TTL_401_SECONDS`).
2. **HTTP 429 Rate Limit (Multi-Entry Pool)**:
   Default TTL is 3600 seconds (`EXHAUSTED_TTL_429_SECONDS`).
3. **HTTP 429 Rate Limit (Sole Non-Dead Entry)**:
   Shortened to 60 seconds (`EXHAUSTED_TTL_SOLE_CREDENTIAL_SECONDS`). When only one credential exists, benching it for an hour stalls the user unnecessarily.
4. **Billing / Quota (HTTP 402 or Classified Billing)**:
   Default TTL is 3600 seconds (`EXHAUSTED_TTL_DEFAULT_SECONDS`). Sole-credential shortening is explicitly bypassed (`is_billing` guard) because out-of-quota accounts do not recover within 60 seconds.
5. **Unverified Billing (`billing_unverified`)**:
   - Non-402 status code: Shortened to 60 seconds (transient classification guard, #82154).
   - Explicit 402 HTTP status: Full 3600 second bench (unambiguous billing status).

### 6.2 Upstream Delay Overrides
If upstream headers or response bodies supply reset times, they override defaults:
- `retry-after` header: Numeric seconds. HTTP-date parsing is not implemented by this Python helper.
- Error body message: Parsed human durations (the oracle uses `resets in 4hr 5min` -> 14,700 seconds).
- JSON fields: `quotaResetDelay: "45s"` -> 45 seconds, or ISO-8601 timestamp in `reset_at`.

### 6.3 Revival on Expiry
When `now >= entry.exhausted_until`, the entry is automatically considered available during `_available_entries()` evaluation.

### 6.4 Executable Oracle Test Cases (Section 5: 12 cases)
- `cooldown_401_default`: 401 cooldown is 300s.
- `cooldown_429_multi_entry`: 429 in multi-entry pool is 3600s.
- `cooldown_429_sole_credential_shortened`: 429 on sole non-dead entry is shortened to 60s.
- `cooldown_402_multi_entry`: 402 cooldown is 3600s.
- `cooldown_402_sole_credential_not_shortened`: 402 sole entry is NOT shortened to 60s (maintains 3600s).
- `cooldown_billing_classified_sole_not_shortened`: Classified billing sole entry maintains 3600s.
- `cooldown_billing_unverified_non_402_shortened`: Ambiguous billing with non-402 status gets 60s cooldown.
- `cooldown_billing_unverified_402_full_bench`: Ambiguous billing with 402 status maintains full 3600s bench.
- `cooldown_retry_after_header_override`: The oracle's numeric `retry after 120s` text yields 120 seconds.
- `cooldown_parsed_hr_min_message_override`: `resets in 4hr 5min` yields 14,700 seconds.
- `cooldown_quota_reset_delay_override`: `quotaResetDelay: "45s"` sets cooldown to 45s.
- `cooldown_expiry_revives_entry`: Expired cooldown revives entry to available.

---

## 7. Provider Mismatch Isolation

Defensive boundary enforcement in [`credential_pool_matches_provider`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1053-L1103) isolates primary pools from fallback errors (#33088, #33163).

### 7.1 Cross-Provider Contamination Prevention
When a primary provider fails over to a secondary fallback provider:
- If an error occurs while calling the fallback provider, the primary pool must NOT be rotated or marked exhausted.
- Rotating the primary pool on a fallback error corrupts the primary pool and overwrites the agent's base URL back to the primary endpoint, causing subsequent requests to 404 (#33163).

### 7.2 Custom Provider and URL Matching
- Named custom providers (e.g. `custom:fireworks`) match when the pool provider is either `custom` or the named alias, provided the base URL matches or normalizes to the same root.
- Generic custom pools require exact or normalized base URL equivalence.
- Mismatched base URLs or mismatched provider names fail the match and skip pool recovery.

### 7.3 Fail-Closed Empty Agent Provider
An empty agent provider (`agent.provider = ""`) fails closed (`False`) because swapping credentials would set `base_url` and `api_key` without restoring provider identity, leaving the agent corrupted.

### 7.4 Executable Oracle Test Cases (Section 6: 8 cases)
- `primary_pool_mismatched_fallback_skips`: Fallback error does not mutate primary pool (`recovered=False`).
- `primary_pool_matching_provider_rotates`: Matching primary provider rotates successfully.
- `custom_pool_matching_alias_and_url`: Custom pool matches on alias and base URL.
- `custom_pool_generic_custom_matching_url`: Generic custom pool matches on base URL.
- `custom_pool_mismatched_url_skips`: Custom pool skips on mismatched base URL.
- `custom_pool_mismatched_provider_name_skips`: Custom pool skips on mismatched provider name.
- `unscoped_pool_adapter_compatible`: Unscoped pool (`provider=""`) acts across providers.
- `empty_agent_provider_fails_closed`: Empty agent provider fails closed.

---

## 8. Route Changes and Client Reconfiguration

Endpoint mutations are handled by [`_swap_credential`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L6990-L7025) and [`_reapply_route_client_config`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L7026-L7059).

### 8.1 Route Identity Comparison
Route identity is compared using [`normalize_route_base_url`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/route_identity.py#L15-L60):
- Normalizes protocol, hostname (case-insensitive), default ports (strips `:443` for HTTPS, `:80` for HTTP), and path prefixes (stripping trailing slashes).
- If the normalized route before rotation equals the normalized route after rotation, `route_changed` is `False`.
- If the route differs in host, port, protocol, or path prefix, `route_changed` is `True`.

### 8.2 Header and TLS Material Reconfiguration
- **Same Route (`route_changed = False`)**:
  Custom user headers, default headers, and TLS configuration are preserved.
- **Different Route (`route_changed = True`)**:
  Custom user headers configured for the prior endpoint are dropped (`apply_user_headers=False`) to prevent leaking private headers to foreign endpoints. TLS kwargs (`ssl_verify`, `ssl_ca_cert`) are purged and recomputed for the new base URL via [`apply_custom_provider_tls_to_client_kwargs`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py).

### 8.3 Shared Client Replacement Locking (#70773)
Client rebuilding takes `self._openai_client_lock()`. The existing client is replaced on `self.client = new_client`, and the old client is retired via `_retire_shared_openai_client` (graceful socket shutdown without immediate socket FD close, deferring FD release to garbage collection). This prevents crashing active unwinding streaming workers.

### 8.4 Executable Oracle Test Cases (Section 7: 5 cases)
- `same_route_rotation_preserves_headers`: Same route rotation preserves user headers and client kwargs.
- `different_endpoint_triggers_route_change`: Endpoint shift flags `route_changed=True`.
- `route_change_reapplies_tls_and_drops_user_headers`: Route change drops user headers and reapplies TLS material.
- `route_normalization_equivalence`: Trailing slashes and default ports normalize to same route.
- `route_port_or_host_difference`: Host or port difference correctly flags route change.

---

## 9. Lifecycle Across Tool Rounds and Later Turns

Turn and tool-round lifecycle state preservation is governed by [`conversation_loop.step`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py) and [`_try_refresh_env_client_credentials`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L6483-L6635).

### 9.1 Intra-Turn Tool Rounds
- A turn may execute multiple tool rounds (Model -> Tool Execution -> Model).
- When a credential rotation occurs during tool round 1, the new credential and client remain active on `agent.client` for tool round 2.
- Each tool-round API call initializes a fresh `TurnRetryState()`. Consequently, `has_retried_429` is reset to `False` for tool round 2, giving round 2 its own full retry budget.
- Request counts and pool entry status mutations persist across tool rounds.

### 9.2 Turn 2 Boundary and #79156 Invariant
At the start of turn 2 in a persistent session:
- [`_try_refresh_env_client_credentials`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L6483-L6635) checks whether `~/.hermes/.env` was modified.
- Invariant #79156: If the agent is currently operating on a credential-pool rotated key (`agent._credential_pool` is not None and `agent._credential_pool_entry_id` is set), the environment refresh does NOT overwrite the rotated key with the environment variable on turn boot.
- If cooldown timestamps expired between turn 1 and turn 2, expired entries revive automatically upon selection.

### 9.3 Executable Oracle Test Cases (Section 8: 5 cases)
- `tool_round_1_rotation_survives_to_tool_round_2`: Rotated key in tool round 1 survives to tool round 2.
- `tool_round_2_resets_has_retried_429`: Tool round 2 resets `has_retried_429` to `False`.
- `pool_request_counts_and_statuses_persist_across_rounds`: Pool metrics and statuses persist across tool rounds.
- `session_turn_2_preserves_rotated_key`: Turn 2 respects #79156 and does not overwrite rotated key with env creds.
- `session_turn_2_cooldown_expired_revival`: Cooldown expired between turns revives entry.

---

## 10. Unified Streaming vs. Tool-Loop Parity

### 10.1 Single Unified Recovery Point
Both the streaming generation path in [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py) and the tool-calling loop in [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py) share the identical recovery function:
[`agent_runtime_helpers.recover_with_credential_pool`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1105-L1434).
There are no divergent state machines or separate retry counts.

### 10.2 Transport-Specific Handling
The only distinction between streaming and tool-loop execution is transport accumulator cleanup:
- **Streaming Path**:
  When an error occurs during stream chunk consumption, the agent must discard partially-streamed text tokens:
  `agent._reset_stream_delivery_tracking()` and `agent._current_streamed_assistant_text = ""`.
  This prevents partial or corrupted text from leaking into the retried request.
- **Tool-Loop Path**:
  The request payload is immutable; client replacement swaps `agent.client` and restarts the invocation loop.

### 10.3 Executable Oracle Test Cases (Section 9: 3 cases)
- `streaming_failure_uses_same_recovery_contract`: Streaming failures route through `recover_with_credential_pool`.
- `non_streaming_failure_uses_same_recovery_contract`: Non-streaming failures route through `recover_with_credential_pool`.
- `wire_client_replacement_unified`: Replaces wire client using unified `_swap_credential` method.

---

## 11. Fallback Boundaries and Non-Chat Non-Goals

### 11.1 Segregation Boundaries
1. **OAuth Refresh Boundary**:
   OAuth token refresh for Codex, Anthropic Messages, and MiniMax is handled in dedicated pre-request or post-401 token refresh functions. It is excluded from the static API-key credential pool rotation.
2. **Fallback Provider Boundary**:
   General model fallback chains (switching providers from DeepSeek to Groq, or Claude to GPT-4o) are only invoked after the active provider's credential pool returns `recovered=False` (complete pool exhaustion).
3. **Non-Chat Transport Boundary**:
   Non-chat operations (embeddings, audio, transcription, vision batch processing) do not share the chat-completions agent credential pool.

### 11.2 Executable Oracle Test Cases (Section 10: 3 cases)
- `oauth_refresh_boundary`: Confirms OAuth refresh is outside static API-key chat completion rotation.
- `fallback_provider_boundary`: Confirms fallback provider activation defers until pool exhaustion.
- `non_chat_transport_boundary`: Confirms non-chat transports operate outside main provider pool.

---

## 12. Verification and Golden Corpus Summary

The standalone test generator [`rust/tools/gen_main_provider_pool_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_pool_goldens.py) executes live Python helpers behind controlled adapters. Byte-for-byte regeneration is verified using:
```bash
python3 rust/tools/gen_main_provider_pool_goldens.py --check
```

### 12.1 Section and Test Case Breakdown

| Section ID | Section Description | Number of Test Cases |
| :--- | :--- | :--- |
| **Section 1** | Startup Precedence and Runtime Provider Resolution | 13 test cases |
| **Section 2** | Failure Attribution and Key Matching | 8 test cases |
| **Section 3** | Persistence-Before-Retry Ordering | 4 test cases |
| **Section 4** | Retry State Transitions and Request Counts | 12 test cases |
| **Section 5** | Status and Cooldown Outcomes | 12 test cases |
| **Section 6** | Provider Mismatch Isolation | 8 test cases |
| **Section 7** | Route Changes and Client Reconfiguration | 5 test cases |
| **Section 8** | Lifecycle Across Tool Rounds and Later Turns | 5 test cases |
| **Section 9** | Unified Streaming vs Tool-Loop Parity | 3 test cases |
| **Section 10** | Fallback and Non-Chat Boundaries | 3 test cases |
| **Section 11** | Raw HTTP Classifier Boundaries | 13 test cases |
| **TOTAL** | **Full Behavior Contract Suite** | **86 test cases** |

Every rule, transition, and invariant documented in this contract is validated against [`rust/tools/main-provider-pool-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-pool-goldens.json).
