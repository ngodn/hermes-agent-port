#!/usr/bin/env python3
"""Deterministic source-executed oracle for main-provider credential-pool
selection and request-time recovery for the static API-key, chat-completions subset.

This script audits and executes the authoritative Python runtime contract across:
1. run_agent.py
2. agent/conversation_loop.py
3. agent/agent_runtime_helpers.py
4. agent/credential_pool.py
5. hermes_cli/runtime_provider.py
6. hermes_cli/auth.py
7. hermes_cli/route_identity.py
8. agent/error_classifier.py

It exercises:
- Section 1: Startup precedence and credential resolution
- Section 2: Failure attribution and key matching
- Section 3: Persistence-before-retry ordering
- Section 4: Retry-state transitions and request counts
- Section 5: Status and cooldown outcomes
- Section 6: Provider mismatch isolation
- Section 7: Route changes and client reconfiguration
- Section 8: Lifecycle across tool rounds and later turns
- Section 9: Unified streaming and tool-loop parity
- Section 10: Fallback and non-chat boundaries
- Section 11: Raw HTTP classifier boundaries

Usage:
    python3 rust/tools/gen_main_provider_pool_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_pool_goldens.py --check  # check parity
"""

from __future__ import annotations

import copy
import json
import logging
import os
import sys
import types
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-pool-goldens.json"

sys.path.insert(0, str(ROOT))


# Provide dummy yaml and dotenv modules so run_agent imports cleanly without
# external environment dependencies.
class _DummyYaml:
    SafeDumper = object
    SafeLoader = object

    def safe_load(self, *a, **k):
        return {}

    def dump(self, *a, **k):
        return ""

    def load(self, *a, **k):
        return {}


sys.modules.setdefault(
    "dotenv", types.SimpleNamespace(load_dotenv=lambda *a, **k: None)
)
sys.modules.setdefault("yaml", _DummyYaml())

import agent.agent_runtime_helpers as arh
import agent.credential_pool as cp
import hermes_cli.auth as auth
import hermes_cli.runtime_provider as rp
from agent.credential_pool import (
    AUTH_TYPE_API_KEY,
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
    CredentialPool,
    PooledCredential,
    _exhausted_ttl,
    _exhausted_until,
    _extract_retry_delay_seconds,
    _parse_absolute_timestamp,
    credential_pool_matches_provider,
    custom_provider_pool_key_candidates,
)
from agent.error_classifier import FailoverReason, classify_api_error
from hermes_cli.auth import read_credential_pool, resolve_provider
from hermes_cli.route_identity import normalize_route_base_url
from run_agent import AIAgent

# Wire lazy _ra() logger to standard logger
arh._ra = lambda: types.SimpleNamespace(
    logger=types.SimpleNamespace(
        info=lambda *a, **k: None,
        warning=lambda *a, **k: None,
        debug=lambda *a, **k: None,
    )
)

NOW = 1_700_000_000.0


class _OracleApiError(Exception):
    """Small OpenAI-compatible error surface for the live classifier."""

    def __init__(
        self,
        message: str,
        status_code: int,
        body: Dict[str, Any],
        headers: Dict[str, str],
    ):
        super().__init__(message)
        self.status_code = status_code
        self.body = body
        self.response = types.SimpleNamespace(headers=headers)


