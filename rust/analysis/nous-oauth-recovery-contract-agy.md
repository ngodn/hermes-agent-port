# Nous Auxiliary OAuth Discovery and Request-Time Recovery Contract

This document establishes the authoritative runtime behavioral contract for Nous Portal OAuth discovery, runtime key selection, single-use refresh token rotation, cross-process concurrency locking, anti-stampede peer adoption, and request-time failure recovery in the Python codebase.

This contract reflects the exact implementation across:
- [`agent/credential_pool.py`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py)
- [`hermes_cli/auth.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py)
- [`agent/auxiliary_client.py`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py)

Deterministic behavior across all 12 domains documented here is verified by the standalone test generator [`rust/tools/gen_nous_oauth_recovery_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_nous_oauth_recovery_goldens.py) and checked against [`rust/tools/nous-oauth-recovery-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/nous-oauth-recovery-goldens.json) (12 sections, 87 test cases).

---

## 1. Executive Summary and Architectural Scope

Auxiliary LLM tasks (notably context compression via `task="compression"`) utilize Nous Portal OAuth credentials to access Nous inference endpoints. Because Nous OAuth issues single-use refresh tokens under OAuth 2.1 specifications, token rotation requires strict serialization, write-through disk ordering, peer rotation detection, and cross-profile synchronization.

The Nous recovery architecture spans three coordinated subsystems:
1. **The Credential Pool Layer** ([`agent/credential_pool.py`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py)):
   Houses [`PooledCredential`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L204-L309) representations and [`CredentialPool`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L931-L1020) instances. It arbitrates entry selection, tracks rotation, provides proactive peer-sync in [`_sync_nous_entry_from_auth_store`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1446-L1516), performs single-use write-through synchronization in [`_sync_device_code_entry_to_auth_store`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1518-L1660), and enforces atomic entry quarantine in [`_quarantine_entry`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1793-L1825).
2. **The Authentication and OAuth Engine** ([`hermes_cli/auth.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py)):
   Coordinates device-code authentication, token verification ([`_nous_invoke_jwt_status`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L3218-L3245), [`_nous_invoke_jwt_is_usable`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L3247-L3263)), single-use refresh serialization under [`_auth_store_lock`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1250-L1275), peer rotation detection via [`_already_rotated_by_peer`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7100-L7113), persistence-before-validation ordering ([`_persist_state("post_refresh_access_token")`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7370-L7376)), cross-profile shared store replication ([`_write_shared_nous_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6229-L6275)), and terminal error quarantine ([`_quarantine_nous_oauth_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6391-L6475)).
3. **The Auxiliary Client Transport and Cache Layer** ([`agent/auxiliary_client.py`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py)):
   Manages HTTP transport clients, executes auxiliary inference calls in [`call_llm`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10296-L11200), coordinates 401 recovery and client cache replacement in [`_refresh_nous_auxiliary_client`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8286-L8351), enforces request timeout floors in [`_effective_aux_timeout`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L9065-L9080), blocks redundant full-budget retries for critical tasks via [`_TIMEOUT_NO_RETRY_TASKS`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5061-L5085), and maintains ephemeral circuit breakers via [`_mark_provider_unhealthy`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4562-L4579).

---

## 2. Profile and Root Ownership (Write-Through Isolation)

### 2.1 Multi-Profile Fallback and Source Attribution
In multi-profile deployments (invoked via `--profile <name>`), Hermes stores configuration and credentials in per-profile directories (`~/.hermes/profiles/<name>/auth.json`), while maintaining the root store (`~/.hermes/auth.json`).

Resolution order and source tracking are executed in [`_load_provider_state_with_source`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1526-L1570):
1. The profile's own `auth.json` is inspected for `providers.nous`.
2. If absent from the profile store, [`_load_provider_state_with_source`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1526-L1570) falls back to the global root store (`~/.hermes/auth.json`).
3. The function returns both the provider dictionary and the filesystem path from which it was loaded (`state, source_path`).
4. In classic mode (where `_global_auth_file_path()` is `None` because no profiles are active), the profile store is the root store.

