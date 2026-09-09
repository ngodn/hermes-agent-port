# Auxiliary Compression Credential-Pool Selection and Request-Time Recovery Contract

This document provides the authoritative, exhaustive runtime contract audit for auxiliary compression credential-pool selection, request-time credential recovery, and failure accounting in the Python codebase.

This contract reflects the exact implementation across:
- [`agent/auxiliary_client.py`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py)
- [`agent/credential_pool.py`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py)
- [`hermes_cli/auth.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py)

Deterministic behavior across all scenarios documented here is verified by the standalone test generator [`rust/tools/gen_compression_credential_recovery_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_compression_credential_recovery_goldens.py) and checked against [`rust/tools/compression-credential-recovery-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/compression-credential-recovery-goldens.json) (10 sections, 64 test cases).

---

## 1. Scope and Architectural Boundaries

Auxiliary compression is executed by [`call_llm`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10296-L11200) with `task="compression"`. In this mode, latency and determinism are paramount: compression must operate within strict time and retry budgets to prevent user-facing turn execution from stalling.

The recovery architecture spans three distinct functional layers:
1. **Pure State Machine Layer**: Synchronous selection, rotation, exhaustion, cooldown, and sibling tracking encapsulated in [`CredentialPool`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L931-L1020).
2. **Transport & Client Management Layer**: Connection initialization, request execution, poisoned client cache eviction ([`_evict_cached_clients`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5088-L5101), [`_evict_cached_client_instance`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5103-L5135)), and ephemeral provider health tracking ([`_mark_provider_unhealthy`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4562-L4579)).
3. **Storage & Multi-Profile Shadowing Layer**: Disk persistence via [`persist_pool_entries`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L861-L895), disk cooldown merging via [`_merge_disk_cooldown_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L2295-L2335), and write-through isolation for single-use OAuth providers ([`SINGLE_USE_REFRESH_POOL_PROVIDERS`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1732-L1736)).

---

## 2. Pool Presence vs. Singleton and Environment Fallback

When auxiliary compression prepares a provider client, credential resolution follows an exact priority ladder.

### 2.1 Credential Resolution Precedence
The provider client probe order is defined in [`_select_pool_entry`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L1582-L1596) and [`_peek_pool_entry`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L1598-L1619):

1. **Active Pool Entry**:
   If a [`CredentialPool`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L931-L1020) exists for the provider and contains at least one available entry (status `STATUS_OK`, or `STATUS_EXHAUSTED` with `now >= exhausted_until`), [`select()`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2367-L2400) or [`peek()`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2627-L2635) returns the entry ID.
   The runtime token and base URL are extracted via [`_pool_runtime_api_key`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L1621-L1628) and [`_pool_runtime_base_url`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L1630-L1637).
2. **Singleton / Environment Fallback**:
   If no pool exists, or if the pool is fully exhausted / dead (`select()` returns `None`), the provider falls back to its legacy singleton credentials:
   - `openrouter`: Checked against `OPENROUTER_API_KEY` in `os.environ`, then `auth.json` singleton entry `api_key`.
   - `openai-codex`: Checked against `auth.json` provider entry `openai-codex` or `~/.openai/credentials.json`.
   - `anthropic`: Checked against `ANTHROPIC_API_KEY` in `os.environ`, then `~/.claude/.credentials.json` (Claude Code OAuth tokens).
   - `gemini` / `google`: Checked against `GEMINI_API_KEY`, then `GOOGLE_API_KEY`.
   - `github-copilot`: Checked against `~/.config/github-copilot/hosts.json` / GitHub CLI oauth tokens.
   - `cerebras`, `groq`, `sambanova`, `minimax`, `mistral`, `together`, `deepseek`: Checked against their standard uppercase environment variables (`<PROVIDER>_API_KEY`).
   - `local/...`: Custom base URL endpoints without mandatory API keys.