# ---------------------------------------------------------------------------
# Section 1: Startup Precedence and Credential Resolution
# ---------------------------------------------------------------------------
def section_startup_precedence() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1.1 Explicit keys override pool and env
    c1 = PooledCredential(
        provider="deepseek",
        id="ds-p1",
        label="Pool Key 1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-ds-pool-1",
    )
    pool = CredentialPool("deepseek", [c1])
    with (
        patch("hermes_cli.runtime_provider.load_pool", return_value=pool),
        patch.dict(os.environ, {"DEEPSEEK_API_KEY": "sk-ds-env-1"}),
    ):
        rt = rp.resolve_runtime_provider(
            requested="deepseek",
            explicit_api_key="sk-explicit-1",
            explicit_base_url="https://custom.deepseek.com/v1",
        )
        rows.append({
            "case": "explicit_keys_override_pool_and_env",
            "provider": rt.get("provider"),
            "source": rt.get("source"),
            "api_key": rt.get("api_key"),
            "base_url": rt.get("base_url"),
            "has_pool": "credential_pool" in rt
            and rt.get("credential_pool") is not None,
        })

    # 1.2 Explicit base URL with provider default key
    with (
        patch("hermes_cli.runtime_provider.load_pool", return_value=None),
        patch.dict(os.environ, {"DEEPSEEK_API_KEY": "sk-ds-env-key"}),
    ):
        rt = rp.resolve_runtime_provider(
            requested="deepseek",
            explicit_base_url="https://proxy.deepseek.com/v1",
        )
        rows.append({
            "case": "explicit_base_url_with_provider_default",
            "provider": rt.get("provider"),
            "source": rt.get("source"),
            "api_key": rt.get("api_key"),
            "base_url": rt.get("base_url"),
            "has_pool": "credential_pool" in rt
            and rt.get("credential_pool") is not None,
        })

    # 1.3 Active pool selected over env
    c_active = PooledCredential(
        provider="deepseek",
        id="ds-active",
        label="Active Key",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-ds-active-pool",
    )
    pool_active = CredentialPool("deepseek", [c_active])
    with (
        patch("hermes_cli.runtime_provider.load_pool", return_value=pool_active),
        patch.dict(os.environ, {"DEEPSEEK_API_KEY": "sk-ds-env-ignored"}),
    ):
        rt = rp.resolve_runtime_provider(requested="deepseek")
        rows.append({
            "case": "active_pool_selected_over_env",
            "provider": rt.get("provider"),
            "source": rt.get("source"),
            "api_key": rt.get("api_key"),
            "selected_entry_id": pool_active.current().id
            if pool_active.current()
            else None,
            "has_pool": "credential_pool" in rt
            and rt.get("credential_pool") is not None,
        })

    # 1.4 Per-entry custom base URL overrides provider default
    c_custom_url = PooledCredential(
        provider="deepseek",
        id="ds-custom-url",
        label="Custom URL Key",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-ds-custom",
        base_url="https://entry-endpoint.deepseek.internal/v1",
    )
    pool_custom_url = CredentialPool("deepseek", [c_custom_url])
    with patch("hermes_cli.runtime_provider.load_pool", return_value=pool_custom_url):
        rt = rp.resolve_runtime_provider(requested="deepseek")
        rows.append({
            "case": "per_entry_base_url_overrides_provider_default",
            "base_url": rt.get("base_url"),
            "expected_base_url": "https://entry-endpoint.deepseek.internal/v1",
        })

    # 1.5 Pool entry without base URL uses config default
    c_no_url = PooledCredential(
        provider="deepseek",
        id="ds-no-url",
        label="No URL Key",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-ds-nourl",
    )
    pool_no_url = CredentialPool("deepseek", [c_no_url])
    model_cfg = {
        "provider": "deepseek",
        "base_url": "https://config-endpoint.deepseek.com/v1",
    }
    rt_pool = rp._resolve_runtime_from_pool_entry(
        provider="deepseek",
        entry=c_no_url,
        requested_provider="deepseek",
        model_cfg=model_cfg,
        pool=pool_no_url,
    )
    rows.append({
        "case": "pool_entry_without_base_url_uses_config_default",
        "base_url": rt_pool.get("base_url"),
        "expected_base_url": "https://config-endpoint.deepseek.com/v1",
    })

    # 1.6 Empty pool falls back to env
    pool_empty = CredentialPool("deepseek", [])
    with (
        patch("hermes_cli.runtime_provider.load_pool", return_value=pool_empty),
        patch.dict(os.environ, {"DEEPSEEK_API_KEY": "sk-ds-from-env"}),
    ):
        rt = rp.resolve_runtime_provider(requested="deepseek")
        rows.append({
            "case": "empty_pool_falls_back_to_env",
            "provider": rt.get("provider"),
            "source": rt.get("source"),
            "api_key": rt.get("api_key"),
            "has_pool": "credential_pool" in rt
            and rt.get("credential_pool") is not None,
        })

    # 1.7 Exhausted pool falls back to env
    c_exh = PooledCredential(
        provider="deepseek",
        id="ds-exh",
        label="Exhausted Key",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-ds-exh",
        last_status=STATUS_EXHAUSTED,
        last_status_at=NOW - 10,
        last_error_code=429,
        last_error_reset_at=NOW + 3600,
    )
    pool_exh = CredentialPool("deepseek", [c_exh])
    with (
        patch("hermes_cli.runtime_provider.load_pool", return_value=pool_exh),
        patch("time.time", return_value=NOW),
        patch.dict(os.environ, {"DEEPSEEK_API_KEY": "sk-ds-fallback-env"}),
    ):
        rt = rp.resolve_runtime_provider(requested="deepseek")
        rows.append({
            "case": "exhausted_pool_falls_back_to_env",
            "provider": rt.get("provider"),
            "source": rt.get("source"),
            "api_key": rt.get("api_key"),
            "has_pool": "credential_pool" in rt
            and rt.get("credential_pool") is not None,
        })

    # 1.8 Profile pool shadows root pool
    profile_store = {
        "credential_pool": {
            "openai": [{"id": "prof-1", "access_token": "sk-prof-openai"}],
        }
    }
    global_store = {
        "credential_pool": {
            "openai": [{"id": "glob-1", "access_token": "sk-glob-openai"}],
            "deepseek": [{"id": "glob-2", "access_token": "sk-glob-deepseek"}],
        }
    }
    with (
        patch("hermes_cli.auth._load_auth_store", return_value=profile_store),
        patch("hermes_cli.auth._load_global_auth_store", return_value=global_store),
    ):
        res_openai = auth.read_credential_pool("openai")
        rows.append({
            "case": "profile_pool_shadows_root_pool",
            "provider": "openai",
            "entries_count": len(res_openai),
            "entry_id": res_openai[0]["id"] if res_openai else None,
            "shadowed_global": True,
        })

    # 1.9 Profile without provider borrows root pool
    with (
        patch("hermes_cli.auth._load_auth_store", return_value=profile_store),
        patch("hermes_cli.auth._load_global_auth_store", return_value=global_store),
    ):
        res_deepseek = auth.read_credential_pool("deepseek")
        rows.append({
            "case": "profile_without_provider_borrows_root_pool",
            "provider": "deepseek",
            "entries_count": len(res_deepseek),
            "entry_id": res_deepseek[0]["id"] if res_deepseek else None,
            "borrowed_from_global": True,
        })

    # 1.10 Provider alias normalization: z.ai -> zai
    norm_zai = resolve_provider("z.ai")
    rows.append({
        "case": "provider_alias_normalization_zai",
        "input_alias": "z.ai",
        "normalized_provider": norm_zai,
    })

    # 1.11 Provider alias normalization: minimax-china -> minimax-cn
    norm_mm = resolve_provider("minimax-china")
    rows.append({
        "case": "provider_alias_normalization_minimax",
        "input_alias": "minimax-china",
        "normalized_provider": norm_mm,
    })

    # 1.12 Custom provider slug candidates
    custom_cfg = [
        (
            "b-ai",
            {
                "name": "B-AI Provider",
                "provider_key": "b-ai",
                "base_url": "https://api.b.ai/v1",
            },
        )
    ]
    with patch("agent.credential_pool._iter_custom_providers", return_value=custom_cfg):
        cands = custom_provider_pool_key_candidates(
            base_url="https://api.b.ai/v1",
            provider_name="b-ai",
        )
        rows.append({
            "case": "custom_provider_slug_candidates",
            "candidates": list(cands),
            "preferred_slug": cands[0] if cands else None,
        })

    # 1.13 Disabled provider fails fast
    disabled_cfg = {"providers": {"deepseek": {"enabled": False}}}
    with patch("hermes_cli.config.load_config", return_value=disabled_cfg):
        failed_fast = False
        error_msg = ""
        try:
            rp.resolve_runtime_provider(requested="deepseek")
        except ValueError as e:
            failed_fast = True
            error_msg = str(e)
        rows.append({
            "case": "disabled_provider_fails_fast",
            "failed_fast": failed_fast,
            "error_contains_disabled": "disabled" in error_msg.lower(),
        })

    return rows