### 2.2 The Self-Sealing Shadowing Invariant (#74339)
When a profile borrows Nous credentials from the global root store (`is_from_root = True`), token rotation triggers [`_sync_device_code_entry_to_auth_store`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1518-L1660):
- **Root Write-Through**: The updated tokens are written directly to the global root store via [`_write_through_provider_state_to_global_root("nous", state)`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1648-L1650).
- **Prohibition on Profile Pollution**: The code strictly skips `_store_provider_state(auth_store, "nous", ...)` for the profile store.
- **Rationale**: If `providers.nous` were written into the profile's `auth.json`, that profile would immediately cease borrowing from root. On the next rotation, the profile would rotate its own local tokens, leaving the root store with a spent single-use refresh token. Subsequent profile runs or sibling profiles borrowing from root would then fail permanently with `refresh_token_reused` or `invalid_grant`.
- Conversely, when `is_from_root = False` (the profile genuinely owns its own `providers.nous` block), updates are saved to the profile store via `_store_provider_state(auth_store, "nous", state, set_active=False)`.

### 2.3 `set_active` Persistence Behavior across Background vs. Direct Paths
The persistence layers explicitly distinguish background token rotation from direct runtime authentication:
- **Background Pool Synchronization Path**:
  When [`_sync_device_code_entry_to_auth_store`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1518-L1660) writes rotated pool tokens back to `auth.json`, it strictly uses `set_active=False`:
  - For profile-owned grants: calls [`_store_provider_state(auth_store, self.provider, state, set_active=False)`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1654-L1657).
  - For root write-through: [`_write_through_provider_state_to_global_root`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L756-L809) calls [`auth_mod._persist_provider_state_to_store(provider_id, state, global_path, set_active=False)`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L797-L802).
  Background pool token rotation is an internal maintenance event and must never alter the user's active provider selection.
- **Direct Runtime Resolution Path**:
  In contrast, [`resolve_nous_runtime_credentials`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7065-L7395) writes refreshed tokens through [`_save_provider_state_to_source(auth_store, "nous", state, state_source_path)`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1611-L1636):
  - When saving to the active store (`same_store`), it calls [`_save_provider_state(auth_store, provider_id, state)`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1602-L1609), which unconditionally sets `auth_store["active_provider"] = provider_id` (effectively `set_active=True`).
  - When persisting to an external source store (`not same_store`), it calls [`_persist_provider_state_to_store(provider_id, state, source_path, set_active=True)`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1630-L1635).
  Direct runtime credential resolution may therefore establish or reinforce `active_provider = "nous"`. These two persistence paths fulfill different contracts and must not be conflated.

### 2.4 Cross-Profile Shared Store Partitioning
Hermes maintains a shared store file (`~/.hermes/shared/nous_auth.json`, or `$HERMES_SHARED_AUTH_DIR/nous_auth.json`) defined by [`hermes_cli.auth.NOUS_SHARED_STORE_FILENAME`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6117) via [`_write_shared_nous_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6229-L6275):
- **Included Keys**: `access_token`, `refresh_token`, `token_type`, `scope`, `client_id`, `portal_base_url`, `inference_base_url`, `obtained_at`, `expires_at`, and `updated_at`.
- **Excluded Keys**: The runtime key `agent_key`, its identifier `agent_key_id`, and its expiry metadata are strictly omitted from the shared store. `agent_key` is volatile and process-specific, whereas OAuth refresh tokens are cross-profile sources of truth.
- **Atomic File Creation**: The file is written with mode `0o600` atomically via `os.open(O_EXCL)` to eliminate time-of-check to time-of-use (TOCTOU) permission windows (#19673, #21148).

---

## 3. Singleton Seeding and In-Place Idempotence

### 3.1 Seeding Mechanism
When Nous authentication is established or updated via [`persist_nous_credentials`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6996-L7055):
1. Secrets and endpoints are written to `providers.nous` in `auth.json`.
2. The state is mirrored to the shared store via [`_write_shared_nous_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6229).
3. The credential pool is seeded by invoking [`load_pool("nous")`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3211-L3270).

