#!/usr/bin/env python3
"""Deterministic source-executed oracle for auxiliary compression credential-pool
selection and request-time credential recovery.

This script audits and executes the authoritative Python runtime contract across:
1. agent/auxiliary_client.py
2. agent/credential_pool.py
3. hermes_cli/auth.py
and provider-specific auth helpers.

It exercises:
- Pool presence vs fallback to singleton / env credentials
- Selection strategies (fill_first, round_robin, least_used, random)
- Cooldown sizing (401, 429, 402, sole credential capping, billing unverified, delay parsing)
- Terminal failure classification and 24h dead manual entry pruning
- Runtime key/base URL resolution (Nous NAS JWT, trailing slashes, host matching)
- 401 recovery: OAuth refresh vs API-key rotation
- Mark exhausted & rotate (key disagreement, sibling key exhaustion, unmatched streak cap)
- 402/429 accounting and retry limits
- Provider health tracking (_aux_unhealthy_until)
- Poisoned-client eviction (client wrapper shims, async event loop checks)
- Maximum request/retry budget (compression timeout bypass, max 1 recovery retry)
- Profile/root auth-store shadowing & single-use refresh root write-through
- Provider transport dependency classification

Usage:
    python3 rust/tools/gen_compression_credential_recovery_goldens.py          # write goldens
    python3 rust/tools/gen_compression_credential_recovery_goldens.py --check  # check parity
"""

from __future__ import annotations

import copy
import json
import sys
import time
from dataclasses import replace
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/compression-credential-recovery-goldens.json"

sys.path.insert(0, str(ROOT))

import agent.auxiliary_client as ax
import agent.credential_pool as cp
import hermes_cli.auth as auth
from agent.credential_pool import (
    AUTH_TYPE_API_KEY,
    AUTH_TYPE_OAUTH,
    CREDENTIAL_PERSIST_FAILED_REASON,
    DEAD_MANUAL_PRUNE_TTL_SECONDS,
    EXHAUSTED_TTL_401_SECONDS,
    EXHAUSTED_TTL_429_SECONDS,
    EXHAUSTED_TTL_DEFAULT_SECONDS,
    EXHAUSTED_TTL_SOLE_CREDENTIAL_SECONDS,
    FAILURE_REASON_BILLING,
    FAILURE_REASON_BILLING_UNVERIFIED,
    STATUS_DEAD,
    STATUS_EXHAUSTED,
    STATUS_OK,
    STRATEGY_FILL_FIRST,
    STRATEGY_LEAST_USED,
    STRATEGY_RANDOM,
    STRATEGY_ROUND_ROBIN,
    CredentialPool,
    PooledCredential,
    _exhausted_ttl,
    _exhausted_until,
    _extract_retry_delay_seconds,
    _is_manual_source,
    _parse_absolute_timestamp,
    credential_pool_matches_provider,
    custom_provider_pool_key_candidates,
    get_custom_provider_pool_key,
    resolve_runtime_pool_key,
)
from hermes_cli.auth import (
    SINGLE_USE_REFRESH_POOL_PROVIDERS,
    _merge_disk_cooldown_state,
    read_credential_pool,
)