3. **Quarantine / Unhealthy Marking**:
   If neither a pool entry nor an environment/singleton fallback is resolvable, the provider is marked unhealthy for 60 seconds via [`_mark_provider_unhealthy(provider, 60, reason="unconfigured")`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4562-L4579) and skipped for subsequent compression requests until the quarantine expires.

### 2.2 Pool Status Distinctions
- **Active Pool**: At least one entry has `status == STATUS_OK` or has elapsed its `exhausted_until` timestamp.
- **Exhausted Pool**: All entries have `status == STATUS_EXHAUSTED` with `exhausted_until > now`. In-memory pool returns `None` on `select()`.
- **Dead Pool**: All entries have `status == STATUS_DEAD`.
- **Empty Pool**: Pool exists as an object with `entries = []`.

---

## 3. Selection Strategy and Cooldown / Exhaustion Eligibility

The in-memory credential pool is managed by [`CredentialPool`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L931-L1020).

### 3.1 Selection Strategies
The pool supports four deterministic strategies specified by `strategy`:
- **`fill_first`** (default):
  Sorts available entries strictly by `priority` (ascending integer, default 10). Ties are broken by original position. The first available entry is selected. It continues to be returned on subsequent requests until marked exhausted or dead.
- **`round_robin`**:
  Maintains an internal cursor across available entries. Successive `select()` calls advance to the next index modulo the number of available entries.
- **`least_used`**:
  Selects the available entry with the lowest `usage_count`. Ties are broken by `priority`.
- **`random`**:
  Picks an entry uniformly at random from available entries using Python's `random.choice`. (In deterministic goldens, seeded or intercepted via `choose_random`).

### 3.2 Availability Rules (`_available_entries`)
Evaluated synchronously in [`_available_entries`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2403-L2453) against current timestamp `now`:
1. Entries with `status == STATUS_DEAD` (`"dead"`) are permanently excluded.
2. Entries with `status == STATUS_EXHAUSTED` (`"exhausted"`):
   - Excluded if `exhausted_until is not None and now < exhausted_until`.
   - **Revived to eligible** if `now >= exhausted_until`. Note: the pool does NOT immediately mutate `status` back to `STATUS_OK` in storage during `_available_entries`, but treats the entry as available for selection.
3. **Dead Manual Pruning**:
   Dead entries marked with `source == "manual"` (or containing `manual_revocation` in error details) that have been dead longer than [`DEAD_MANUAL_PRUNE_TTL_SECONDS`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L111) (86,400s / 24 hours) are pruned from the pool and removed from storage on the next rotation or prune cycle. Singleton-seeded entries are NEVER auto-pruned; they remain dead until external re-auth.

### 3.3 Cooldown TTL Calculation (`_exhausted_ttl`)
Cooldown TTL calculation is governed by [`_exhausted_ttl`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L328-L370) and [`_exhausted_until`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L450-L485):

| Failure Category | Status / Condition | Default TTL | Sole-Credential Shortening | Upstream Override Handling |
| :--- | :--- | :--- | :--- | :--- |
| **Auth Failure** | HTTP 401 | 300s (`EXHAUSTED_TTL_401_SECONDS`) | No | N/A |
| **Rate Limit** | HTTP 429 | 3600s (`EXHAUSTED_TTL_429_SECONDS`) | **Yes -> 60s** (`EXHAUSTED_TTL_SOLE_CREDENTIAL_SECONDS`) if sole non-dead entry | `retry-after`, `x-ratelimit-reset`, `reset_at`, `quotaResetDelay` parsed from headers/body override both default and sole-key TTL |
| **Billing / Quota** | HTTP 402, `insufficient_quota`, `billing_not_active`, `credit_exhausted` | 3600s (`EXHAUSTED_TTL_DEFAULT_SECONDS`) | **No** (sole-credential shortening is explicitly bypassed via `is_billing`) | `reset_at` timestamp if present in error payload |
| **Unverified Billing** | `billing_unverified` with non-402 status | 60s | No | Parsed delay if present |
| **Other / Default** | Generic network/5xx | 3600s (`EXHAUSTED_TTL_DEFAULT_SECONDS`) | **Yes -> 60s** if sole non-dead entry | Parsed delay if present |