Inside [`_seed_from_singletons`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3211-L3270) and [`_upsert_entry`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3012-L3080):
- If no existing pool entry has `source == "device_code"`, a singleton entry is materialized. Legacy or independently managed `manual:device_code` rows do not satisfy this exact-source lookup:
  - `id`: minted dynamically as a six-character hex string via `uuid.uuid4().hex[:6]` in [`_upsert_entry`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3024) (not a fixed constant).
  - `label`: defaults to `payload.get("label") or source`, where `_seed_from_singletons` computes `seeded_label = custom_label or label_from_token(...)` ([`agent/credential_pool.py#L3235-L3237`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3235-L3237)); when no custom label or identity claims (`email`, `preferred_username`, `upn`) exist in the token, this defaults to `"device_code"`.
  - `auth_type`: `AUTH_TYPE_OAUTH` (`"oauth"`).
  - `priority`: dynamic priority from [`_next_priority(entries)`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3025).
  - `source`: `"device_code"`.
  - `access_token`: extracted from `providers.nous.access_token`.
  - `refresh_token`: extracted from `providers.nous.refresh_token`.
  - `agent_key`: extracted from `providers.nous.agent_key`.
- If an existing entry with `source == "device_code"` already exists:
  - `id` and `priority` are skipped and strictly preserved.
  - `label` is preserved if `existing.label` is already present (`if key == "label" and existing.label: continue` in [`_upsert_entry`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3054-L3055)), preserving an established label across updates.
  - Token and metadata updates are merged in place.

### 3.2 In-Place Idempotence
Repeated calls to [`persist_nous_credentials`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6996-L7055) or [`_seed_from_singletons`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L3211-L3270) locate the existing entry by ID or source. Existing entries are updated in place:
- Duplicate rows are never created.
- Total pool entry count remains stable.
- Manual entries (`source="manual"`) added by `hermes auth add` coexist alongside the singleton entry and are never overwritten.

---

## 4. Access-Token and Runtime-Key Selection Precedence

### 4.1 Candidate Key Precedence Ladder
When preparing an inference request for Nous, [`PooledCredential.runtime_api_key`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L282-L302) evaluates candidate tokens in an exact order:

```python
for token, expires_at in (
    (self.agent_key, self.agent_key_expires_at),
    (self.access_token, self.expires_at),
):
    if (
        isinstance(token, str)
        and token.strip()
        and auth_mod._nous_invoke_jwt_is_usable(
            token,
            scope=getattr(self, "scope", None),
            expires_at=expires_at,
        )
    ):
        return token.strip()
return ""
```

1. **`agent_key` Candidate**: Evaluated first. If present, non-empty, and valid as a Nous inference JWT via [`_nous_invoke_jwt_is_usable`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L3247-L3263), it is selected immediately.
2. **`access_token` Candidate**: Evaluated second if `agent_key` is missing, blank, expired, or lacking required scopes. If valid and usable, it is selected.
3. **Empty String Fallback**: If neither candidate qualifies, `runtime_api_key` returns `""` (empty string). An empty string signals to the transport layer that the entry has no valid inference credential.

### 4.2 Runtime Base URL Selection
For Nous pool entries, [`PooledCredential.runtime_base_url`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L304-L308) returns:
- `self.inference_base_url` if present and non-empty.
- Otherwise, falls back to `self.base_url`.

---

## 5. Expiry, Clock Skew, and Scope Verification

### 5.1 Skew Constants
- `ACCESS_TOKEN_REFRESH_SKEW_SECONDS = 120` ([`hermes_cli/auth.py#L5996`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L5996))
- `NOUS_INVOKE_JWT_MIN_TTL_SECONDS = 120` ([`hermes_cli/auth.py#L3214`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L3214))