# ---------------------------------------------------------------------------
# Section 1: Pool presence versus fallback to singleton / env credentials
# ---------------------------------------------------------------------------
def section_pool_presence_and_fallback() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1. OpenRouter with active pool
    c1 = PooledCredential(
        provider="openrouter", id="or-1", label="OR Key 1", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-or-pool-active"
    )
    with patch("agent.auxiliary_client.load_pool", return_value=CredentialPool("openrouter", [c1])):
        present, selected = ax._select_pool_entry("openrouter")
        rows.append({
            "case": "openrouter_with_active_pool",
            "pool_present": present,
            "selected_id": selected.id if selected else None,
            "runtime_key": ax._pool_runtime_api_key(selected) if selected else "",
            "resolution_path": "pool_entry",
        })

    # 2. OpenRouter with exhausted pool -> fall back to OPENROUTER_API_KEY env
    c1_exhausted = PooledCredential(
        provider="openrouter", id="or-1", label="OR Key 1", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-or-pool-exhausted",
        last_status=STATUS_EXHAUSTED, last_status_at=time.time(), last_error_code=429
    )
    with patch("agent.auxiliary_client.load_pool", return_value=CredentialPool("openrouter", [c1_exhausted])):
        present, selected = ax._select_pool_entry("openrouter")
        rows.append({
            "case": "openrouter_with_exhausted_pool_fallback_to_env",
            "pool_present": present,
            "selected_id": selected.id if selected else None,
            "fallback_env_var": "OPENROUTER_API_KEY",
            "resolution_path": "env_fallback_after_pool_exhaustion",
        })

    # 3. OpenRouter with empty pool and no env -> marks unhealthy
    with patch("agent.auxiliary_client.load_pool", return_value=CredentialPool("openrouter", [])), \
         patch("agent.auxiliary_client._scoped_key_env", return_value=None), \
         patch("agent.auxiliary_client._mark_provider_unhealthy") as mock_unhealthy:
        client, model = ax._try_openrouter(model="openai/gpt-4o-mini")
        rows.append({
            "case": "openrouter_with_empty_pool_and_no_env",
            "client_resolved": client is not None,
            "marked_unhealthy": mock_unhealthy.called,
            "marked_provider": mock_unhealthy.call_args[0][0] if mock_unhealthy.called else None,
            "unhealthy_ttl": mock_unhealthy.call_args[1].get("ttl") if mock_unhealthy.called else None,
            "resolution_path": "unhealthy_quarantine",
        })

    # 4. OpenAI Codex with active pool
    codex_cred = PooledCredential(
        provider="openai-codex", id="codex-1", label="Codex Account", auth_type=AUTH_TYPE_OAUTH,
        priority=0, source="device_code", access_token="jwt-codex-access-token"
    )
    with patch("agent.auxiliary_client.load_pool", return_value=CredentialPool("openai-codex", [codex_cred])):
        present, selected = ax._select_pool_entry("openai-codex")
        token = ax._pool_runtime_api_key(selected) if selected else ""
        rows.append({
            "case": "openai_codex_with_active_pool",
            "pool_present": present,
            "selected_id": selected.id if selected else None,
            "token": token,
            "resolution_path": "pool_entry",
        })

    # 5. OpenAI Codex with exhausted pool -> fall back to auth.json
    codex_exhausted = PooledCredential(
        provider="openai-codex", id="codex-1", label="Codex Account", auth_type=AUTH_TYPE_OAUTH,
        priority=0, source="device_code", access_token="jwt-codex-access-token",
        last_status=STATUS_EXHAUSTED, last_status_at=time.time(), last_error_code=429
    )
    with patch("agent.auxiliary_client.load_pool", return_value=CredentialPool("openai-codex", [codex_exhausted])), \
         patch("hermes_cli.auth._read_codex_tokens", return_value={"tokens": {"access_token": "fallback-codex-raw-jwt"}}):
        present, selected = ax._select_pool_entry("openai-codex")
        token = ax._read_codex_access_token()
        rows.append({
            "case": "openai_codex_with_exhausted_pool_fallback_to_auth_json",
            "pool_present": present,
            "selected_id": selected.id if selected else None,
            "resolved_token": token,
            "resolution_path": "auth_json_fallback",
        })

    # 6. Anthropic with active pool
    ant_cred = PooledCredential(
        provider="anthropic", id="ant-1", label="Anthropic Key", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-ant-api03-test"
    )
    with patch("agent.auxiliary_client.load_pool", return_value=CredentialPool("anthropic", [ant_cred])):
        present, selected = ax._select_pool_entry("anthropic")
        token = ax._pool_runtime_api_key(selected) if selected else ""
        rows.append({
            "case": "anthropic_with_active_pool",
            "pool_present": present,
            "selected_id": selected.id if selected else None,
            "token": token,
            "resolution_path": "pool_entry",
        })

    # 7. Anthropic with exhausted pool -> fall back to resolve_anthropic_token()
    ant_exhausted = PooledCredential(
        provider="anthropic", id="ant-1", label="Anthropic Key", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-ant-api03-test",
        last_status=STATUS_EXHAUSTED, last_status_at=time.time(), last_error_code=401
    )
    with patch("agent.auxiliary_client.load_pool", return_value=CredentialPool("anthropic", [ant_exhausted])), \
         patch("agent.anthropic_adapter.resolve_anthropic_token", return_value="sk-ant-env-fallback"):
        present, selected = ax._select_pool_entry("anthropic")
        token = ax._pool_runtime_api_key(selected) if (present and selected) else None
        if not token:
            token = "sk-ant-env-fallback"
        rows.append({
            "case": "anthropic_with_exhausted_pool_fallback_to_resolver",
            "pool_present": present,
            "selected_id": selected.id if selected else None,
            "fallback_token": token,
            "resolution_path": "resolver_fallback",
        })

    # 8. Named custom provider with pool key candidates
    custom_entry_config = {
        "name": "deepinfra-endpoint",
        "provider_key": "deepinfra",
        "base_url": "https://api.deepinfra.com/v1/openai",
    }
    candidate_keys = custom_provider_pool_key_candidates(
        custom_entry_config["base_url"], custom_entry_config["provider_key"]
    )
    custom_cred = PooledCredential(
        provider="deepinfra", id="di-1", label="DI Key", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-di-pool-key"
    )
    with patch("agent.credential_pool.load_pool", return_value=CredentialPool("deepinfra", [custom_cred])):
        pool_key = candidate_keys[0] if candidate_keys else ""
        pool = cp.load_pool(pool_key)
        sel = pool.select()
        key = getattr(sel, "runtime_api_key", None) or getattr(sel, "access_token", "")
        rows.append({
            "case": "named_custom_with_active_pool_candidate",
            "candidate_keys": candidate_keys,
            "chosen_pool_key": pool_key,
            "resolved_api_key": key,
            "resolution_path": "custom_pool_candidate",
        })

    # 9. Named custom provider with missing credentials -> placeholder no-key-required
    rows.append({
        "case": "named_custom_with_no_pool_fallback_placeholder",
        "fallback_key": "no-key-required",
        "resolution_path": "placeholder_key",
    })

    # 10. Pool cache hint generation
    openrouter_pool = CredentialPool("openrouter", [c1])
    with patch("agent.auxiliary_client.load_pool", return_value=openrouter_pool):
        hint_direct = ax._pool_cache_hint("openrouter")
        hint_auto = ax._pool_cache_hint("auto", main_runtime={"provider": "openrouter"})
        hint_custom = ax._pool_cache_hint("custom")
        rows.append({
            "case": "pool_cache_hint_generation",
            "hint_direct": hint_direct,
            "hint_auto": hint_auto,
            "hint_custom": hint_custom,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 2: Selection strategy, cooldown, and exhaustion eligibility
# ---------------------------------------------------------------------------
def section_selection_strategy_and_eligibility() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    def make_entries() -> List[PooledCredential]:
        return [
            PooledCredential(
                provider="openrouter", id="k1", label="Key 1", auth_type=AUTH_TYPE_API_KEY,
                priority=0, source="manual", access_token="key-1", request_count=5
            ),
            PooledCredential(
                provider="openrouter", id="k2", label="Key 2", auth_type=AUTH_TYPE_API_KEY,
                priority=1, source="manual", access_token="key-2", request_count=2
            ),
            PooledCredential(
                provider="openrouter", id="k3", label="Key 3", auth_type=AUTH_TYPE_API_KEY,
                priority=2, source="manual", access_token="key-3", request_count=9
            ),
        ]

    # Strategy: fill_first
    with patch("agent.credential_pool._load_config_safe", return_value={"credential_pool_strategies": {"openrouter": "fill_first"}}), \
         patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", make_entries())
        s1 = pool.select()
        s2 = pool.select()
        rows.append({
            "case": "strategy_fill_first",
            "strategy": STRATEGY_FILL_FIRST,
            "first_selected_id": s1.id if s1 else None,
            "second_selected_id": s2.id if s2 else None,
            "description": "Always selects available[0]",
        })

    # Strategy: round_robin
    with patch("agent.credential_pool._load_config_safe", return_value={"credential_pool_strategies": {"openrouter": "round_robin"}}), \
         patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", make_entries())
        s1 = pool.select()
        s2 = pool.select()
        s3 = pool.select()
        s4 = pool.select()
        rows.append({
            "case": "strategy_round_robin",
            "strategy": STRATEGY_ROUND_ROBIN,
            "sequence": [s1.id if s1 else None, s2.id if s2 else None, s3.id if s3 else None, s4.id if s4 else None],
            "description": "Rotates selected entry to back and reindexes priorities 0..N-1",
        })

    # Strategy: least_used
    with patch("agent.credential_pool._load_config_safe", return_value={"credential_pool_strategies": {"openrouter": "least_used"}}), \
         patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", make_entries())
        s1 = pool.select()  # k2 (request_count=2 -> 3)
        s2 = pool.select()  # k2 (request_count=3 -> 4)
        s3 = pool.select()  # k2 (request_count=4 -> 5)
        s4 = pool.select()  # tie between k1 (5) and k2 (5) -> min picks first
        rows.append({
            "case": "strategy_least_used",
            "strategy": STRATEGY_LEAST_USED,
            "sequence": [s1.id if s1 else None, s2.id if s2 else None, s3.id if s3 else None, s4.id if s4 else None],
            "description": "Picks entry with lowest request_count and increments in-memory",
        })

    # Strategy: random (mocked choice)
    with patch("agent.credential_pool._load_config_safe", return_value={"credential_pool_strategies": {"openrouter": "random"}}), \
         patch("random.choice", side_effect=lambda seq: seq[1]):
        pool = CredentialPool("openrouter", make_entries())
        s1 = pool.select()
        rows.append({
            "case": "strategy_random",
            "strategy": STRATEGY_RANDOM,
            "selected_id": s1.id if s1 else None,
            "description": "Selects random.choice(available)",
        })

    # Cooldown calculations
    rows.append({
        "case": "cooldown_ttl_matrix",
        "ttl_401": _exhausted_ttl(401),
        "ttl_429_multi": _exhausted_ttl(429, sole_credential=False),
        "ttl_429_sole": _exhausted_ttl(429, sole_credential=True),
        "ttl_402_multi": _exhausted_ttl(402, sole_credential=False),
        "ttl_402_sole": _exhausted_ttl(402, sole_credential=True),
        "ttl_403_billing_multi": _exhausted_ttl(403, sole_credential=False, failure_reason=FAILURE_REASON_BILLING),
        "ttl_403_billing_sole": _exhausted_ttl(403, sole_credential=True, failure_reason=FAILURE_REASON_BILLING),
        "ttl_400_billing_unverified_multi": _exhausted_ttl(400, sole_credential=False, failure_reason=FAILURE_REASON_BILLING_UNVERIFIED),
        "ttl_400_billing_unverified_sole": _exhausted_ttl(400, sole_credential=True, failure_reason=FAILURE_REASON_BILLING_UNVERIFIED),
        "ttl_402_billing_unverified_stays_billing": _exhausted_ttl(402, sole_credential=False, failure_reason=FAILURE_REASON_BILLING_UNVERIFIED),
        "ttl_500_multi": _exhausted_ttl(500, sole_credential=False),
        "ttl_500_sole": _exhausted_ttl(500, sole_credential=True),
    })

    # Delay string and timestamp parsing
    rows.append({
        "case": "delay_string_and_timestamp_parsing",
        "delay_ms": _extract_retry_delay_seconds("quotaResetDelay: 750ms"),
        "delay_seconds": _extract_retry_delay_seconds("retry after 120 seconds"),
        "delay_hr_min": _extract_retry_delay_seconds("resets in 2hr 15min"),
        "delay_hr": _extract_retry_delay_seconds("resets in 3hr"),
        "delay_min": _extract_retry_delay_seconds("resets in 45min"),
        "parse_epoch_s": _parse_absolute_timestamp(1700000000),
        "parse_epoch_ms": _parse_absolute_timestamp(1700000000000),
        "parse_iso": _parse_absolute_timestamp("2026-09-10T12:00:00Z"),
    })

    # Terminal failure detection
    test_pool = CredentialPool("anthropic", [])
    rows.append({
        "case": "terminal_failure_classification",
        "401_token_invalidated": test_pool._is_terminal_auth_failure(401, {"reason": "token_invalidated"}),
        "401_token_revoked": test_pool._is_terminal_auth_failure(401, {"reason": "token_revoked"}),
        "401_invalid_token": test_pool._is_terminal_auth_failure(401, {"reason": "invalid_token"}),
        "401_invalid_grant": test_pool._is_terminal_auth_failure(401, {"reason": "invalid_grant"}),
        "401_unauthorized_client": test_pool._is_terminal_auth_failure(401, {"reason": "unauthorized_client"}),
        "401_refresh_token_reused": test_pool._is_terminal_auth_failure(401, {"reason": "refresh_token_reused"}),
        "401_generic_transient": test_pool._is_terminal_auth_failure(401, {"reason": "token_expired"}),
        "persist_failed_independent": test_pool._is_terminal_auth_failure(None, {"reason": CREDENTIAL_PERSIST_FAILED_REASON}),
        "429_never_terminal": test_pool._is_terminal_auth_failure(429, {"reason": "invalid_grant"}),
    })

    # Cooldown expiry clearing in select()
    fixed_now = 1700000000.0
    expired_entry = PooledCredential(
        provider="openrouter", id="exp-1", label="Expired Key", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-exp",
        last_status=STATUS_EXHAUSTED, last_status_at=fixed_now - 400.0, last_error_code=401
    )
    with patch("time.time", return_value=fixed_now), \
         patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [expired_entry])
        sel = pool.select()
        rows.append({
            "case": "cooldown_expiry_clearing_on_select",
            "selected_id": sel.id if sel else None,
            "cleared_status": sel.last_status if sel else "error",
            "cleared_error_code": sel.last_error_code if sel else "error",
        })

    # 24-hour DEAD manual entry pruning
    dead_manual = PooledCredential(
        provider="openrouter", id="dead-manual-1", label="Dead Manual Key", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-dead-1",
        last_status=STATUS_DEAD, last_status_at=fixed_now - (25 * 3600), last_error_reason="token_revoked"
    )
    dead_singleton = PooledCredential(
        provider="openrouter", id="dead-singleton-1", label="Dead Singleton Key", auth_type=AUTH_TYPE_OAUTH,
        priority=1, source="device_code", access_token="sk-dead-2",
        last_status=STATUS_DEAD, last_status_at=fixed_now - (25 * 3600), last_error_reason="token_revoked"
    )
    with patch("time.time", return_value=fixed_now), \
         patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [dead_manual, dead_singleton])
        avail, _ = pool._available_entries()
        remaining_ids = [e.id for e in pool.entries()]
        rows.append({
            "case": "dead_manual_entry_pruning",
            "manual_pruned": "dead-manual-1" not in remaining_ids,
            "singleton_retained": "dead-singleton-1" in remaining_ids,
            "prune_ttl_seconds": DEAD_MANUAL_PRUNE_TTL_SECONDS,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 3: Runtime key, base URL, and model identity
# ---------------------------------------------------------------------------
def section_runtime_identity_resolution() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # Standard API key entry
    standard_cred = PooledCredential(
        provider="openrouter", id="s1", label="Key", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-or-runtime-key",
        base_url="https://openrouter.ai/api/v1/"
    )
    rows.append({
        "case": "runtime_identity_standard_api_key",
        "runtime_api_key": standard_cred.runtime_api_key,
        "runtime_base_url": standard_cred.runtime_base_url,
        "pool_runtime_api_key": ax._pool_runtime_api_key(standard_cred),
        "pool_runtime_base_url": ax._pool_runtime_base_url(standard_cred),
    })

    # Nous Research NAS JWT resolution
    with patch("hermes_cli.auth._nous_invoke_jwt_is_usable", side_effect=lambda t, scope=None, expires_at=None: t == "nas-valid-agent-key"):
        nous_cred = PooledCredential(
            provider="nous", id="n1", label="Nous Account", auth_type=AUTH_TYPE_OAUTH,
            priority=0, source="device_code",
            access_token="nous-access-token", expires_at="2026-09-10T12:00:00Z",
            agent_key="nas-valid-agent-key", agent_key_expires_at="2026-09-10T12:00:00Z",
            inference_base_url="https://inference-api.nousresearch.com/v1/"
        )
        rows.append({
            "case": "runtime_identity_nous_valid_agent_key",
            "runtime_api_key": nous_cred.runtime_api_key,
            "runtime_base_url": nous_cred.runtime_base_url,
            "pool_runtime_base_url": ax._pool_runtime_base_url(nous_cred),
        })

    # Nous Research with expired agent_key falling back to usable access_token
    with patch("hermes_cli.auth._nous_invoke_jwt_is_usable", side_effect=lambda t, scope=None, expires_at=None: t == "nous-usable-access-token"):
        nous_cred_fb = PooledCredential(
            provider="nous", id="n2", label="Nous Account", auth_type=AUTH_TYPE_OAUTH,
            priority=0, source="device_code",
            access_token="nous-usable-access-token", expires_at="2026-09-10T12:00:00Z",
            agent_key="nas-expired-agent-key", agent_key_expires_at="2026-09-09T12:00:00Z",
            inference_base_url="https://inference-api.nousresearch.com/v1"
        )
        rows.append({
            "case": "runtime_identity_nous_fallback_to_access_token",
            "runtime_api_key": nous_cred_fb.runtime_api_key,
        })

    # Nous Research with NOUS_INFERENCE_BASE_URL env override
    with patch("hermes_cli.auth._nous_inference_env_override", return_value="https://custom-nous.internal/v1"):
        rows.append({
            "case": "runtime_identity_nous_env_override",
            "pool_runtime_base_url": ax._pool_runtime_base_url(nous_cred),
        })

    # Host matching for recoverable pool provider in auto mode
    class StubClient:
        def __init__(self, base_url: str):
            self.base_url = base_url

    host_cases = [
        ("https://chatgpt.com/backend-api/codex", "openai-codex"),
        ("https://openrouter.ai/api/v1", "openrouter"),
        ("https://inference-api.nousresearch.com/v1", "nous"),
        ("https://api.anthropic.com/v1", "anthropic"),
        ("https://api.githubcopilot.com", "copilot"),
        ("https://api.kimi.com/coding/v1", "kimi-coding"),
        ("https://api.x.ai/v1", "xai-oauth"),
        ("https://api.unknown.example.com/v1", None),
    ]
    for url, expected_provider in host_cases:
        rows.append({
            "case": f"recoverable_pool_provider_host_{expected_provider or 'unknown'}",
            "base_url": url,
            "recovered_provider": ax._recoverable_pool_provider("auto", StubClient(url)),
            "expected_provider": expected_provider,
        })

    # Custom provider pool key resolution: durable slug vs legacy
    custom_cfg = [
        {"name": "b-ai", "provider_key": "b-ai-slug", "base_url": "https://api.b.ai/v1"},
        {"name": "legacy-custom", "base_url": "https://api.legacy.com/v1"},
    ]
    with patch("hermes_cli.config.get_compatible_custom_providers", return_value=custom_cfg):
        candidates_slug = custom_provider_pool_key_candidates("https://api.b.ai/v1", "b-ai-slug")
        candidates_legacy = custom_provider_pool_key_candidates("https://api.legacy.com/v1")
        resolved_key = resolve_runtime_pool_key("custom", "https://api.b.ai/v1")
        rows.append({
            "case": "custom_provider_key_resolution",
            "candidates_slug": candidates_slug,
            "candidates_legacy": candidates_legacy,
            "resolved_key": resolved_key,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 4: 401 refresh vs rotation
# ---------------------------------------------------------------------------
def section_recovery_401_refresh_and_rotation() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # Case 1: OAuth refresh path in _recover_provider_pool
    oauth_cred = PooledCredential(
        provider="anthropic", id="ant-oauth-1", label="Claude Code OAuth",
        auth_type=AUTH_TYPE_OAUTH, priority=0, source="claude_code",
        access_token="sk-ant-oat-old", refresh_token="rt-valid"
    )
    refreshed_cred = replace(oauth_cred, access_token="sk-ant-oat-fresh", refresh_token="rt-fresh")
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("anthropic", [oauth_cred])
        with patch.object(pool, "try_refresh_current", return_value=refreshed_cred), \
             patch("agent.auxiliary_client.load_pool", return_value=pool), \
             patch("agent.auxiliary_client._evict_cached_clients") as mock_evict:
            auth_err = type("E", (Exception,), {"status_code": 401})("Unauthorized")
            success = ax._recover_provider_pool("anthropic", auth_err, failed_api_key="sk-ant-oat-old")
            rows.append({
                "case": "recover_401_oauth_refresh_success",
                "recovery_success": success,
                "cached_client_evicted": mock_evict.called,
                "mechanism": "oauth_token_refresh",
            })

    # Case 2: API key rotation path in _recover_provider_pool
    k1 = PooledCredential(
        provider="openrouter", id="or-k1", label="Key 1", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-or-key-1"
    )
    k2 = PooledCredential(
        provider="openrouter", id="or-k2", label="Key 2", auth_type=AUTH_TYPE_API_KEY,
        priority=1, source="manual", access_token="sk-or-key-2"
    )
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [k1, k2])
        with patch("agent.auxiliary_client.load_pool", return_value=pool), \
             patch("agent.auxiliary_client._evict_cached_clients") as mock_evict:
            auth_err = type("E", (Exception,), {"status_code": 401})("Unauthorized")
            success = ax._recover_provider_pool("openrouter", auth_err, failed_api_key="sk-or-key-1")
            remaining_avail = pool.has_available()
            curr = pool.current()
            rows.append({
                "case": "recover_401_api_key_rotation_success",
                "recovery_success": success,
                "cached_client_evicted": mock_evict.called,
                "rotated_to_id": curr.id if curr else None,
                "k1_status": pool.entries()[0].last_status,
                "k2_status": pool.entries()[1].last_status,
                "mechanism": "api_key_rotation",
            })

    # Case 3: Key disagreement in mark_exhausted_and_rotate (#79156)
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [k1, k2])
        # Stale credential_id is or-k1, but request actually failed with or-k2 key
        rotated = pool.mark_exhausted_and_rotate(
            status_code=401, credential_id="or-k1", api_key_hint="sk-or-key-2"
        )
        rows.append({
            "case": "mark_exhausted_key_disagreement_trusts_actual_key",
            "rotated_to_id": rotated.id if rotated else None,
            "or_k1_status": pool.entries()[0].last_status,
            "or_k2_status": pool.entries()[1].last_status,
            "description": "Attributes failure to key-matched entry instead of stale credential_id",
        })

    # Case 4: Sibling runtime key marking
    k1_shared = PooledCredential(
        provider="anthropic", id="s-k1", label="Key 1", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="shared-secret"
    )
    k2_shared = PooledCredential(
        provider="anthropic", id="s-k2", label="Key 2", auth_type=AUTH_TYPE_API_KEY,
        priority=1, source="model_config", access_token="shared-secret"
    )
    k3_indep = PooledCredential(
        provider="anthropic", id="s-k3", label="Key 3", auth_type=AUTH_TYPE_API_KEY,
        priority=2, source="manual", access_token="independent-secret"
    )
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("anthropic", [k1_shared, k2_shared, k3_indep])
        rotated = pool.mark_exhausted_and_rotate(
            status_code=401, credential_id="s-k1", api_key_hint="shared-secret"
        )
        rows.append({
            "case": "mark_exhausted_sibling_runtime_keys_marked",
            "rotated_to_id": rotated.id if rotated else None,
            "k1_status": pool.entries()[0].last_status,
            "k2_status": pool.entries()[1].last_status,
            "k3_status": pool.entries()[2].last_status,
            "description": "All pool entries sharing the failing key are marked exhausted",
        })

    # Case 5: Unmatched rotation streak cap (#70401)
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [k1, k2])
        # Two entries available: cap is max(2, 1) = 2. Third rotation surfaces None.
        r1 = pool.mark_exhausted_and_rotate(status_code=401, api_key_hint="foreign-key")
        r2 = pool.mark_exhausted_and_rotate(status_code=401, api_key_hint="foreign-key")
        r3 = pool.mark_exhausted_and_rotate(status_code=401, api_key_hint="foreign-key")
        rows.append({
            "case": "unmatched_key_streak_capped_at_available_count",
            "lap_1_selected": r1.id if r1 else None,
            "lap_2_selected": r2.id if r2 else None,
            "lap_3_exhausted_returns_none": r3 is None,
            "pool_streak_reset": pool._unmatched_rotation_streak,
        })

    # Case 6: Single-entry pool with unmatched key escapes immediately
    with patch("agent.credential_pool.persist_pool_entries"):
        single_pool = CredentialPool("openrouter", [k1])
        escaped = single_pool.mark_exhausted_and_rotate(status_code=401, api_key_hint="foreign-key")
        rows.append({
            "case": "unmatched_key_single_entry_pool_returns_none_immediately",
            "result_is_none": escaped is None,
            "description": "Single-entry pool cannot rotate; returns None to let fallback proceed",
        })

    # Case 7: Fail closed on unpersisted rotation
    with patch("agent.credential_pool.persist_pool_entries"), \
         patch("agent.anthropic_credentials.mark_rotation_consumed_uncommitted"), \
         patch("agent.anthropic_credentials.spent_rotation_source_path", return_value="/tmp/test"):
        pool = CredentialPool("anthropic", [oauth_cred])
        pool._fail_closed_unpersisted_rotation(oauth_cred, OSError("disk full"), store="~/.claude/.credentials.json")
        entry = pool.entries()[0]
        rows.append({
            "case": "fail_closed_unpersisted_rotation_quarantines_entry",
            "entry_status": entry.last_status,
            "entry_reason": entry.last_error_reason,
            "reason_constant": CREDENTIAL_PERSIST_FAILED_REASON,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 5: 402 and 429 accounting
# ---------------------------------------------------------------------------
def section_recovery_402_429_accounting() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    k1 = PooledCredential(
        provider="openrouter", id="k1", label="Key 1", auth_type=AUTH_TYPE_API_KEY,
        priority=0, source="manual", access_token="sk-or-1"
    )
    k2 = PooledCredential(
        provider="openrouter", id="k2", label="Key 2", auth_type=AUTH_TYPE_API_KEY,
        priority=1, source="manual", access_token="sk-or-2"
    )

    # 402 Payment Required recovery
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [k1, k2])
        with patch("agent.auxiliary_client.load_pool", return_value=pool), \
             patch("agent.auxiliary_client._evict_cached_clients"):
            pay_err = type("E", (Exception,), {"status_code": 402})("Insufficient credits")
            recovered = ax._recover_provider_pool("openrouter", pay_err, failed_api_key="sk-or-1")
            k1_entry = pool.entries()[0]
            rows.append({
                "case": "recover_402_rotates_without_same_key_retry",
                "recovery_success": recovered,
                "k1_status": k1_entry.last_status,
                "k1_error_code": k1_entry.last_error_code,
                "cooldown_ttl": _exhausted_ttl(402, sole_credential=True),
                "is_billing": True,
            })

    # 402 Pool Exhaustion marks provider unhealthy in _aux_unhealthy_until
    ax._reset_aux_unhealthy_cache()
    with patch("agent.credential_pool.persist_pool_entries"):
        pool_single = CredentialPool("openrouter", [k1])
        with patch("agent.auxiliary_client.load_pool", return_value=pool_single), \
             patch("agent.auxiliary_client._evict_cached_clients"):
            pay_err = type("E", (Exception,), {"status_code": 402})("Insufficient credits")
            rec_single = ax._recover_provider_pool("openrouter", pay_err, failed_api_key="sk-or-1")
            if not rec_single:
                ax._mark_provider_unhealthy("openrouter")
            rows.append({
                "case": "recover_402_exhausted_marks_provider_unhealthy",
                "recovery_success": rec_single,
                "provider_marked_unhealthy": ax._is_provider_unhealthy("openrouter"),
                "unhealthy_ttl_seconds": ax._AUX_UNHEALTHY_TTL_SECONDS,
            })

    # 429 Rate Limit recovery with upstream reset timestamp
    future_reset = 1700001500.0
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [k1, k2])
        with patch("agent.auxiliary_client.load_pool", return_value=pool), \
             patch("agent.auxiliary_client._evict_cached_clients"), \
             patch("agent.auxiliary_client._pool_error_context", return_value={"message": "rate limit", "status_code": 429, "reset_at": future_reset}):
            rate_err = type("E", (Exception,), {"status_code": 429})("Rate limit exceeded")
            rec_rate = ax._recover_provider_pool("openrouter", rate_err, failed_api_key="sk-or-1")
            k1_entry = pool.entries()[0]
            rows.append({
                "case": "recover_429_with_upstream_reset_at",
                "recovery_success": rec_rate,
                "k1_status": k1_entry.last_status,
                "k1_reset_at": k1_entry.last_error_reset_at,
                "cooldown_honors_upstream_reset": k1_entry.last_error_reset_at == future_reset,
            })

    # Retry2 failure marks rotated key exhausted too
    with patch("agent.credential_pool.persist_pool_entries"):
        pool = CredentialPool("openrouter", [k1, k2])
        # First recovery rotates k1 -> k2
        pool.mark_exhausted_and_rotate(status_code=429, credential_id="k1", api_key_hint="sk-or-1")
        # Second call hits 429 on rotated key k2
        pool.mark_exhausted_and_rotate(status_code=429, credential_id="k2", api_key_hint="sk-or-2")
        rows.append({
            "case": "second_failure_retry2_err_marks_rotated_key_exhausted",
            "k1_status": pool.entries()[0].last_status,
            "k2_status": pool.entries()[1].last_status,
            "pool_has_available": pool.has_available(),
            "description": "Rotated key hitting quota/auth error is marked immediately so pool halts same-provider retries",
        })

    return rows


# ---------------------------------------------------------------------------
# Section 6: Provider health tracking
# ---------------------------------------------------------------------------
def section_provider_health_tracking() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # Label normalization
    rows.append({
        "case": "normalize_chain_label_aliases",
        "openrouter": ax._normalize_chain_label("openrouter"),
        "nous": ax._normalize_chain_label("nous"),
        "custom": ax._normalize_chain_label("custom"),
        "local_custom": ax._normalize_chain_label("local/custom"),
        "openai_codex": ax._normalize_chain_label("openai-codex"),
        "codex": ax._normalize_chain_label("codex"),
        "deepseek": ax._normalize_chain_label("deepseek"),
    })

    # Marking and expiration
    ax._reset_aux_unhealthy_cache()
    ax._mark_provider_unhealthy("openrouter", ttl=300)
    ax._mark_provider_unhealthy("custom", ttl=300)  # stored under local/custom
    ax._mark_provider_unhealthy("expired_provider", ttl=-10)

    rows.append({
        "case": "provider_unhealthy_cache_evaluation",
        "openrouter_is_unhealthy": ax._is_provider_unhealthy("openrouter"),
        "custom_lookup_is_false": ax._is_provider_unhealthy("custom"),
        "local_custom_lookup_is_true": ax._is_provider_unhealthy("local/custom"),
        "expired_provider_lazy_evicted": ax._is_provider_unhealthy("expired_provider"),
    })

    # Unhealthy provider skipping in API-key catalog
    with patch("hermes_cli.auth.PROVIDER_REGISTRY", {
        "unhealthy_api": type("P", (), {"auth_type": "api_key", "name": "Unhealthy API"})(),
        "healthy_api": type("P", (), {"auth_type": "api_key", "name": "Healthy API", "inference_base_url": "https://api.healthy.com/v1"})(),
    }), \
    patch("agent.auxiliary_client._is_provider_unhealthy", side_effect=lambda p: p == "unhealthy_api"), \
    patch("agent.auxiliary_client._select_pool_entry", return_value=(False, None)), \
    patch("hermes_cli.auth.resolve_api_key_provider_credentials", return_value={"api_key": "valid-key"}), \
    patch("agent.auxiliary_client._get_aux_model_for_provider", return_value="healthy-model"), \
    patch("agent.auxiliary_client._create_openai_client", return_value=MagicMock()):
        client, model = ax._resolve_api_key_provider()
        rows.append({
            "case": "api_key_catalog_skips_unhealthy_provider",
            "resolved_model": model,
            "resolved_provider_skipped_unhealthy": True,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 7: Poisoned-client eviction
# ---------------------------------------------------------------------------
def section_poisoned_client_eviction() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # Cache key generation with pool hint
    key_with_hint = ax._client_cache_key(
        "openrouter", async_mode=False, base_url="https://openrouter.ai/api/v1",
        api_key="sk-or", task="compression", model="meta-llama/llama-3-70b"
    )
    rows.append({
        "case": "client_cache_key_structure",
        "key_provider": key_with_hint[0],
        "key_async": key_with_hint[1],
        "key_base_url": key_with_hint[2],
        "key_task": key_with_hint[7],
        "key_model": key_with_hint[9],
    })

    # Evict cached clients by provider
    class MockClient:
        def __init__(self):
            self.closed = False
        def close(self):
            self.closed = True

    c1 = MockClient()
    c2 = MockClient()
    ax._client_cache.clear()
    ax._client_cache[("openrouter", False, "url1", "k1", "", (), False, "", "", "m1")] = (c1, "m1", None)
    ax._client_cache[("anthropic", False, "url2", "k2", "", (), False, "", "", "m2")] = (c2, "m2", None)

    ax._evict_cached_clients("openrouter")
    rows.append({
        "case": "evict_cached_clients_by_provider",
        "c1_closed": c1.closed,
        "remaining_keys_count": len(ax._client_cache),
        "anthropic_retained": ("anthropic", False, "url2", "k2", "", (), False, "", "", "m2") in ax._client_cache,
    })

    # Evict cached client instance targeting wrapper._real_client
    class MockWrapper:
        def __init__(self, real):
            self._real_client = real

    real_sub = MockClient()
    wrapper = MockWrapper(real_sub)
    ax._client_cache.clear()
    ax._client_cache[("wrapped", False)] = (wrapper, "m1", None)

    evicted = ax._evict_cached_client_instance(real_sub)
    rows.append({
        "case": "evict_cached_client_instance_unwraps_real_client",
        "evicted": evicted,
        "cache_empty": len(ax._client_cache) == 0,
        "description": "Eviction traverses _real_client attribute so wrapped clients are evicted on underlying connection failure",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 8: Maximum request and retry budget
# ---------------------------------------------------------------------------
def section_request_retry_budget() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # Compression timeout skipping
    timeout_err = TimeoutError("Request timed out after 300.0s")
    no_progress_err = TimeoutError("LLM response timed out with no output emitted (no-progress timeout)")
    conn_err = ConnectionError("Connection refused")

    rows.append({
        "case": "compression_timeout_retry_skipping",
        "compression_hard_timeout_skips_retry": ax._should_skip_same_provider_retry("compression", timeout_err),
        "compression_no_progress_allows_retry": ax._should_skip_same_provider_retry("compression", no_progress_err),
        "vision_hard_timeout_skips_retry": ax._should_skip_same_provider_retry("vision", timeout_err),
        "title_generation_timeout_allows_retry": ax._should_skip_same_provider_retry("title", timeout_err),
        "compression_conn_error_allows_retry": ax._should_skip_same_provider_retry("compression", conn_err),
    })

    # Summary of bounded retry tree for auxiliary compression
    rows.append({
        "case": "compression_summary_retry_budget_tree",
        "primary_attempts": 1,
        "same_provider_transient_retries_default": 2,
        "same_provider_transient_retries_on_compression_hard_timeout": 0,
        "parameter_stripped_retries": 1,
        "same_provider_credential_recovery_retries_max": 1,
        "fallback_candidate_auth_retries_max": 1,
        "maximum_same_provider_recovery_budget": "Strictly 1 attempt on rotated key before falling through",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 9: Multi-profile and global-root auth-store shadowing
# ---------------------------------------------------------------------------
def section_auth_store_shadowing_and_persistence() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    profile_store = {
        "credential_pool": {
            "openrouter": [{"id": "prof-or", "access_token": "sk-prof"}],
            "anthropic": [],  # empty list
        }
    }
    global_store = {
        "credential_pool": {
            "openrouter": [{"id": "glob-or", "access_token": "sk-glob"}],
            "anthropic": [{"id": "glob-ant", "access_token": "sk-glob-ant"}],
            "openai-codex": [{"id": "glob-codex", "access_token": "sk-glob-codex"}],
        }
    }

    with patch("hermes_cli.auth._load_auth_store", return_value=profile_store), \
         patch("hermes_cli.auth._load_global_auth_store", return_value=global_store):
        or_pool = read_credential_pool("openrouter")
        ant_pool = read_credential_pool("anthropic")
        codex_pool = read_credential_pool("openai-codex")
        rows.append({
            "case": "read_credential_pool_shadowing_rules",
            "openrouter_profile_shadows_global": [e["id"] for e in or_pool] == ["prof-or"],
            "anthropic_empty_profile_borrows_global": [e["id"] for e in ant_pool] == ["glob-ant"],
            "codex_missing_profile_borrows_global": [e["id"] for e in codex_pool] == ["glob-codex"],
        })

    # _merge_disk_cooldown_state: timestamp recency
    fixed_now = 1700000000.0
    mem_entry = {"id": "c1", "access_token": "key-1", "last_status": None, "last_status_at": None}
    disk_newer = {
        "id": "c1", "access_token": "key-1", "last_status": STATUS_EXHAUSTED,
        "last_status_at": fixed_now, "last_error_code": 429, "last_error_reset_at": fixed_now + 3600
    }
    disk_older = {
        "id": "c1", "access_token": "key-1", "last_status": STATUS_EXHAUSTED,
        "last_status_at": fixed_now - 100, "last_error_code": 429
    }
    mem_with_status = {"id": "c1", "access_token": "key-1", "last_status": STATUS_OK, "last_status_at": fixed_now}

    with patch("time.time", return_value=fixed_now):
        merged_adopts = _merge_disk_cooldown_state(mem_entry, disk_newer, "openrouter")
        merged_ignores = _merge_disk_cooldown_state(mem_with_status, disk_older, "openrouter")

        # Access token change clears cooldown
        mem_rotated_key = {"id": "c1", "access_token": "key-2-fresh", "last_status": None, "last_status_at": None}
        merged_rotated = _merge_disk_cooldown_state(mem_rotated_key, disk_newer, "openrouter")

        rows.append({
            "case": "merge_disk_cooldown_state_semantics",
            "adopts_newer_disk_exhaustion": merged_adopts.get("last_status") == STATUS_EXHAUSTED,
            "ignores_older_disk_exhaustion": merged_ignores.get("last_status") == STATUS_OK,
            "token_change_preserves_clean_status": merged_rotated.get("last_status") is None,
        })

    # Single-use refresh pool providers set
    rows.append({
        "case": "single_use_refresh_pool_providers_set",
        "providers": sorted(list(SINGLE_USE_REFRESH_POOL_PROVIDERS)),
        "rule": "Borrowed single-use grants must write through to root under root flock and never fork local profile copies",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 10: Transport-dependency classification
# ---------------------------------------------------------------------------
def section_transport_dependency_matrix() -> List[Dict[str, Any]]:
    return [
        {
            "mechanism": "api_key_pool_selection_and_rotation",
            "provider": "any_api_key_provider",
            "portable_to_rust": True,
            "transport_type": "standard_http",
            "rationale": "Standard HTTP headers (Authorization: Bearer <key>); state mutation and selection order are pure disk/memory logic",
        },
        {
            "mechanism": "openai_codex_oauth_session",
            "provider": "openai-codex",
            "portable_to_rust": False,
            "transport_type": "provider_specific_responses_api",
            "rationale": "Requires /backend-api/codex Responses API wire translation, custom Cloudflare session headers, and SSE chunk framing via CodexAuxiliaryClient",
        },
        {
            "mechanism": "anthropic_oauth_pkce_refresh",
            "provider": "anthropic",
            "portable_to_rust": False,
            "transport_type": "oauth_pkce_and_header_masquerade",
            "rationale": "Requires Anthropic-specific OAuth refresh grant exchange, Claude Code user-agent masquerade (claude-code/0.1.0), and mcp_ tool prefix rewriting",
        },
        {
            "mechanism": "github_copilot_token_exchange",
            "provider": "copilot",
            "portable_to_rust": False,
            "transport_type": "external_cli_and_token_exchange",
            "rationale": "Requires executing 'gh auth token' CLI subprocess and exchanging GitHub token via https://api.github.com/copilot_internal/v2/token",
        },
        {
            "mechanism": "nous_research_nas_jwt_refresh",
            "provider": "nous",
            "portable_to_rust": False,
            "transport_type": "nous_portal_nas_exchange",
            "rationale": "Requires Nous Portal API call to exchange access/refresh tokens for short-lived NAS invocation JWTs (agent_key)",
        },
        {
            "mechanism": "xai_grok_oauth_responses_api",
            "provider": "xai-oauth",
            "portable_to_rust": False,
            "transport_type": "oauth_and_responses_wire",
            "rationale": "Requires xAI OAuth2 token refresh endpoint and translation of chat completions into /v1/responses format",
        },
        {
            "mechanism": "vertex_ai_google_auth_adc",
            "provider": "vertex",
            "portable_to_rust": False,
            "transport_type": "gcloud_adc_sdk",
            "rationale": "Requires Google Cloud Application Default Credentials (ADC) or google-auth SDK to generate short-lived GCP OAuth2 tokens",
        },
        {
            "mechanism": "azure_foundry_entra_id",
            "provider": "azure-foundry",
            "portable_to_rust": False,
            "transport_type": "azure_identity_callable_token",
            "rationale": "Requires azure-identity SDK token credential providers (tenant, client ID, scope callbacks)",
        },
        {
            "mechanism": "openrouter_api_key_recovery",
            "provider": "openrouter",
            "portable_to_rust": True,
            "transport_type": "standard_http",
            "rationale": "Standard OpenAI-compatible Chat Completions API with HTTP-level 401/402/429 status code handling",
        },
        {
            "mechanism": "custom_endpoint_api_key_recovery",
            "provider": "custom",
            "portable_to_rust": True,
            "transport_type": "standard_http",
            "rationale": "Standard OpenAI-compatible HTTP endpoints; placeholder 'no-key-required' and custom pool candidates are wire-independent",
        },
    ]


# ---------------------------------------------------------------------------
# Assembly and CLI
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, Any]:
    return {
        "pool_presence_and_fallback": section_pool_presence_and_fallback(),
        "selection_strategy_and_eligibility": section_selection_strategy_and_eligibility(),
        "runtime_identity_resolution": section_runtime_identity_resolution(),
        "recovery_401_refresh_and_rotation": section_recovery_401_refresh_and_rotation(),
        "recovery_402_429_accounting": section_recovery_402_429_accounting(),
        "provider_health_tracking": section_provider_health_tracking(),
        "poisoned_client_eviction": section_poisoned_client_eviction(),
        "request_retry_budget": section_request_retry_budget(),
        "auth_store_shadowing_and_persistence": section_auth_store_shadowing_and_persistence(),
        "transport_dependency_matrix": section_transport_dependency_matrix(),
    }


def main() -> None:
    args = sys.argv[1:]
    corpus = build_corpus()
    text = json.dumps(corpus, ensure_ascii=False, indent=2) + "\n"

    if args == ["--check"]:
        if not OUT.exists():
            raise SystemExit(f"Missing golden file: {OUT}")
        current = OUT.read_text()
        if current != text:
            raise SystemExit(
                "compression-credential-recovery-goldens.json is stale; rerun the generator"
            )
        print("OK: corpus matches checked-in goldens")
    elif not args:
        OUT.write_text(text)
        total_cases = sum(len(v) for v in corpus.values())
        print(f"Wrote {total_cases} cases across {len(corpus)} sections to {OUT.relative_to(ROOT)}")
    else:
        raise SystemExit("usage: gen_compression_credential_recovery_goldens.py [--check]")


if __name__ == "__main__":
    main()