#### Header & Body Parsing Rules:
- Header `retry-after`: Parsed as either integer seconds (e.g. `"120"`) or HTTP-date string via [`_extract_retry_delay_seconds`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L402-L448).
- Header `x-ratelimit-reset`: Parsed as epoch timestamp float or relative seconds.
- JSON error fields `reset_at` / `quotaResetDelay`: Parsed via [`_parse_absolute_timestamp`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L372-L400) (supports ISO-8601 strings e.g. `2026-09-10T04:10:00Z` and epoch floats).

---

## 4. Runtime Key, Base URL, and Model Identity

### 4.1 Field Extraction Logic
[`_pool_runtime_api_key`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L1621-L1628) and [`_pool_runtime_base_url`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L1630-L1637) extract identity from a pooled credential dictionary:

- **Runtime API Key**:
  Checks the following fields in order: `api_key`, `token`, `access_token`, `credential`.
  The value is trimmed of leading/trailing whitespace. If missing or non-string, returns empty string `""`.
- **Runtime Base URL**:
  Checks `base_url`, `url`, `endpoint`.
  Strips whitespace and trailing slashes (e.g. `"https://api.together.xyz/v1/"` -> `"https://api.together.xyz/v1"`).
  Returns `None` if not present.

### 4.2 Client Cache Key
The auxiliary client caches instantiated SDK clients in `_AUXILIARY_CLIENTS` ([`agent/auxiliary_client.py#L8230`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8230)).
The cache key is computed by [`_client_cache_key`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8234-L8284):
```python
cache_key = (
    provider,
    base_url,
    hashlib.sha256(api_key.encode("utf-8")).hexdigest()[:16] if api_key else None,
    credential_id,
    timeout,
)
```
This guarantees that clients bound to different pool entries or rotated keys never collide in the cache.

### 4.3 Auxiliary Compression Model Resolution
Compression requests resolve their model through the following cascade:
1. `auxiliary.compression_model` configuration override.
2. `auxiliary.model` general auxiliary fallback model.
3. Provider default compression model (e.g., `anthropic/claude-3-5-haiku`, `openai/gpt-4o-mini`, `google/gemini-2.0-flash`).
4. **Free-Only Guardrail**:
   When `auxiliary.free_only: true` is configured, OpenRouter fallbacks are restricted to `:free` SKUs (e.g. `meta-llama/llama-3.3-70b-instruct:free`). Non-free models are blocked to prevent unintended spend.

---

## 5. 401 Refresh vs. Rotation State Machine

Request-time credential recovery for 401 Unauthorized errors is coordinated by [`mark_exhausted_and_rotate`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2637-L2785) and [`call_llm`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10850-L11025).

### 5.1 Static API Keys vs. Refreshable OAuth Credentials
1. **Static API Keys**:
   API keys cannot be refreshed over the network. Upon receiving a 401 error:
   - The pool marks the key as `STATUS_EXHAUSTED` (TTL 300s) or `STATUS_DEAD` (if terminal).
   - The pool immediately selects and returns the next available entry.
2. **OAuth Credentials (Codex, Claude Code, etc.)**:
   - The auxiliary client first attempts in-place network token refresh via [`_refresh_provider_credentials`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5409-L5454) and [`try_refresh_current`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2864-L2900).
   - If refresh succeeds, the entry is updated in-place with the new `access_token` and `expires_at`, its status remains `STATUS_OK`, and the request is retried.
   - If refresh fails (or no refresh token exists), the credential transitions to [`mark_exhausted_and_rotate`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2637).