### 5.2 Verification Pipeline in `_nous_invoke_jwt_status`
[`_nous_invoke_jwt_status`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L3218-L3245) inspects a candidate token:
1. **JWT Structure Check**:
   The token must comprise three period-delimited base64url segments (`header.payload.signature`). If unparseable or claims are empty, returns `"access_token_not_jwt"`.
2. **Scope Verification**:
   The effective scopes are calculated as the union of:
   - Explicit `scope` parameter (split on whitespace or commas)
   - Payload `scope` claim
   - Payload `scp` claim (array or space-delimited string)
   If `"inference:invoke"` (`NOUS_INFERENCE_INVOKE_SCOPE`) is absent from this union, returns `"missing_inference_invoke_scope"`.
3. **Timestamp Expiry with 120-Second Skew**:
   - If payload contains `exp` (numeric epoch timestamp):
     If `float(exp) <= time.time() + 120`, returns `"invoke_jwt_expiring"`.
   - If payload lacks `exp`:
     Evaluates `expires_at` via [`_is_expiring(expires_at, 120)`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L2254-L2265). If expired, expiring within 120s, or unparseable, returns `"invoke_jwt_expiry_unknown_or_expiring"`.
4. **Usability Decision**:
   Returns `None` if and only if all checks pass. [`_nous_invoke_jwt_is_usable`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L3247) returns `True` when status is `None`.

---

## 6. Forced 401 Refresh and Auxiliary Client Cache Invalidation

### 6.1 Recovery Trigger on HTTP 401
When an auxiliary call receives an HTTP 401 Unauthorized response from a Nous inference endpoint, [`call_llm`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L10850) triggers recovery via [`_refresh_nous_auxiliary_client`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8286-L8351):
1. Passes `stale_access_token=api_key` to [`resolve_nous_runtime_credentials(force_refresh=True)`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7065-L7395).
2. Sets `force_refresh=True` to bypass local memory and disk caches.

### 6.2 Wire Exchange via `_refresh_access_token`
In [`_refresh_access_token`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6580-L6625):
- Target URL: `{portal_base_url}/api/oauth/token`
- Header: `{"x-nous-refresh-token": refresh_token}` (proxy-friendly header required by some sandbox routing proxies)
- Form Body:
  - `grant_type`: `"refresh_token"`
  - `client_id`: `client_id` (e.g. `"hermes-cli"`)