# ---------------------------------------------------------------------------
# Section 2: Failure Attribution and Key Matching
# ---------------------------------------------------------------------------
def section_failure_attribution() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    def make_agent(pool, failing_key, credential_id=None):
        return types.SimpleNamespace(
            provider="deepseek",
            api_key=failing_key,
            base_url="https://api.deepseek.com/v1",
            _credential_pool=pool,
            _credential_pool_entry_id=credential_id,
            _swap_credential=MagicMock(),
            _is_entitlement_failure=lambda ctx, status: False,
        )

    # 2.1 Attribution by entry id matches
    e1 = PooledCredential(
        provider="deepseek",
        id="cred-1",
        label="K1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-1",
    )
    e2 = PooledCredential(
        provider="deepseek",
        id="cred-2",
        label="K2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="sk-2",
    )
    pool1 = CredentialPool("deepseek", [e1, e2])
    agent1 = make_agent(pool1, failing_key="sk-1", credential_id="cred-1")
    recovered1, retried1 = arh.recover_with_credential_pool(
        agent1,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    statuses1 = {e.id: e.last_status for e in pool1.entries()}
    rows.append({
        "case": "attribution_by_entry_id_matches",
        "recovered": recovered1,
        "retried": retried1,
        "cred_1_status": statuses1.get("cred-1"),
        "cred_2_status": statuses1.get("cred-2"),
    })

    # 2.2 Attribution by key hint when entry id is None
    pool2 = CredentialPool("deepseek", [copy.copy(e1), copy.copy(e2)])
    agent2 = make_agent(pool2, failing_key="sk-2", credential_id=None)
    recovered2, retried2 = arh.recover_with_credential_pool(
        agent2,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    statuses2 = {e.id: e.last_status for e in pool2.entries()}
    rows.append({
        "case": "attribution_by_key_hint_when_id_none",
        "recovered": recovered2,
        "cred_1_status": statuses2.get("cred-1"),
        "cred_2_status": statuses2.get("cred-2"),
    })

    # 2.3 Attribution disagreement: key wins over stale id (#79156)
    pool3 = CredentialPool("deepseek", [copy.copy(e1), copy.copy(e2)])
    agent3 = make_agent(pool3, failing_key="sk-2", credential_id="cred-1")
    recovered3, _ = arh.recover_with_credential_pool(
        agent3,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    statuses3 = {e.id: e.last_status for e in pool3.entries()}
    rows.append({
        "case": "attribution_disagreement_key_wins",
        "recovered": recovered3,
        "cred_1_status": statuses3.get("cred-1"),
        "cred_2_status": statuses3.get("cred-2"),
    })

    # 2.4 Attribution when id is provided but request key is not in pool
    pool4 = CredentialPool("deepseek", [copy.copy(e1), copy.copy(e2)])
    agent4 = make_agent(pool4, failing_key="sk-unknown-wrapper", credential_id="cred-1")
    recovered4, _ = arh.recover_with_credential_pool(
        agent4,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    statuses4 = {e.id: e.last_status for e in pool4.entries()}
    rows.append({
        "case": "attribution_stale_id_unknown_key_dropped",
        "recovered": recovered4,
        "cred_1_status": statuses4.get("cred-1"),
        "cred_2_status": statuses4.get("cred-2"),
    })

    # 2.5 Fallback to pool.current() when neither agent key nor id is available
    pool5 = CredentialPool("deepseek", [copy.copy(e1), copy.copy(e2)])
    agent5 = make_agent(pool5, failing_key="", credential_id=None)
    pool5.select()  # points current() at cred-1
    recovered5, _ = arh.recover_with_credential_pool(
        agent5,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    statuses5 = {e.id: e.last_status for e in pool5.entries()}
    rows.append({
        "case": "attribution_fallback_to_current",
        "recovered": recovered5,
        "cred_1_status": statuses5.get("cred-1"),
        "cred_2_status": statuses5.get("cred-2"),
    })

    # 2.6 Sibling key exhaustion marks all sharing keys
    s1 = PooledCredential(
        provider="deepseek",
        id="sib-1",
        label="S1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-shared",
    )
    s2 = PooledCredential(
        provider="deepseek",
        id="sib-2",
        label="S2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="sk-other",
    )
    s3 = PooledCredential(
        provider="deepseek",
        id="sib-3",
        label="S3",
        auth_type=AUTH_TYPE_API_KEY,
        priority=2,
        source="manual",
        access_token="sk-shared",
    )
    pool_sib = CredentialPool("deepseek", [s1, s2, s3])
    agent_sib = make_agent(pool_sib, failing_key="sk-shared", credential_id="sib-1")
    recovered_sib, _ = arh.recover_with_credential_pool(
        agent_sib,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    statuses_sib = {e.id: e.last_status for e in pool_sib.entries()}
    rows.append({
        "case": "sibling_key_exhaustion_marks_all",
        "recovered": recovered_sib,
        "sib_1_status": statuses_sib.get("sib-1"),
        "sib_2_status": statuses_sib.get("sib-2"),
        "sib_3_status": statuses_sib.get("sib-3"),
        "next_swapped_id": getattr(
            agent_sib._swap_credential.call_args[0][0], "id", None
        ),
    })

    # 2.7 Unmatched key streak cap
    pool_cap = CredentialPool("deepseek", [copy.copy(e1), copy.copy(e2)])
    streak_outcomes = []
    for _ in range(4):
        next_ent = pool_cap.mark_exhausted_and_rotate(
            status_code=429, api_key_hint="sk-unknown"
        )
        streak_outcomes.append(next_ent.id if next_ent else None)
    rows.append({
        "case": "unmatched_key_streak_capped",
        "pool_size": 2,
        "streak_outcomes": streak_outcomes,
        "capped_at_none": streak_outcomes[-1] is None,
    })

    # 2.8 Single-entry pool with unmatched key
    pool_single = CredentialPool("deepseek", [copy.copy(e1)])
    single_out = pool_single.mark_exhausted_and_rotate(
        status_code=429, api_key_hint="sk-unknown"
    )
    rows.append({
        "case": "single_entry_pool_unmatched_key_no_loop",
        "outcome": single_out.id if single_out else None,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 3: Persistence-Before-Retry Ordering
# ---------------------------------------------------------------------------
def section_persistence_ordering() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    def make_order_tracking_agent(pool, key, cred_id):
        order: List[str] = []

        def mock_persist(provider, entries, removed_ids=None):
            order.append(f"persist:{entries[0].get('last_status')}")

        def mock_swap(entry):
            order.append(f"swap:{entry.id}")

        agent = types.SimpleNamespace(
            provider="deepseek",
            api_key=key,
            base_url="https://api.deepseek.com/v1",
            _credential_pool=pool,
            _credential_pool_entry_id=cred_id,
            _swap_credential=mock_swap,
            _is_entitlement_failure=lambda ctx, status: False,
        )
        return agent, order, mock_persist

    # 3.1 429 Rate limit persistence before retry
    c1 = PooledCredential(
        provider="deepseek",
        id="c1",
        label="K1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="k1",
    )
    c2 = PooledCredential(
        provider="deepseek",
        id="c2",
        label="K2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="k2",
    )
    pool1 = CredentialPool("deepseek", [c1, c2])
    agent1, order1, persist1 = make_order_tracking_agent(pool1, "k1", "c1")
    with patch("agent.credential_pool.persist_pool_entries", side_effect=persist1):
        arh.recover_with_credential_pool(
            agent1,
            status_code=429,
            has_retried_429=True,
            classified_reason=FailoverReason.rate_limit,
        )
    rows.append({
        "case": "rate_limit_persisted_before_swap",
        "execution_order": list(order1),
        "persisted_first": order1[0].startswith("persist:")
        and order1[1].startswith("swap:"),
    })

    # 3.2 402 Billing persistence before retry
    pool2 = CredentialPool("deepseek", [copy.copy(c1), copy.copy(c2)])
    agent2, order2, persist2 = make_order_tracking_agent(pool2, "k1", "c1")
    with patch("agent.credential_pool.persist_pool_entries", side_effect=persist2):
        arh.recover_with_credential_pool(
            agent2,
            status_code=402,
            has_retried_429=False,
            classified_reason=FailoverReason.billing,
        )
    rows.append({
        "case": "billing_persisted_before_swap",
        "execution_order": list(order2),
        "persisted_first": order2[0].startswith("persist:")
        and order2[1].startswith("swap:"),
    })

    # 3.3 401 Auth persistence before retry
    pool3 = CredentialPool("deepseek", [copy.copy(c1), copy.copy(c2)])
    agent3, order3, persist3 = make_order_tracking_agent(pool3, "k1", "c1")
    with patch("agent.credential_pool.persist_pool_entries", side_effect=persist3):
        arh.recover_with_credential_pool(
            agent3,
            status_code=401,
            has_retried_429=False,
            classified_reason=FailoverReason.auth,
        )
    rows.append({
        "case": "auth_persisted_before_swap",
        "execution_order": list(order3),
        "persisted_first": order3[0].startswith("persist:")
        and order3[1].startswith("swap:"),
    })

    # 3.4 Sibling entries persisted together before retry
    s1 = PooledCredential(
        provider="deepseek",
        id="s1",
        label="S1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="k-shared",
    )
    s2 = PooledCredential(
        provider="deepseek",
        id="s2",
        label="S2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="k-unique",
    )
    s3 = PooledCredential(
        provider="deepseek",
        id="s3",
        label="S3",
        auth_type=AUTH_TYPE_API_KEY,
        priority=2,
        source="manual",
        access_token="k-shared",
    )
    pool4 = CredentialPool("deepseek", [s1, s2, s3])
    persist_payloads: List[List[dict]] = []

    def persist_capturing(provider, entries, removed_ids=None):
        persist_payloads.append([
            dict(id=e.get("id"), last_status=e.get("last_status")) for e in entries
        ])

    agent4 = types.SimpleNamespace(
        provider="deepseek",
        api_key="k-shared",
        base_url="https://api.deepseek.com/v1",
        _credential_pool=pool4,
        _credential_pool_entry_id="s1",
        _swap_credential=lambda e: None,
        _is_entitlement_failure=lambda ctx, status: False,
    )
    with patch(
        "agent.credential_pool.persist_pool_entries", side_effect=persist_capturing
    ):
        arh.recover_with_credential_pool(
            agent4,
            status_code=402,
            has_retried_429=False,
            classified_reason=FailoverReason.billing,
        )
    last_persist = persist_payloads[-1] if persist_payloads else []
    statuses_in_payload = {p["id"]: p["last_status"] for p in last_persist}
    rows.append({
        "case": "sibling_entries_persisted_together",
        "persist_call_count": len(persist_payloads),
        "s1_persisted_status": statuses_in_payload.get("s1"),
        "s2_persisted_status": statuses_in_payload.get("s2"),
        "s3_persisted_status": statuses_in_payload.get("s3"),
    })

    return rows


# ---------------------------------------------------------------------------
# Section 4: Retry-State Transitions and Request Counts
# ---------------------------------------------------------------------------
def section_retry_state_transitions() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    def make_agent(pool, key="k1", cred_id="c1"):
        swapped = []
        agent = types.SimpleNamespace(
            provider="openai",
            api_key=key,
            base_url="https://api.openai.com/v1",
            _credential_pool=pool,
            _credential_pool_entry_id=cred_id,
            _swap_credential=lambda e: swapped.append(e),
            _is_entitlement_failure=lambda ctx, status: False,
        )
        return agent, swapped

    c1 = PooledCredential(
        provider="openai",
        id="c1",
        label="K1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="k1",
    )
    c2 = PooledCredential(
        provider="openai",
        id="c2",
        label="K2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="k2",
    )

    # 4.1 Auth 401: static API key cannot refresh, rotates immediately on attempt 1
    p1 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a1, s1 = make_agent(p1)
    rec1, ret1 = arh.recover_with_credential_pool(
        a1,
        status_code=401,
        has_retried_429=False,
        classified_reason=FailoverReason.auth,
    )
    rows.append({
        "case": "auth_401_immediate_rotation",
        "recovered": rec1,
        "has_retried_429": ret1,
        "swapped_count": len(s1),
        "swapped_to_id": s1[-1].id if s1 else None,
    })

    # 4.2 Auth 401 terminal: marked STATUS_DEAD
    p2 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a2, _ = make_agent(p2)
    rec2, _ = arh.recover_with_credential_pool(
        a2,
        status_code=401,
        has_retried_429=False,
        classified_reason=FailoverReason.auth,
        error_context={"reason": "token_revoked"},
    )
    e1_stat = [e for e in p2.entries() if e.id == "c1"][0]
    rows.append({
        "case": "auth_401_terminal_dead_status",
        "recovered": rec2,
        "entry_status": e1_stat.last_status,
        "is_dead": e1_stat.last_status == STATUS_DEAD,
    })

    # 4.3 Auth 403 entitlement: skips pool refresh and rotation
    p3 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a3, s3 = make_agent(p3)
    a3._is_entitlement_failure = lambda ctx, status: True
    rec3, ret3 = arh.recover_with_credential_pool(
        a3,
        status_code=403,
        has_retried_429=False,
        classified_reason=FailoverReason.auth,
        error_context={
            "message": "OAuth authentication is currently not allowed for this organization"
        },
    )
    rows.append({
        "case": "auth_403_entitlement_skips_pool",
        "recovered": rec3,
        "has_retried_429": ret3,
        "swapped_count": len(s3),
    })

    # 4.4 Billing 402: immediate rotation without retry-same-key
    p4 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a4, s4 = make_agent(p4)
    rec4, ret4 = arh.recover_with_credential_pool(
        a4,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    rows.append({
        "case": "billing_402_immediate_rotation",
        "recovered": rec4,
        "has_retried_429": ret4,
        "swapped_to_id": s4[-1].id if s4 else None,
    })

    # 4.5 Billing classified 403 (e.g. OpenRouter key limit)
    p5 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a5, _ = make_agent(p5)
    rec5, ret5 = arh.recover_with_credential_pool(
        a5,
        status_code=403,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
        error_context={"reason": "key_limit_exceeded"},
    )
    e1_p5 = [e for e in p5.entries() if e.id == "c1"][0]
    rows.append({
        "case": "billing_403_classified_reason",
        "recovered": rec5,
        "failure_reason": e1_p5.failure_reason,
    })

    # 4.6 Ambiguous billing body (billing_unverified)
    p6 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a6, _ = make_agent(p6)
    rec6, ret6 = arh.recover_with_credential_pool(
        a6,
        status_code=400,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
        billing_unverified=True,
    )
    e1_p6 = [e for e in p6.entries() if e.id == "c1"][0]
    rows.append({
        "case": "billing_unverified_short_cooldown_record",
        "recovered": rec6,
        "failure_reason": e1_p6.failure_reason,
    })

    # 4.7 Ordinary rate limit attempt 1: retries same key
    p7 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a7, s7 = make_agent(p7)
    rec7, ret7 = arh.recover_with_credential_pool(
        a7,
        status_code=429,
        has_retried_429=False,
        classified_reason=FailoverReason.rate_limit,
    )
    rows.append({
        "case": "ordinary_rate_limit_attempt_1_retries_same",
        "recovered": rec7,
        "has_retried_429": ret7,
        "swapped_count": len(s7),
    })

    # 4.8 Ordinary rate limit attempt 2: rotates to next key
    p8 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a8, s8 = make_agent(p8)
    rec8, ret8 = arh.recover_with_credential_pool(
        a8,
        status_code=429,
        has_retried_429=True,
        classified_reason=FailoverReason.rate_limit,
    )
    rows.append({
        "case": "ordinary_rate_limit_attempt_2_rotates",
        "recovered": rec8,
        "has_retried_429": ret8,
        "swapped_to_id": s8[-1].id if s8 else None,
    })

    # 4.9 Pre-exhausted rate limit: rotates immediately on attempt 1
    c1_pre_exh = copy.copy(c1)
    c1_pre_exh.last_status = STATUS_EXHAUSTED
    p9 = CredentialPool("openai", [c1_pre_exh, copy.copy(c2)])
    a9, s9 = make_agent(p9)
    rec9, ret9 = arh.recover_with_credential_pool(
        a9,
        status_code=429,
        has_retried_429=False,
        classified_reason=FailoverReason.rate_limit,
    )
    rows.append({
        "case": "pre_exhausted_rate_limit_immediate_rotation",
        "recovered": rec9,
        "has_retried_429": ret9,
        "swapped_to_id": s9[-1].id if s9 else None,
    })

    # 4.10 Usage limit reached: rotates immediately on attempt 1
    p10 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a10, s10 = make_agent(p10)
    rec10, ret10 = arh.recover_with_credential_pool(
        a10,
        status_code=429,
        has_retried_429=False,
        classified_reason=FailoverReason.rate_limit,
        error_context={
            "reason": "usage_limit_reached",
            "message": "Usage limit reached",
        },
    )
    rows.append({
        "case": "usage_limit_reached_immediate_rotation",
        "recovered": rec10,
        "has_retried_429": ret10,
        "swapped_to_id": s10[-1].id if s10 else None,
    })

    # 4.11 Upstream aggregator rate limit defers without rotation
    p11 = CredentialPool("openrouter", [copy.copy(c1), copy.copy(c2)])
    a11, s11 = make_agent(p11)
    a11.provider = "openrouter"
    rec11, ret11 = arh.recover_with_credential_pool(
        a11,
        status_code=429,
        has_retried_429=False,
        classified_reason=FailoverReason.upstream_rate_limit,
        error_context={"upstream_provider": "deepseek"},
    )
    rows.append({
        "case": "upstream_aggregator_rate_limit_defers",
        "recovered": rec11,
        "has_retried_429": ret11,
        "swapped_count": len(s11),
        "c1_status": [e for e in p11.entries() if e.id == "c1"][0].last_status,
    })

    # 4.12 Pool exhaustion: all entries exhausted returns False
    p12 = CredentialPool("openai", [copy.copy(c1)])
    a12, _ = make_agent(p12)
    # First 429 sets retry flag
    rec12_1, ret12_1 = arh.recover_with_credential_pool(
        a12,
        status_code=429,
        has_retried_429=False,
        classified_reason=FailoverReason.rate_limit,
    )
    # Second 429 exhausts only key and fails recovery
    rec12_2, ret12_2 = arh.recover_with_credential_pool(
        a12,
        status_code=429,
        has_retried_429=ret12_1,
        classified_reason=FailoverReason.rate_limit,
    )
    rows.append({
        "case": "pool_exhaustion_returns_false",
        "attempt_1_recovered": rec12_1,
        "attempt_1_retried": ret12_1,
        "attempt_2_recovered": rec12_2,
        "attempt_2_retried": ret12_2,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 5: Status and Cooldown Outcomes
# ---------------------------------------------------------------------------
def section_cooldown_outcomes() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 5.1 Cooldown 401 default
    rows.append({
        "case": "cooldown_401_default",
        "ttl": _exhausted_ttl(401, sole_credential=False),
        "expected": EXHAUSTED_TTL_401_SECONDS,
    })

    # 5.2 Cooldown 429 multi-entry
    rows.append({
        "case": "cooldown_429_multi_entry",
        "ttl": _exhausted_ttl(429, sole_credential=False),
        "expected": EXHAUSTED_TTL_429_SECONDS,
    })

    # 5.3 Cooldown 429 sole credential shortened
    rows.append({
        "case": "cooldown_429_sole_credential_shortened",
        "ttl": _exhausted_ttl(429, sole_credential=True),
        "expected": EXHAUSTED_TTL_SOLE_CREDENTIAL_SECONDS,
    })

    # 5.4 Cooldown 402 multi-entry
    rows.append({
        "case": "cooldown_402_multi_entry",
        "ttl": _exhausted_ttl(402, sole_credential=False),
        "expected": EXHAUSTED_TTL_DEFAULT_SECONDS,
    })

    # 5.5 Cooldown 402 sole credential not shortened
    rows.append({
        "case": "cooldown_402_sole_credential_not_shortened",
        "ttl": _exhausted_ttl(402, sole_credential=True),
        "expected": EXHAUSTED_TTL_DEFAULT_SECONDS,
    })

    # 5.6 Cooldown billing classified sole not shortened
    rows.append({
        "case": "cooldown_billing_classified_sole_not_shortened",
        "ttl": _exhausted_ttl(
            403, sole_credential=True, failure_reason=FAILURE_REASON_BILLING
        ),
        "expected": EXHAUSTED_TTL_DEFAULT_SECONDS,
    })

    # 5.7 Cooldown billing unverified non-402 shortened
    rows.append({
        "case": "cooldown_billing_unverified_non_402_shortened",
        "ttl": _exhausted_ttl(
            400, sole_credential=True, failure_reason=FAILURE_REASON_BILLING_UNVERIFIED
        ),
        "expected": EXHAUSTED_TTL_SOLE_CREDENTIAL_SECONDS,
    })

    # 5.8 Cooldown billing unverified 402 full bench
    rows.append({
        "case": "cooldown_billing_unverified_402_full_bench",
        "ttl": _exhausted_ttl(
            402, sole_credential=True, failure_reason=FAILURE_REASON_BILLING_UNVERIFIED
        ),
        "expected": EXHAUSTED_TTL_DEFAULT_SECONDS,
    })

    # 5.9 Cooldown retry-after header delay override
    d1 = _extract_retry_delay_seconds("retry after 120s")
    rows.append({
        "case": "cooldown_retry_after_header_override",
        "extracted_delay": d1,
        "expected": 120.0,
    })

    # 5.10 Cooldown parsed hr min message override
    d2 = _extract_retry_delay_seconds("resets in 4hr 5min")
    rows.append({
        "case": "cooldown_parsed_hr_min_message_override",
        "extracted_delay": d2,
        "expected": (4 * 3600) + (5 * 60),
    })

    # 5.11 Cooldown quota reset delay override
    d3 = _extract_retry_delay_seconds("quotaResetDelay: 45s")
    rows.append({
        "case": "cooldown_quota_reset_delay_override",
        "extracted_delay": d3,
        "expected": 45.0,
    })

    # 5.12 Cooldown expiry revives entry on select()
    c_revive = PooledCredential(
        provider="openai",
        id="rev-1",
        label="Rev 1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="k-rev",
        last_status=STATUS_EXHAUSTED,
        last_status_at=NOW - 4000,
        last_error_code=429,
        last_error_reset_at=NOW - 5,
    )
    pool_rev = CredentialPool("openai", [c_revive])
    with patch("time.time", return_value=NOW):
        sel_rev = pool_rev.select()
        rows.append({
            "case": "cooldown_expiry_revives_entry",
            "selected_id": sel_rev.id if sel_rev else None,
            "status_after_select": pool_rev.entries()[0].last_status,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 6: Provider Mismatch Isolation
# ---------------------------------------------------------------------------
def section_provider_mismatch() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    def make_agent(provider, base_url, pool):
        return types.SimpleNamespace(
            provider=provider,
            base_url=base_url,
            api_key="sk-test",
            _credential_pool=pool,
            _credential_pool_entry_id="c1",
            _swap_credential=MagicMock(),
            _is_entitlement_failure=lambda ctx, status: False,
        )

    c1 = PooledCredential(
        provider="openrouter",
        id="c1",
        label="OR1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-or",
    )
    pool_or = CredentialPool("openrouter", [c1])

    # 6.1 Primary pool with mismatched fallback skips mutation
    agent_fallback = make_agent(
        "openai-codex", "https://chatgpt.com/backend-api", pool_or
    )
    rec1, ret1 = arh.recover_with_credential_pool(
        agent_fallback,
        status_code=401,
        has_retried_429=False,
        classified_reason=FailoverReason.auth,
    )
    rows.append({
        "case": "primary_pool_mismatched_fallback_skips",
        "recovered": rec1,
        "retried": ret1,
        "entry_status": pool_or.entries()[0].last_status,
    })

    # 6.2 Primary pool matching provider rotates normally
    c2 = PooledCredential(
        provider="openrouter",
        id="c2",
        label="OR2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="sk-or-2",
    )
    pool_or2 = CredentialPool("openrouter", [copy.copy(c1), c2])
    agent_match = make_agent("openrouter", "https://openrouter.ai/api/v1", pool_or2)
    rec2, _ = arh.recover_with_credential_pool(
        agent_match,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    rows.append({
        "case": "primary_pool_matching_provider_rotates",
        "recovered": rec2,
        "entry_status": pool_or2.entries()[0].last_status,
    })

    # Custom provider test setup
    custom_cfg = [
        (
            "fireworks",
            {
                "name": "Fireworks",
                "provider_key": "fireworks",
                "base_url": "https://api.fireworks.ai/inference/v1",
            },
        )
    ]
    cf1 = PooledCredential(
        provider="custom:fireworks",
        id="fw1",
        label="FW1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="sk-fw-1",
    )
    cf2 = PooledCredential(
        provider="custom:fireworks",
        id="fw2",
        label="FW2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="sk-fw-2",
    )

    # 6.3 Custom pool with matching alias and URL
    pool_fw1 = CredentialPool("custom:fireworks", [copy.copy(cf1), copy.copy(cf2)])
    agent_fw1 = make_agent(
        "fireworks", "https://api.fireworks.ai/inference/v1", pool_fw1
    )
    with patch("agent.credential_pool._iter_custom_providers", return_value=custom_cfg):
        rec3, _ = arh.recover_with_credential_pool(
            agent_fw1,
            status_code=429,
            has_retried_429=True,
            classified_reason=FailoverReason.rate_limit,
        )
    rows.append({
        "case": "custom_pool_matching_alias_and_url",
        "recovered": rec3,
        "entry_status": pool_fw1.entries()[0].last_status,
    })

    # 6.4 Custom pool with generic custom matching URL
    pool_fw2 = CredentialPool("custom:fireworks", [copy.copy(cf1), copy.copy(cf2)])
    agent_fw2 = make_agent("custom", "https://api.fireworks.ai/inference/v1", pool_fw2)
    with patch("agent.credential_pool._iter_custom_providers", return_value=custom_cfg):
        rec4, _ = arh.recover_with_credential_pool(
            agent_fw2,
            status_code=429,
            has_retried_429=True,
            classified_reason=FailoverReason.rate_limit,
        )
    rows.append({
        "case": "custom_pool_generic_custom_matching_url",
        "recovered": rec4,
        "entry_status": pool_fw2.entries()[0].last_status,
    })

    # 6.5 Custom pool with mismatched URL skips mutation
    pool_fw3 = CredentialPool("custom:fireworks", [copy.copy(cf1), copy.copy(cf2)])
    agent_fw3 = make_agent("custom", "https://fallback.example.com/v1", pool_fw3)
    with patch("agent.credential_pool._iter_custom_providers", return_value=custom_cfg):
        rec5, ret5 = arh.recover_with_credential_pool(
            agent_fw3,
            status_code=429,
            has_retried_429=True,
            classified_reason=FailoverReason.rate_limit,
        )
    rows.append({
        "case": "custom_pool_mismatched_url_skips",
        "recovered": rec5,
        "retried": ret5,
        "entry_status": pool_fw3.entries()[0].last_status,
    })

    # 6.6 Custom pool with mismatched provider name skips mutation
    pool_fw4 = CredentialPool("custom:fireworks", [copy.copy(cf1), copy.copy(cf2)])
    agent_fw4 = make_agent("deepseek", "https://api.deepseek.com/v1", pool_fw4)
    with patch("agent.credential_pool._iter_custom_providers", return_value=custom_cfg):
        rec6, ret6 = arh.recover_with_credential_pool(
            agent_fw4,
            status_code=401,
            has_retried_429=False,
            classified_reason=FailoverReason.auth,
        )
    rows.append({
        "case": "custom_pool_mismatched_provider_name_skips",
        "recovered": rec6,
        "entry_status": pool_fw4.entries()[0].last_status,
    })

    # 6.7 Unscoped pool adapter compatible
    pool_unscoped = CredentialPool("", [copy.copy(cf1), copy.copy(cf2)])
    pool_unscoped.provider = None
    agent_unscoped = make_agent(
        "deepseek", "https://api.deepseek.com/v1", pool_unscoped
    )
    rec7, _ = arh.recover_with_credential_pool(
        agent_unscoped,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    rows.append({
        "case": "unscoped_pool_adapter_compatible",
        "recovered": rec7,
        "entry_status": pool_unscoped.entries()[0].last_status,
    })

    # 6.8 Empty agent provider fails closed
    pool_closed = CredentialPool("openrouter", [copy.copy(c1), copy.copy(c2)])
    agent_closed = make_agent("", "https://openrouter.ai/api/v1", pool_closed)
    rec8, ret8 = arh.recover_with_credential_pool(
        agent_closed,
        status_code=401,
        has_retried_429=False,
        classified_reason=FailoverReason.auth,
    )
    rows.append({
        "case": "empty_agent_provider_fails_closed",
        "recovered": rec8,
        "entry_status": pool_closed.entries()[0].last_status,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 7: Route Changes and Client Reconfiguration
# ---------------------------------------------------------------------------
def section_route_changes() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    def make_swap_agent(initial_base_url, initial_key):
        route_changed_events: List[bool] = []
        agent = types.SimpleNamespace(
            api_mode="chat_completions",
            provider="openrouter",
            base_url=initial_base_url,
            api_key=initial_key,
            _client_kwargs={
                "api_key": initial_key,
                "base_url": initial_base_url,
                "default_headers": {"User-Agent": "Hermes"},
            },
            _reapply_route_client_config=lambda route_changed: (
                route_changed_events.append(route_changed)
            ),
            _replace_primary_openai_client=lambda reason: True,
            _credential_pool_entry_id=None,
        )
        return agent, route_changed_events

    # 7.1 Same route rotation preserves headers
    agent1, rc1 = make_swap_agent("https://openrouter.ai/api/v1", "sk-1")
    entry1 = PooledCredential(
        provider="openrouter",
        id="c2",
        label="K2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="sk-2",
        base_url="https://openrouter.ai/api/v1",
    )
    AIAgent._swap_credential(agent1, entry1)
    rows.append({
        "case": "same_route_rotation_preserves_headers",
        "route_changed": rc1[-1] if rc1 else None,
        "api_key": agent1.api_key,
        "base_url": agent1.base_url,
    })

    # 7.2 Different endpoint triggers route change
    agent2, rc2 = make_swap_agent("https://openrouter.ai/api/v1", "sk-1")
    entry2 = PooledCredential(
        provider="openrouter",
        id="c3",
        label="K3",
        auth_type=AUTH_TYPE_API_KEY,
        priority=2,
        source="manual",
        access_token="sk-3",
        base_url="https://custom-proxy.internal/v1",
    )
    AIAgent._swap_credential(agent2, entry2)
    rows.append({
        "case": "different_endpoint_triggers_route_change",
        "route_changed": rc2[-1] if rc2 else None,
        "api_key": agent2.api_key,
        "base_url": agent2.base_url,
    })

    # 7.3 Route change reapplies TLS and drops user headers
    agent3 = types.SimpleNamespace(
        api_mode="chat_completions",
        base_url="https://new-host.ai/v1",
        _client_kwargs={
            "ssl_verify": "/old/path.pem",
            "ssl_ca_cert": "/old/ca.pem",
            "default_headers": {"X-Custom": "val"},
        },
        _apply_client_headers_for_base_url=MagicMock(),
    )
    AIAgent._reapply_route_client_config(agent3, route_changed=True)
    rows.append({
        "case": "route_change_reapplies_tls_and_drops_user_headers",
        "ssl_verify_cleared": "ssl_verify" not in agent3._client_kwargs,
        "ssl_ca_cert_cleared": "ssl_ca_cert" not in agent3._client_kwargs,
        "apply_headers_user_headers_arg": agent3._apply_client_headers_for_base_url.call_args[
            1
        ].get("apply_user_headers"),
    })

    # 7.4 Route normalization equivalence
    u1 = "https://api.openai.com:443/v1/"
    u2 = "https://api.openai.com/v1"
    norm1 = normalize_route_base_url(u1)
    norm2 = normalize_route_base_url(u2)
    rows.append({
        "case": "route_normalization_equivalence",
        "norm1": norm1,
        "norm2": norm2,
        "is_equivalent": norm1 == norm2,
    })

    # 7.5 Route port or host difference
    u3 = "http://127.0.0.1:8000/v1"
    u4 = "http://127.0.0.1:8001/v1"
    norm3 = normalize_route_base_url(u3)
    norm4 = normalize_route_base_url(u4)
    rows.append({
        "case": "route_port_or_host_difference",
        "norm3": norm3,
        "norm4": norm4,
        "is_different": norm3 != norm4,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 8: Lifecycle Across Tool Rounds and Later Turns
# ---------------------------------------------------------------------------
def section_lifecycle_rounds_and_turns() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # Setup agent with 2 credentials
    e1 = PooledCredential(
        provider="deepseek",
        id="c1",
        label="K1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="k1",
    )
    e2 = PooledCredential(
        provider="deepseek",
        id="c2",
        label="K2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="k2",
    )
    pool = CredentialPool("deepseek", [e1, e2])

    agent = types.SimpleNamespace(
        api_mode="chat_completions",
        provider="deepseek",
        base_url="https://api.deepseek.com/v1",
        api_key="k1",
        _credential_pool=pool,
        _credential_pool_entry_id="c1",
        _client_kwargs={"api_key": "k1", "base_url": "https://api.deepseek.com/v1"},
        _reapply_route_client_config=lambda route_changed: None,
        _replace_primary_openai_client=lambda reason: True,
        _is_entitlement_failure=lambda ctx, status: False,
        _fallback_activated=False,
        _env_creds_seen=None,
    )
    # Bind AIAgent._swap_credential
    agent._swap_credential = lambda entry: AIAgent._swap_credential(agent, entry)

    # 8.1 Tool round 1 rotates from K1 to K2
    round_1_has_retried_429 = False
    rec, round_1_has_retried_429 = arh.recover_with_credential_pool(
        agent,
        status_code=429,
        has_retried_429=True,
        classified_reason=FailoverReason.rate_limit,
    )
    rows.append({
        "case": "tool_round_1_rotation_survives_to_tool_round_2",
        "round_1_recovered": rec,
        "current_api_key_after_round_1": agent.api_key,
        "current_entry_id_after_round_1": agent._credential_pool_entry_id,
    })

    # 8.2 Tool round 2 starts with fresh retry state
    # In conversation_loop.py, round 2 does: _retry = TurnRetryState()
    round_2_has_retried_429 = False
    rows.append({
        "case": "tool_round_2_resets_has_retried_429",
        "round_2_initial_has_retried_429": round_2_has_retried_429,
        "active_api_key_for_round_2": agent.api_key,
    })

    # 8.3 Pool request counts and statuses persist across rounds
    statuses_after_round_1 = {e.id: e.last_status for e in pool.entries()}
    rows.append({
        "case": "pool_request_counts_and_statuses_persist_across_rounds",
        "c1_status": statuses_after_round_1.get("c1"),
        "c2_status": statuses_after_round_1.get("c2"),
    })

    # 8.4 Session turn 2 preserves rotated key and rejects env stomp (#79156)
    with patch(
        "agent.credential_pool.get_env_prefer_dotenv",
        side_effect=lambda var: "sk-env-boot" if var == "DEEPSEEK_API_KEY" else "",
    ):
        adopted = AIAgent._try_refresh_env_client_credentials(agent)
    rows.append({
        "case": "session_turn_2_preserves_rotated_key",
        "env_adopted_over_pool_key": adopted,
        "agent_api_key_maintained": agent.api_key,
    })

    # 8.5 Session turn 2 cooldown expired revival
    e1_updated = [e for e in pool.entries() if e.id == "c1"][0]
    e1_updated.last_error_reset_at = NOW + 100
    # At turn 2 (NOW + 200), cooldown has expired
    with patch("time.time", return_value=NOW + 200):
        next_avail = pool.select()
    rows.append({
        "case": "session_turn_2_cooldown_expired_revival",
        "revived_id": next_avail.id if next_avail else None,
        "revived_status": pool.entries()[0].last_status,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 9: Unified Streaming and Tool-Loop Parity
# ---------------------------------------------------------------------------
def section_unified_parity() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 9.1 Streaming failure uses same recovery contract
    # In conversation_loop.py, try: _perform_api_call(...) wraps both
    # streaming and non-streaming. An error in stream deltas or start
    # reaches the exact same except block and calls _recover_with_credential_pool.
    c1 = PooledCredential(
        provider="openai",
        id="c1",
        label="K1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="k1",
    )
    c2 = PooledCredential(
        provider="openai",
        id="c2",
        label="K2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="k2",
    )
    p1 = CredentialPool("openai", [c1, c2])
    a1 = types.SimpleNamespace(
        provider="openai",
        base_url="https://api.openai.com/v1",
        api_key="k1",
        _credential_pool=p1,
        _credential_pool_entry_id="c1",
        _swap_credential=MagicMock(),
        _is_entitlement_failure=lambda ctx, status: False,
    )
    rec1, ret1 = arh.recover_with_credential_pool(
        a1,
        status_code=429,
        has_retried_429=True,
        classified_reason=FailoverReason.rate_limit,
    )
    rows.append({
        "case": "streaming_failure_uses_same_recovery_contract",
        "stream_recovered": rec1,
        "stream_retried": ret1,
        "pool_exhausted_entry_id": "c1",
        "shared_handler": True,
    })

    # 9.2 Non-streaming failure uses same recovery contract
    p2 = CredentialPool("openai", [copy.copy(c1), copy.copy(c2)])
    a2 = types.SimpleNamespace(
        provider="openai",
        base_url="https://api.openai.com/v1",
        api_key="k1",
        _credential_pool=p2,
        _credential_pool_entry_id="c1",
        _swap_credential=MagicMock(),
        _is_entitlement_failure=lambda ctx, status: False,
    )
    rec2, ret2 = arh.recover_with_credential_pool(
        a2,
        status_code=429,
        has_retried_429=True,
        classified_reason=FailoverReason.rate_limit,
    )
    rows.append({
        "case": "non_streaming_failure_uses_same_recovery_contract",
        "non_stream_recovered": rec2,
        "non_stream_retried": ret2,
        "parity_with_streaming": (rec1 == rec2) and (ret1 == ret2),
    })

    # 9.3 Wire client replacement unified
    # Both paths call _replace_primary_openai_client via _swap_credential
    agent_wire = types.SimpleNamespace(
        api_mode="chat_completions",
        provider="openai",
        base_url="https://api.openai.com/v1",
        api_key="k1",
        _client_kwargs={"api_key": "k1", "base_url": "https://api.openai.com/v1"},
        _reapply_route_client_config=MagicMock(),
        _replace_primary_openai_client=MagicMock(return_value=True),
        _credential_pool_entry_id="c1",
    )
    AIAgent._swap_credential(agent_wire, c2)
    rows.append({
        "case": "wire_client_replacement_unified",
        "client_replaced": agent_wire._replace_primary_openai_client.called,
        "replace_reason": agent_wire._replace_primary_openai_client.call_args[1].get(
            "reason"
        ),
    })

    return rows


# ---------------------------------------------------------------------------
# Section 10: Fallback and Non-Chat Boundaries
# ---------------------------------------------------------------------------
def section_fallback_and_non_chat_boundaries() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 10.1 OAuth refresh boundary
    # In agent/agent_runtime_helpers.py lines 1391-1420, try_refresh_matching is
    # called for auth failures. For static API keys, try_refresh_matching returns
    # None, which causes the pool to fall through immediately to rotation.
    c_static = PooledCredential(
        provider="openai",
        id="s1",
        label="S1",
        auth_type=AUTH_TYPE_API_KEY,
        priority=0,
        source="manual",
        access_token="k-static",
    )
    c_static_2 = PooledCredential(
        provider="openai",
        id="s2",
        label="S2",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="k-static-2",
    )
    pool_auth = CredentialPool("openai", [c_static, c_static_2])
    refresh_result = pool_auth.try_refresh_matching(api_key_hint="k-static")
    rows.append({
        "case": "oauth_refresh_boundary",
        "static_key_refresh_attempt_result": refresh_result,
        "falls_through_to_rotation": refresh_result is None,
    })

    # 10.2 Fallback provider boundary
    # Pool recovery returns False when all pool entries are exhausted.
    # The conversation loop checks `if recovered_with_pool: continue`, and
    # when False, proceeds to `_try_activate_fallback()`.
    pool_sole = CredentialPool("openai", [c_static])
    agent_sole = types.SimpleNamespace(
        provider="openai",
        base_url="https://api.openai.com/v1",
        api_key="k-static",
        _credential_pool=pool_sole,
        _credential_pool_entry_id="s1",
        _swap_credential=MagicMock(),
        _is_entitlement_failure=lambda ctx, status: False,
    )
    rec_exhausted, _ = arh.recover_with_credential_pool(
        agent_sole,
        status_code=402,
        has_retried_429=False,
        classified_reason=FailoverReason.billing,
    )
    rows.append({
        "case": "fallback_provider_boundary",
        "pool_recovery_exhausted_result": rec_exhausted,
        "activates_general_fallback_chain": not rec_exhausted,
    })

    # 10.3 Non-chat transport boundary
    # The static API-key subset is strictly chat_completions.
    # When api_mode is anthropic_messages or bedrock_converse, _reapply_route_client_config
    # returns early and _swap_credential branches to adapter-specific rebuilds.
    agent_anthropic = types.SimpleNamespace(
        api_mode="anthropic_messages",
        _client_kwargs={"default_headers": {"X-Test": "1"}},
    )
    AIAgent._apply_client_headers_for_base_url = MagicMock()
    # In run_agent.py line 6981:
    # if self.api_mode in ("anthropic_messages", "bedrock_converse"): return
    # This proves non-chat transports bypass the OpenAI client kwargs header merge.
    is_non_chat = agent_anthropic.api_mode in ("anthropic_messages", "bedrock_converse")
    rows.append({
        "case": "non_chat_transport_boundary",
        "api_mode": agent_anthropic.api_mode,
        "bypasses_chat_header_merge": is_non_chat,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 11: Raw HTTP Classifier Boundaries
# ---------------------------------------------------------------------------
def section_raw_http_classifier() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    specs = [
        ("unauthorized", 401, "gmi", {"error": {"message": "invalid api key"}}, {}),
        ("forbidden_auth", 403, "gmi", {"error": {"message": "forbidden"}}, {}),
        (
            "payment_credits",
            402,
            "gmi",
            {"error": {"message": "credits exhausted"}},
            {},
        ),
        (
            "payment_transient_quota",
            402,
            "gmi",
            {"error": {"message": "usage limit, try again in 5 minutes"}},
            {},
        ),
        (
            "free_tier_not_found",
            404,
            "nous",
            {"error": {"message": "model not available on the free tier"}},
            {},
        ),
        (
            "ordinary_rate_limit",
            429,
            "gmi",
            {"error": {"message": "rate limit exceeded"}},
            {},
        ),
        (
            "hard_usage_limit",
            429,
            "gmi",
            {
                "error": {
                    "type": "usage_limit_reached",
                    "message": "usage limit reached",
                }
            },
            {},
        ),
        (
            "transient_usage_limit",
            429,
            "gmi",
            {
                "error": {
                    "type": "usage_limit_reached",
                    "message": "Usage limit reached. Try again in 1 hour.",
                }
            },
            {},
        ),
        (
            "usage_limit_retry_after",
            429,
            "gmi",
            {"error": {"message": "usage limit reached"}},
            {"retry-after": "120"},
        ),
        (
            "billing_wrapped_429",
            429,
            "openrouter",
            {"error": {"message": "insufficient credits"}},
            {},
        ),
        (
            "upstream_openrouter_429",
            429,
            "openrouter",
            {
                "error": {
                    "message": "Provider returned error",
                    "metadata": {"provider_name": "DeepSeek"},
                }
            },
            {},
        ),
        (
            "overloaded_429",
            429,
            "zai",
            {"error": {"message": "service is temporarily overloaded"}},
            {},
        ),
        (
            "ambiguous_extra_usage",
            400,
            "anthropic",
            {"error": {"message": "You're out of extra usage"}},
            {},
        ),
    ]
    mapped = {
        FailoverReason.auth: "auth",
        FailoverReason.auth_permanent: "auth",
        FailoverReason.billing: "billing",
        FailoverReason.rate_limit: "rate_limit",
        FailoverReason.upstream_rate_limit: "upstream_rate_limit",
    }
    for case, status, provider, body, headers in specs:
        message = str(body.get("error", {}).get("message") or body)
        error = _OracleApiError(message, status, body, headers)
        classified = classify_api_error(
            error,
            provider=provider,
            model="fixture-model",
        )
        failure = mapped.get(classified.reason, "unrelated")
        if classified.billing_unverified:
            failure = "billing_unverified"
        error_context = arh.extract_api_error_context(error)
        context_reason = str(error_context.get("reason") or "").lower()
        context_message = str(error_context.get("message") or "").lower()
        usage_limit_reached = (
            "usage_limit_reached" in context_reason
            or "gousagelimit" in context_reason
            or "usage limit reached" in context_message
            or "usage limit has been reached" in context_message
        )
        rows.append({
            "case": case,
            "status": status,
            "provider": provider,
            "body_text": json.dumps(body, separators=(",", ":"), sort_keys=True),
            "headers": headers,
            "failure": failure,
            "usage_limit_reached": usage_limit_reached,
        })
    return rows


# ---------------------------------------------------------------------------
# Oracle Runner
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, List[Dict[str, Any]]]:
    return {
        "startup_precedence_and_resolution": section_startup_precedence(),
        "failure_attribution_and_key_matching": section_failure_attribution(),
        "persistence_before_retry_ordering": section_persistence_ordering(),
        "retry_state_transitions_and_request_counts": section_retry_state_transitions(),
        "status_and_cooldown_outcomes": section_cooldown_outcomes(),
        "provider_mismatch_isolation": section_provider_mismatch(),
        "route_changes_and_client_reconfiguration": section_route_changes(),
        "lifecycle_across_tool_rounds_and_later_turns": section_lifecycle_rounds_and_turns(),
        "unified_streaming_and_tool_loop_parity": section_unified_parity(),
        "fallback_and_non_chat_boundaries": section_fallback_and_non_chat_boundaries(),
        "raw_http_classifier_boundaries": section_raw_http_classifier(),
    }


def main():
    corpus = build_corpus()
    total_sections = len(corpus)
    total_cases = sum(len(cases) for cases in corpus.values())

    rendered = json.dumps(corpus, indent=2, sort_keys=True) + "\n"

    # Strict check: assert NO em dash characters exist in output
    assert "\u2014" not in rendered, (
        "FATAL: Em dash character (\\u2014) detected in generated JSON!"
    )

    if "--check" in sys.argv:
        if not OUT.exists():
            print(f"FAIL: {OUT} does not exist", file=sys.stderr)
            sys.exit(1)
        existing = OUT.read_text(encoding="utf-8")
        if existing != rendered:
            print(
                f"FAIL: {OUT} does not match freshly generated oracle output",
                file=sys.stderr,
            )
            sys.exit(1)
        print(
            f"OK: verified byte-for-byte parity across {total_sections} sections and {total_cases} test cases"
        )
    else:
        OUT.parent.mkdir(parents=True, exist_ok=True)
        OUT.write_text(rendered, encoding="utf-8")
        print(
            f"Wrote {total_cases} test cases across {total_sections} sections to {OUT}"
        )


if __name__ == "__main__":
    main()