### 5.2 Terminal Auth Failure Detection
In [`_is_terminal_auth_failure`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1071-L1098), a failure is deemed terminal if the error reason or OAuth error string matches any of:
- `token_invalidated`
- `token_revoked`
- `invalid_token`
- `invalid_grant`
- `unauthorized_client`
- `refresh_token_reused`
- `credential_persist_failed` ([`CREDENTIAL_PERSIST_FAILED_REASON`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L98))

**Terminal Transition**: An entry matching any terminal failure reason transitions directly to `status = STATUS_DEAD` (`"dead"`). It will never revive on cooldown and cannot be selected again.

### 5.3 Key Disagreement Resolution
In asynchronous or pipelined environments, the entry currently pointed to by `pool.current_id` may differ from the key that actually failed the request.
[`mark_exhausted_and_rotate`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2654-L2675) accepts `credential_id` and `api_key_hint`:
- If `credential_id` matches an entry, but that entry's runtime key does NOT match `api_key_hint`, the pool searches all entries for `api_key_hint`.
- If an entry matching `api_key_hint` is found, **that matching entry** is marked exhausted/dead instead of the index indicated by `credential_id`.
- This guarantees that an innocent credential is never punished for another credential's failure.

### 5.4 Sibling Key Exhaustion
If multiple pool entries share the exact same `runtime_api_key` (e.g. duplicate entries added with different IDs or priorities), [`mark_exhausted_and_rotate`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2757-L2775) iterates through all sibling entries:
- Any sibling entry possessing identical `runtime_api_key` is marked with the same failure status, error context, and cooldown timestamp.
- This prevents rotation from cycling between duplicate instances of the same failing key.

### 5.5 Unmatched Rotation Streak Capping
If an error arrives with a `credential_id` or `api_key_hint` that does not match ANY entry in the pool:
- The pool increments an internal counter `_unmatched_rotation_streak` ([`agent/credential_pool.py#L2700-L2725`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L2700-L2725)).
- If `_unmatched_rotation_streak > max(len(available), 1)`:
  - The pool clears `_unmatched_rotation_streak = 0`.
  - Clears `current_id = None`.
  - Returns `None` immediately to surface the error and break the rotation loop.
- Special single-entry rule: For a pool with only 1 available entry, an unmatched rotation streak triggers failure immediately without cycling.

---

## 6. 402 and 429 Accounting and Provider Health

### 6.1 429 Rate Limit Handling
When an auxiliary compression call encounters HTTP 429:
1. The failing entry is marked `STATUS_EXHAUSTED` in the pool via [`_recover_provider_pool`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5210-L5257).
2. The cooldown is set according to Section 3.3 (3600s multi-key, 60s sole-key, or upstream header override).
3. If the pool has another available entry, [`_retry_same_provider_sync`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5259-L5333) executes a single retry using the next rotated entry.
4. If all pool entries are exhausted, the provider fails and execution advances to the next provider in the fallback chain.

### 6.2 402 Payment and Credit Exhaustion Handling
When an auxiliary call encounters HTTP 402 or billing errors (`insufficient_quota`, `credit_exhausted`):
1. The failing entry is marked `STATUS_EXHAUSTED` in the pool with default 3600s TTL. Sole-credential shortening is explicitly disabled.
2. If the pool has another credential, the request is retried once with that
   rotated credential. The clear payment error only suppresses the preliminary
   retry on the already-failed key.
3. If recovery still fails, the **entire provider** is placed in quarantine via
   [`_mark_provider_unhealthy`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4562-L4579) for the default **600 seconds** (10 minutes).

### 6.3 Provider Health Tracking (`_PROVIDER_HEALTH`)
Provider health is stored in a module-level dict `_PROVIDER_HEALTH` mapping normalized provider labels to expiration epochs:
- **Normalization** ([`_normalize_chain_label`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4550-L4560)):
  - Model suffixes are stripped: `"openrouter/openai/gpt-4o-mini"` -> `"openrouter"`.
  - Custom local endpoints are preserved: `"local/vllm"` -> `"local/vllm"`.