### 6.3 Exact Client Cache Invalidation (#56889, #58894)
The auxiliary client cache maps tuple keys to live client instances.
In [`_refresh_nous_auxiliary_client`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8286):
- **Lookup Model Invariant**: Cache key construction must use `lookup_model` (the unresolved model passed during acquisition, typically `None`), rather than the wire-resolved model (such as `"google/gemini-3.6-flash"`). If the cache key used the resolved model name, the new client would be stored under a different key, leaving the stale client in the cache and producing continuous 401 failures (#56889).
- **Lookup Task Invariant**: For `auto` providers, `lookup_task` must be included in the cache key to prevent collision across task-specific fallback policies (#58894).
- The newly constructed OpenAI client overwrites the stale cache entry via [`_store_cached_client`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8220-L8240).

---

## 7. Peer-Rotated-Token Adoption (Anti-Stampede Dynamics)

### 7.1 Stampede Window in Concurrent Processes
Because Nous OAuth refresh tokens are single-use, if two concurrent worker processes or threads encounter a 401 and both attempt to POST the same refresh token to `/api/oauth/token`, the second request is rejected with `refresh_token_reused` or `invalid_grant`, triggering catastrophic session revocation.

### 7.2 Detection via `_already_rotated_by_peer`
Before initiating network token refresh, [`resolve_nous_runtime_credentials`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7100) checks whether a peer process has already completed rotation:

```python
def _already_rotated_by_peer(token: Any) -> bool:
    return bool(
        force_refresh
        and stale_access_token
        and isinstance(token, str)
        and token
        and token != stale_access_token
        and _nous_invoke_jwt_status(
            token,
            scope=state.get("scope"),
            expires_at=state.get("expires_at"),
        ) is None
    )
```

The adoption criteria require all of:
1. `force_refresh` is active.
2. `stale_access_token` was provided by the caller.
3. The token currently present on disk or in shared store differs from `stale_access_token`.
4. The token is non-empty and passes [`_nous_invoke_jwt_status`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L3218) with valid `inference:invoke` scope and remaining TTL.

When these conditions hold:
- Network refresh is canceled (`force_refresh = False`).
- The process immediately adopts the peer-rotated token without sending an HTTP request.
- If `stale_access_token` was not provided, peer adoption is bypassed and refresh proceeds.

### 7.3 Pool-Level Proactive and Reactive Peer Sync
In [`CredentialPool._refresh_entry_impl`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1840-L1910):
- **Pre-Refresh Sync**: Before calling `resolve_nous_runtime_credentials`, the pool invokes [`_sync_nous_entry_from_auth_store`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1446). If disk state contains fresh credentials, the entry is updated in-memory and refresh returns immediately.
- **Post-Failure Sync**: If `resolve_nous_runtime_credentials` raises an exception (such as a lock timeout or network glitch), the pool re-syncs from disk once more. If a peer completed rotation in the background while the failed process was waiting, the entry adopts the newly rotated token and clears error status.

---

## 8. Single-Use Refresh Locking and Concurrency Guarantees

### 8.1 Cross-Process Lock Hierarchy
Token refresh operations are serialized using operating-system file locks (`fcntl.flock` on Unix systems):
1. **Auth Store Lock** ([`_auth_store_lock`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L1250-L1275)): Guards `auth.json` reads and writes. Default timeout is 10.0 seconds (`AUTH_LOCK_TIMEOUT_SECONDS`).
2. **Shared Store Lock** ([`_nous_shared_store_lock`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6166-L6193)): Guards `nous_auth.json` reads, writes, and unlinks.

### 8.2 Lock Contention is Not Credential Failure
If lock acquisition fails due to contention:
- [`resolve_nous_runtime_credentials`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7065) or the lock context manager raises `TimeoutError`.
- In [`CredentialPool._refresh_entry_impl`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1875-L1885):
  - `TimeoutError` is caught explicitly.
  - The entry is returned unmodified (`res is entry`).
  - The entry's `last_status` remains unchanged (or `None`).
  - No error code or cooldown TTL is recorded.
- **Invariant**: Lock contention is a transient concurrency delay, not a credential defect. Marking an entry dead or exhausted due to a lock timeout would prematurely disable viable credentials during load spikes.

### 8.3 Atomic Pool Rebinds under Thread Lock (#77714)
Within a single process, [`CredentialPool`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L931) synchronizes all mutations via `self._lock` (a `threading.RLock`).
When quarantining or removing pool entries in [`_quarantine_entry`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1793):
- Rebinding `self._entries = [...]` executes strictly within `with self._lock:`.
- This ensures atomic read-modify-write semantics, preventing concurrent threads from losing newly added manual keys or corrupting entry indices.

---

## 9. Persistence-Before-Retry and Durability Invariants

### 9.1 The Refresh-Token Durability Sequence
Under single-use refresh token schemes, the token exchange endpoint consumes the old refresh token at the instant the response is generated. If the client crashes, loses power, or errors before persisting the replacement tokens, the entire authentication chain is permanently lost.

In [`resolve_nous_runtime_credentials`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7340-L7385), the execution order is strictly enforced:

```
[POST /api/oauth/token]
        |
        v
[Payload Parsed (access_token, refresh_token)]
        |
        v
[_persist_state("post_refresh_access_token")]   <-- MUST HAPPEN BEFORE ASSERTION
        |
        v
[_assert_nous_inference_jwt_usable]             <-- VALIDATION
        |
        v
[_select_nous_invoke_jwt]                       <-- RUNTIME SELECTION
```

### 9.2 Resilience to Claims Assertion Failure
If `_assert_nous_inference_jwt_usable` raises an `AuthError` (for example, if the upstream server issued a token missing the `inference:invoke` scope):
1. `_persist_state("post_refresh_access_token")` has ALREADY completed.
2. Both the new access token and the single-use replacement refresh token are durable in `auth.json` and mirrored to the shared store.
3. The session is preserved for future runs, preventing upstream reuse rejections.

### 9.3 Persistence Failure Semantics
If disk writing fails during `_persist_state` (e.g. disk full or read-only filesystem):
- The error is flagged as [`CREDENTIAL_PERSIST_FAILED_REASON`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L98) (`"credential_persist_failed"`).
- In [`_is_terminal_auth_failure`](file:///home/eins0fx/development/hermes-agent-port/agent/credential_pool.py#L1071-L1098), this is classified as terminal, transitioning the entry to `STATUS_DEAD` immediately because the pre-rotation token remaining on disk is already spent upstream.

---

## 10. Terminal vs. Relogin Classifications and Quarantine Protocols

### 10.1 Terminal Error Predicates
In [`_is_terminal_nous_refresh_error`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6341-L6348):
An error is classified as terminal if and only if:
- It is an instance of [`AuthError`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L70-L95).
- `exc.provider == "nous"`.
- `exc.relogin_required is True`.
- `exc.code` is a member of:
  `{"invalid_grant", "invalid_token", "refresh_token_reused"}`.

### 10.2 Non-Terminal Classifications
The following are non-terminal and do NOT trigger quarantine:
- Network connection drops, socket timeouts, DNS resolution failures.
- Upstream HTTP 500, 502, 503, 504 server responses.
- HTTP 429 Too Many Requests (rate limits).
- Transient errors with `relogin_required=False`.

### 10.3 Quarantine Protocol (`_quarantine_nous_oauth_state`)
When a terminal refresh error occurs, [`_quarantine_nous_oauth_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6391-L6475) executes:
1. **Forensic Logging with Redaction**:
   Emits a structured forensic warning log containing `client_id`, `agent_key_id`, error code, auth store path, and `refresh_token_fp` (the 12-character SHA-256 hex prefix of the refresh token). Raw tokens are never logged.
2. **Secret Purging**:
   Deletes the following keys from the provider dictionary:
   - `access_token`, `refresh_token`, `expires_at`, `expires_in`, `obtained_at`
   - `agent_key`, `agent_key_id`, `agent_key_expires_at`, `agent_key_expires_in`, `agent_key_reused`, `agent_key_obtained_at`
3. **Routing Metadata Preservation**:
   Preserves non-secret routing configuration: `portal_base_url`, `inference_base_url`, `client_id`.
4. **Structured Error Recording**:
   Populates `last_auth_error`:
   ```json
   {
     "provider": "nous",
     "code": "<error.code>",
     "message": "<error_message>",
     "reason": "<reason>",
     "relogin_required": true,
     "at": "<iso_timestamp>"
   }
   ```
5. **Shared Store Unlink**:
   Calls [`_clear_shared_nous_state`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6327-L6339) to delete `~/.hermes/shared/nous_auth.json`.
6. **Pool Entry Purge**:
   Calls [`_quarantine_nous_pool_entries`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L6476), removing all singleton-seeded (`device_code`) entries from the pool while preserving any manually configured API keys (`source="manual"`).

---

## 11. Provider Health Tracking and Circuit Breaking

### 11.1 Health State and TTL Windows
The auxiliary subsystem tracks provider operational health via [`_mark_provider_unhealthy`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4562-L4579) and [`_is_provider_unhealthy`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4581-L4600):
- **Unconfigured Provider**: 60.0s TTL (`_mark_provider_unhealthy("nous", 60, reason="unconfigured")`).
- **Payment / Billing / Credit Error**: 600.0s TTL (`_AUX_UNHEALTHY_TTL_SECONDS = 600.0`).
- **Rate Limit (429)**: Dynamically bound to `nous_rate_limit_remaining()` when available; defaults to 60.0s TTL.
- **Generic Transport Blip**: 60.0s TTL.

### 11.2 Circuit Breaker Gate
Before dispatching an auxiliary request, [`_is_provider_unhealthy("nous")`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L4581) checks `time.time() < unhealthy_until`:
- If unhealthy, the provider is immediately skipped in the fallback chain.
- Once `time.time() >= unhealthy_until`, the provider is automatically eligible for probe traffic without requiring manual resets.

---

## 12. Request Bounds, Timeouts, and Retry Budgets

### 12.1 Auxiliary Compression Timeout Floor (#54915)
In [`_effective_aux_timeout`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L9065-L9080):
- Constant: `_COMPRESSION_TIMEOUT_FLOOR_SECONDS = 300.0`
- **Floor Application**: When `task == "compression"` and caller passes `timeout=None`, the effective timeout is calculated as `max(config_timeout, 300.0)`. This ensures context compression involving large contexts has sufficient time to complete.
- **Caller Override**: If the caller supplies an explicit per-call timeout (e.g. `timeout=45.0`), the explicit deadline is strictly honored and the floor is bypassed.

### 12.2 Critical-Path Same-Provider Retry Suppression (#54465)
In [`_should_skip_same_provider_retry`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L5064-L5085):
- Constant: `_TIMEOUT_NO_RETRY_TASKS = frozenset({"compression", "vision"})`
- **Rule**: If a compression request exhausts its full timeout budget, `_should_skip_same_provider_retry` returns `True`.
- **Rationale**: Context compression sits on the interactive preflight path. Retrying against the same provider after a full timeout would double the user-visible stall (costing an additional 300 seconds) before falling back to alternative providers.
- **Exception**: Fast no-progress stream terminations (detected within the initial 60-second window with zero output) preserve the normal retry path.

### 12.3 Bound on 401 Recovery Retries
When an auxiliary call encounters a 401 and successfully rebuilds its client via [`_refresh_nous_auxiliary_client`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L8286), exactly **one** recovery retry is dispatched. If the retried call fails, recovery terminates immediately to prevent infinite retry loops.

---

## 13. Model and Base URL Selection Cascades

### 13.1 Auxiliary Model Cascades
- **Default Nous Model**: `_NOUS_MODEL = "google/gemini-3.6-flash"` ([`agent/auxiliary_client.py#L650`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L650)).
- **Dynamic Portal Recommendation**: [`get_nous_recommended_aux_model()`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L670-L710) queries the portal for recommended auxiliary models.
- **Policy Filtering**: Candidate models are validated against [`_nous_policy_blocks`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L720-L750), rejecting disallowed or deprecated model identifiers.

### 13.2 Base URL Canonicalization and Sanitization
- **Canonical Default Base URL**: `_NOUS_DEFAULT_BASE_URL = "https://inference-api.nousresearch.com/v1"` ([`agent/auxiliary_client.py#L648`](file:///home/eins0fx/development/hermes-agent-port/agent/auxiliary_client.py#L648)).
- **Trailing Slash Normalization**: Both pool properties and client builders strip trailing slashes (`rstrip("/")`).
- **Portal Base URL Security Healing**:
  In [`resolve_nous_runtime_credentials`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/auth.py#L7126-L7155), `portal_base_url` is validated against allowed hosts (`_NOUS_PORTAL_ALLOWED_HOSTS`). If a stored URL specifies an insecure scheme (such as `http://portal.nousresearch.com`) or unauthorized host, it is ignored and heals to the secure default `https://portal.nousresearch.com`. Loopback HTTP (`http://localhost`, `http://127.0.0.1`) is permitted for testing.
- **Environment Overrides**:
  - `NOUS_INFERENCE_BASE_URL`: Overrides the inference base URL at runtime without persisting to disk.
  - `HERMES_PORTAL_BASE_URL`, then `NOUS_PORTAL_BASE_URL`: Trusted operator overrides bypass the network-value allowlist and become the effective Portal URL. The direct runtime resolver writes that effective URL back through `_save_provider_state_to_source`, so unlike the inference override it can persist in `auth.json`.

---

## 14. Deterministic Verification Matrix (Golden Corpus Mapping)

Every scenario and invariant specified in this document is verified by the deterministic test generator [`rust/tools/gen_nous_oauth_recovery_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_nous_oauth_recovery_goldens.py) and checked into [`rust/tools/nous-oauth-recovery-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/nous-oauth-recovery-goldens.json).

| Section | Domain Name | Test Case Count | Key Validations and Invariants Covered |
| :---: | :--- | :---: | :--- |
| **§ 2** | `profile_and_root_ownership` | 10 | Profile-to-root resolution tracking, root write-through without profile pollution (#74339), profile-owned direct updates, `set_active=False` background preservation vs direct runtime active path, shared store `nous_auth.json` inclusion/exclusion of `agent_key`, atomic `0o600` creation. |
| **§ 3** | `singleton_seeding_and_idempotence` | 6 | Singleton device-code seeding, 6-character minted hex id, label defaulting to `device_code` unless preserving existing row label, in-place updates, stability under repeat persists, manual entry preservation, source partitioning. |
| **§ 4** | `access_token_and_runtime_key_selection` | 10 | Precedence of `agent_key` over `access_token`, fallback on expired `agent_key`, empty string return when both invalid, URL selection between `inference_base_url` and `base_url`, scope parsing from parameter, claims, and `scp`. |
| **§ 5** | `expiry_and_skew_semantics` | 8 | 120-second skew threshold, token expiry at `exp == now + 120`, validity at `exp > now + 120`, missing `exp` fallback to `expires_at`, non-JWT rejection, missing `inference:invoke` scope rejection. |
| **§ 6** | `forced_401_refresh_and_client_rebuild` | 6 | Portal token exchange dispatch, `x-nous-refresh-token` header attachment, grant and client ID payload verification, cache key invalidation using `lookup_model` and `lookup_task` threaded at tuple index 7 (#56889, #58894). |
| **§ 7** | `peer_rotated_token_adoption` | 5 | Anti-stampede detection via `_already_rotated_by_peer`, refresh bypass when peer rotated, proactive pre-refresh sync adopting fresh peer token, reactive post-failure sync. |
| **§ 8** | `single_use_refresh_locking_expectations` | 5 | Refresh execution serialized under `_auth_store_lock`, lock timeout treated as transient busy without modifying entry status, atomic pool rebind under thread lock (#77714). |
| **§ 9** | `persistence_before_retry` | 4 | `_persist_state("post_refresh_access_token")` completes strictly before `_assert_nous_inference_jwt_usable`, refresh token preserved on assertion failure, shared store `nous_auth.json` updated before return, `CREDENTIAL_PERSIST_FAILED_REASON` handling. |
| **§ 10** | `terminal_and_relogin_classifications` | 11 | Terminal classification for `invalid_grant`, `invalid_token`, and `refresh_token_reused`, non-terminal classification for network/5xx/429, secret clearing, forensic logging with SHA-256 fingerprint prefix, shared store `nous_auth.json` unlink. |
| **§ 11** | `provider_health_tracking` | 6 | 60s TTL for unconfigured providers, 600s TTL for payment/credit errors, rate limit TTL calculation, circuit breaker bypass when expired. |
| **§ 12** | `request_bounds_and_retry_limits` | 6 | 300s compression timeout floor (#54915), caller explicit timeout override, suppression of full-budget same-provider retries (#54465), single 401 recovery retry ceiling. |
| **§ 13** | `model_and_base_url_selection` | 10 | Default model `google/gemini-3.6-flash`, dynamic recommendation query, policy filtering, trailing slash stripping, insecure URL healing, and distinct runtime versus persisted environment overrides. |
| **Total** | **Authoritative Contract Corpus** | **87** | **All 12 sections verified against source implementation with zero discrepancy.** |