- **Health Verification** ([`_is_provider_unhealthy`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4581-L4595)):
  - If `now < unhealthy_until`, the provider is skipped immediately during fallback traversal without making any network calls.

---

## 7. Poisoned-Client Eviction

To prevent stale TCP sockets, cached auth headers, or expired TLS sessions from contaminating subsequent requests after credential failure:

1. **Eviction by Provider and Hint**:
   [`_evict_cached_clients(provider, api_key_hint=...)`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5088-L5101) inspects `_AUXILIARY_CLIENTS` and purges all client cache keys matching the provider and (if supplied) the SHA-256 hash of the failed key.
2. **Instance Unwrapping and Purging**:
   [`_evict_cached_client_instance(client)`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5103-L5135) inspects cached instances. If the client is a proxy or wrapper (e.g. containing `_real_client` or `client`), it traverses the wrapper chain, matches underlying references, and removes all associated cache entries.
3. **Trigger Invariants**:
   - Eviction MUST occur synchronously before [`_retry_same_provider_sync`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5259) or fallback chain advancement.

---

## 8. Auxiliary Compression Request and Retry Budgets

Compression operations run during context compaction and cannot tolerate long delays. The retry budget is enforced by two strict rules:

### 8.1 Compression Timeout Fast-Fail
Defined in [`_should_skip_same_provider_retry`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5064-L5086):
```python
if task in ("compression", "context_pruning"):
    if isinstance(exc, (TimeoutError, httpx.ConnectTimeout, httpx.ReadTimeout, openai.APITimeoutError)):
        return True
```
- For `task="compression"`, ANY timeout exception immediately aborts same-provider retries.
- The pool marks the entry, does NOT attempt another key on the same provider, and advances immediately to the next provider in the fallback chain.

### 8.2 Same-Provider Retry Budget
In [`_retry_same_provider_sync`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5259-L5333):
- The rotated-credential retry budget is strictly capped at **1 retry**.
- If the rotated key fails (`retry2_err`), the pool marks the second key exhausted via [`_recover_provider_pool(pool_provider, retry2_err)`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L11017).
- No further retries are attempted on this provider; control returns `None` and falls through to the next provider in the fallback chain.
- Auth and payment failures therefore permit at most **2** requests on the
  provider (initial dispatch plus one rotated-key retry). An ordinary 429 has
  one additional preliminary retry on the failed key, so that branch permits
  at most **3** requests before provider fallback.

---

## 9. Profile / Root Auth Store Shadowing and Single-Use Grants

Multi-tenant profile and storage resolution is governed by [`read_credential_pool`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L2238-L2293) and [`persist_pool_entries`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L861-L895).

### 9.1 Storage Shadowing Hierarchy
When running inside a named profile (e.g. `profile == "research"`):
1. **Provider-Level Shadowing**:
   If the profile's `auth.json` contains entries under
   `credential_pool.<provider>`, those entries **completely shadow** the global
   root `auth.json` for that provider.
2. **Root Fallback**:
   If and only if the profile has **zero** entries for a provider, [`read_credential_pool`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L2238) falls back to reading the entries from the global root `auth.json`.

### 9.2 Disk Cooldown Merging (`_merge_disk_cooldown_state`)
In-memory pool entries periodically synchronize cooldowns with disk via [`_merge_disk_cooldown_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L2295-L2335):
- A status recorded on disk (`STATUS_EXHAUSTED` or `STATUS_DEAD`) is adopted in memory only if `disk.last_status_at > memory.last_status_at`.
- If the `access_token` on disk differs from the in-memory token (indicating external re-auth or refresh by another process), the in-memory status is reset to `STATUS_OK` and cooldowns are cleared.

### 9.3 Single-Use Provider Isolation (`SINGLE_USE_REFRESH_POOL_PROVIDERS`)
Providers with single-use refresh tokens (`anthropic`, `openai-codex`, `xai-oauth`):
- Defined in [`SINGLE_USE_REFRESH_POOL_PROVIDERS`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1732-L1736).
- When a profile reads a single-use grant from root, updates to that grant (e.g. rotated refresh token) **write through directly to root storage** (`~/.hermes/auth.json` or `~/.claude/.credentials.json`).
- Grants are never materialized or cloned into profile storage, preventing token forking and desynchronization.

### 9.4 Disk Write Failure Behavior (`_fail_closed_unpersisted_rotation`)
If disk persistence fails during a single-use token rotation (e.g. disk full, EACCES, read-only filesystem):
- [`_fail_closed_unpersisted_rotation`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1752-L1795) is invoked.
- The credential fails closed: status is set to `STATUS_DEAD` and failure reason is set to `CREDENTIAL_PERSIST_FAILED_REASON` (`"credential_persist_failed"`).
- In-memory client caches are evicted immediately.
- This prevents the runtime from holding in-memory tokens that cannot be reloaded on next startup.

---

## 10. Pure State Machine vs. Transport / Refresh Effects

The contract strictly separates pure deterministic state machine transitions from transport-level network and I/O effects:

| Feature / Operation | Pure State Machine (Synchronous) | Transport / Refresh Effects (Async / I/O) |
| :--- | :--- | :--- |
| **Credential Selection** | `select()` filters `STATUS_DEAD`, checks `now >= exhausted_until`, applies strategy (`fill_first`, `round_robin`, `least_used`, `random`). | None. No network calls or disk reads occur during selection. |
| **Rotation on 401** | `mark_exhausted_and_rotate()` matches `api_key_hint`, updates failure context, marks siblings, caps streak. | `_refresh_provider_credentials()` invokes OAuth HTTP token exchange. |
| **Terminal Check** | `_is_terminal_auth_failure()` inspects error strings, transitions entry to `STATUS_DEAD`. | None. |
| **Cooldown Calculation** | `_exhausted_ttl()` computes TTL based on status code, error strings, and parsed header delays. | Extracting headers (`retry-after`, `reset_at`) from HTTP response. |
| **Dead Pruning** | `_prune_dead_entries()` removes manual entries dead > 24h. | None. |
| **Storage Persistence** | `persist_now()` schedules or invokes the persistence callback. | Disk I/O writing `auth.json`, filesystem lock acquisition. |
| **Poisoned Client Eviction** | None (client cache is outside `CredentialPool`). | `_evict_cached_clients()` deletes SDK instances from `_AUXILIARY_CLIENTS`. |
| **Provider Quarantine** | None. | `_mark_provider_unhealthy()` updates module-level `_PROVIDER_HEALTH`. |

---

## 11. Python-Owned State and Rust Seam Boundaries

To implement the Rust seam cleanly without carrying Python runtime quirks, the Python-owned state and interface boundary are defined as follows:

### 11.1 Python-Owned Mutable State
1. **`CredentialPool` In-Memory State**:
   - `entries: list[PooledCredential]`
   - `current_id: Optional[str]`
   - `unmatched_rotation_streak: int`
   - `clock: Callable[[], float]`
2. **Auxiliary Client Global State**:
   - `_PROVIDER_HEALTH: dict[str, float]` (normalized provider -> unhealthy_until epoch)
   - `_AUXILIARY_CLIENTS: dict[tuple, Any]` (client cache keys -> SDK client instances)
3. **On-Disk Persistent State**:
   - `auth.json`: `{"providers": { "<provider>": { "pool": [ { "id": ..., "status": ..., "last_status_at": ..., "exhausted_until": ... } ] } } }`

### 11.2 Rust Seam Boundary Contract
The Rust rewrite interfaces with this subsystem via:
1. **Runtime Credential State**:
   A pure struct `RuntimeCredential { id: String, api_key: String, base_url: Option<String> }` returned by `select_runtime()` and `mark_exhausted_and_rotate()`.
2. **Synchronous Transition Driver**:
   Rust code passes `(status_code, error_context, api_key_hint, credential_id, failure_reason)` into `mark_exhausted_and_rotate()`.
3. **Decoupled Refresh Sink**:
   Network OAuth token refresh operates entirely outside the synchronous pool state machine. The state machine receives only the outcome (success with updated token, or failure with error context).

---

## 12. Golden Suite Mapping and Verification

The test corpus in [`rust/tools/compression-credential-recovery-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/compression-credential-recovery-goldens.json) validates the 10 core contract sections across 64 deterministic cases:

| Section | Cases | Key Behaviors Verified |
| :--- | :--- | :--- |
| **`pool_presence_and_fallback`** | 10 | Pool priority over env/singleton, exhausted pool fallback, empty pool fallback, unconfigured quarantine for OpenRouter, Codex, Anthropic, Gemini, Copilot, Cerebras, Minimax, Sambanova, Groq, and Local endpoints. |
| **`selection_strategy_and_eligibility`** | 9 | `fill_first`, `round_robin`, `least_used`, cooldown expiry revival, 24h dead manual pruning, singleton dead retention. |
| **`runtime_identity_resolution`** | 13 | Token extraction (`api_key`, `token`, `access_token`, `credential`), base URL normalization, whitespace stripping, cache key computation, OpenRouter free-only restriction. |
| **`recovery_401_refresh_and_rotation`** | 7 | Static API key rotation, OAuth token refresh retry, terminal 401 dead transition, key disagreement attribution, sibling key exhaustion, unmatched streak capping. |
| **`recovery_402_429_accounting`** | 4 | 429 3600s cooldown, 429 60s sole-key cooldown, upstream header override (`retry-after` / `reset_at`), 402 payment 3600s cooldown without sole-key shortening. |
| **`provider_health_tracking`** | 3 | Default 600s quarantine, label normalization (`openrouter/...` -> `openrouter`), and fast-fail skip. The corpus also exercises explicit 300s overrides without claiming that value is the default. |
| **`poisoned_client_eviction`** | 3 | Eviction by provider and key hash, instance unwrapping (`_real_client`), socket purge prior to retry. |
| **`request_retry_budget`** | 2 | Compression timeout fast-fail (`TimeoutError` aborts same-provider retry), maximum 1 same-provider retry budget. |
| **`auth_store_shadowing_and_persistence`** | 3 | Profile shadowing root, profile fallback to root, disk cooldown merging (`last_status_at` comparison), single-use persistence failure fail-closed quarantine. |
| **`transport_dependency_matrix`** | 10 | Complete classification of operations into pure state transitions vs. transport/refresh effects across all providers. |

### Verification Command
Run the verification check at any time:
```bash
python3 rust/tools/gen_compression_credential_recovery_goldens.py --check
```
Output:
```
OK: corpus matches checked-in goldens
```

---

## 13. Explicit Unknowns, Edge Cases, and Ambiguities

1. **Concurrent Rotation Races in Multi-Threaded/Async Contexts**:
   `CredentialPool` uses an in-memory lock for thread-safety, but separate OS processes sharing `auth.json` rely on file locking during persistence. In high-concurrency environments, a process may read a slightly stale cooldown timestamp before disk syncing occurs.
2. **Rate Limit Header Variance Across Model Providers**:
   While OpenAI/OpenRouter use standard `retry-after` and `x-ratelimit-reset`, certain providers return non-standard headers (e.g. `quotaResetDelay` in body or `x-ratelimit-reset-tokens` vs `requests`). The contract prioritizes absolute timestamps (`reset_at`) over relative delays when both are present.
3. **Single-Entry Unmatched Rotation Immediate Failure**:
   When a pool has only 1 available entry and receives an unmatched error identity, it immediately fails closed (`return None`) without cycling, because `unmatched_streak > max(1, 1)` triggers on streak count 2.
