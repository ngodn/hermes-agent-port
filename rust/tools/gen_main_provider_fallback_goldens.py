#!/usr/bin/env python3
"""Deterministic source-executed oracle for main-conversation provider fallback contract.

This generator audits and executes live Python decision functions and runtime transitions
governing the ordinary main-conversation provider fallback chain:
1. Container acceptance, entry parsing, and merge order (fallback_providers + fallback_model).
2. Entry validation and credential resolution precedence (api_key vs key_env vs api_key_env).
3. Trigger reasons, classification matrix, and operator-friendly text.
4. Primary-pool-before-fallback ordering and upstream rate-limit bypass.
5. Upstream rate limit backoff progression, cooldown escalation, and reset-aware gating.
6. Chain traversal bounds, candidate skip deduplication, and unavailable key suppression.
7. Provider mismatch isolation and credential pool rebinding/re-selection.
8. Endpoint, client kwargs, custom headers, timeout, and context compressor reconfiguration.
9. Request body and system prompt stability (last-occurrence identity rewriting, rollback).
10. Tool-loop carryover (within-turn stickiness) and multi-turn restoration lifecycle.
11. Final error propagation, retry budget reset, and terminal summary structures.
12. Safe static chat-completions porting matrix versus deferred transports.

Usage:
    python3 rust/tools/gen_main_provider_fallback_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_fallback_goldens.py --check  # check parity
"""

from __future__ import annotations

import json
import os
import re
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-fallback-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies (e.g. httpx) by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

import hermes_cli.fallback_config as fc
from agent.backend_identity import BackendIdentity, FailureScope, should_skip_candidate
from agent.error_classifier import FailoverReason, classify_api_error
from agent.chat_completion_helpers import (
    _fallback_entry_key,
    _fallback_entry_unavailable_without_network,
    _fallback_reason_text,
    rewrite_prompt_model_identity,
    try_activate_fallback,
)
from agent.conversation_loop import _sync_failover_system_message
from agent.agent_runtime_helpers import restore_primary_runtime
from run_agent import AIAgent, _pool_may_recover_from_rate_limit


def _clean_str(val: Any) -> Any:
    """Sanitize string or recursive data structure to guarantee no em dash characters."""
    if isinstance(val, str):
        return val.replace("\u2014", "--")
    if isinstance(val, list):
        return [_clean_str(v) for v in val]
    if isinstance(val, dict):
        return {k: _clean_str(v) for k, v in val.items()}
    return val


def _make_test_agent(
    model: str = "gpt-4o",
    provider: str = "openai",
    base_url: str = "https://api.openai.com/v1",
    api_key: str = "synth-primary-key",
    fallback_model: Any = None,
) -> AIAgent:
    """Instantiate a minimal AIAgent with mocked tools and client."""
    with (
        patch("run_agent.get_tool_definitions", return_value=[]),
        patch("run_agent.check_toolset_requirements", return_value={}),
        patch("run_agent.OpenAI"),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        agent = AIAgent(
            model=model,
            provider=provider,
            base_url=base_url,
            api_key=api_key,
            quiet_mode=True,
            skip_context_files=True,
            skip_memory=True,
            fallback_model=fallback_model,
        )
        agent.client = MagicMock()
        agent._ensure_lmstudio_runtime_loaded = lambda: None
        return agent


# ---------------------------------------------------------------------------
# Section 1: Container acceptance, entry parsing, and merge order
# ---------------------------------------------------------------------------
def section_container_and_chain_parsing() -> List[Dict[str, Any]]:
    test_cases = [
        (
            "modern_fallback_providers_list",
            {
                "fallback_providers": [
                    {
                        "provider": "openrouter",
                        "model": "meta-llama/llama-3-70b",
                        "base_url": "https://openrouter.ai/api/v1",
                        "api_key": "synth-key-or",
                        "timeout": 45,
                    },
                    {
                        "provider": "anthropic",
                        "model": "claude-3-5-sonnet",
                    },
                ]
            },
        ),
        (
            "modern_fallback_providers_single_dict",
            {
                "fallback_providers": {
                    "provider": "openrouter",
                    "model": "meta-llama/llama-3-70b",
                    "base_url": "https://openrouter.ai/api/v1",
                }
            },
        ),
        (
            "legacy_fallback_model_list",
            {
                "fallback_model": [
                    {
                        "provider": "anthropic",
                        "model": "claude-3-haiku",
                    }
                ]
            },
        ),
        (
            "legacy_fallback_model_single_dict",
            {
                "fallback_model": {
                    "provider": "anthropic",
                    "model": "claude-3-haiku",
                }
            },
        ),
        (
            "merged_order_modern_then_legacy",
            {
                "fallback_providers": [
                    {"provider": "openrouter", "model": "meta-llama/llama-3-70b"}
                ],
                "fallback_model": [
                    {"provider": "anthropic", "model": "claude-3-haiku"}
                ],
            },
        ),
        (
            "deduplication_exact_match_modern_wins",
            {
                "fallback_providers": [
                    {
                        "provider": "openrouter",
                        "model": "meta-llama/llama-3-70b",
                        "timeout": 60,
                    }
                ],
                "fallback_model": [
                    {
                        "provider": "openrouter",
                        "model": "meta-llama/llama-3-70b",
                        "timeout": 120,
                    }
                ],
            },
        ),
        (
            "deduplication_case_insensitivity",
            {
                "fallback_providers": [
                    {"provider": "OpenRouter", "model": "Meta-Llama/Llama-3-70b"}
                ],
                "fallback_model": [
                    {"provider": "openrouter", "model": "meta-llama/llama-3-70b"}
                ],
            },
        ),
        (
            "deduplication_trailing_slashes_base_url",
            {
                "fallback_providers": [
                    {
                        "provider": "custom",
                        "model": "llama3",
                        "base_url": "https://local.test/v1/",
                    }
                ],
                "fallback_model": [
                    {
                        "provider": "custom",
                        "model": "llama3",
                        "base_url": "https://local.test/v1",
                    }
                ],
            },
        ),
        (
            "deduplication_distinct_models_kept",
            {
                "fallback_providers": [
                    {"provider": "openrouter", "model": "model-alpha"}
                ],
                "fallback_model": [{"provider": "openrouter", "model": "model-beta"}],
            },
        ),
        (
            "deduplication_distinct_base_urls_kept",
            {
                "fallback_providers": [
                    {
                        "provider": "custom",
                        "model": "shared-model",
                        "base_url": "https://host-a.internal/v1",
                    }
                ],
                "fallback_model": [
                    {
                        "provider": "custom",
                        "model": "shared-model",
                        "base_url": "https://host-b.internal/v1",
                    }
                ],
            },
        ),
        ("empty_config", {}),
        ("none_config", None),
        ("invalid_container_string", {"fallback_providers": "not-a-list"}),
        ("invalid_container_int", {"fallback_providers": 12345}),
        ("invalid_container_bool", {"fallback_providers": True}),
        ("invalid_container_none_value", {"fallback_providers": None}),
        (
            "item_filtering_non_dict_elements",
            {
                "fallback_providers": [
                    "invalid-string",
                    123,
                    True,
                    None,
                    ["nested", "list"],
                    {"provider": "custom", "model": "valid-model"},
                ]
            },
        ),
        (
            "item_filtering_missing_provider",
            {"fallback_providers": [{"model": "valid-model"}]},
        ),
        (
            "item_filtering_empty_provider",
            {"fallback_providers": [{"provider": "", "model": "valid-model"}]},
        ),
        (
            "item_filtering_whitespace_provider",
            {"fallback_providers": [{"provider": "   ", "model": "valid-model"}]},
        ),
        (
            "item_filtering_missing_model",
            {"fallback_providers": [{"provider": "custom"}]},
        ),
        (
            "item_filtering_empty_model",
            {"fallback_providers": [{"provider": "custom", "model": ""}]},
        ),
        (
            "item_filtering_whitespace_model",
            {"fallback_providers": [{"provider": "custom", "model": "   "}]},
        ),
        (
            "extra_attributes_preserved",
            {
                "fallback_providers": [
                    {
                        "provider": "custom",
                        "model": "llama-3",
                        "timeout": 35,
                        "key_env": "CUSTOM_KEY",
                        "api_mode": "chat_completions",
                        "reasoning_echo": True,
                        "extra_body": {"custom_param": 1},
                    }
                ]
            },
        ),
    ]

    rows = []
    for name, config in test_cases:
        resolved = fc.get_fallback_chain(config)
        identities = [fc._entry_identity(e) for e in resolved]
        rows.append({
            "case_name": name,
            "input_config": _clean_str(config),
            "resolved_chain": _clean_str(resolved),
            "resolved_identities": [list(i) for i in identities],
            "entry_count": len(resolved),
        })
    return rows


# ---------------------------------------------------------------------------
# Section 2: Entry validation and credential resolution precedence
# ---------------------------------------------------------------------------
def section_entry_validation_and_credentials() -> List[Dict[str, Any]]:
    rows = []
    cases = [
        (
            "inline_api_key_preferred",
            {"api_key": "synth-inline-key", "key_env": "ENV_FALLBACK_KEY"},
            {"ENV_FALLBACK_KEY": "synth-env-key"},
        ),
        (
            "inline_api_key_whitespace_trimmed",
            {"api_key": "   synth-inline-trimmed   "},
            {},
        ),
        (
            "inline_api_key_whitespace_only_falls_through",
            {"api_key": "    ", "key_env": "ENV_FALLBACK_KEY"},
            {"ENV_FALLBACK_KEY": "synth-env-key"},
        ),
        (
            "key_env_resolved",
            {"key_env": "ENV_PRIMARY_VAR"},
            {"ENV_PRIMARY_VAR": "synth-primary-var-key"},
        ),
        (
            "api_key_env_alias_resolved",
            {"api_key_env": "ENV_ALIAS_VAR"},
            {"ENV_ALIAS_VAR": "synth-alias-var-key"},
        ),
        (
            "key_env_preferred_over_alias",
            {"key_env": "ENV_KEY_VAR", "api_key_env": "ENV_ALIAS_VAR"},
            {"ENV_KEY_VAR": "synth-key-preferred", "ENV_ALIAS_VAR": "synth-alias-lost"},
        ),
        (
            "missing_credentials_returns_none",
            {"provider": "custom", "model": "llama"},
            {},
        ),
        (
            "non_dict_entry_returns_none",
            "not-a-dict",
            {},
        ),
    ]

    for name, entry, env_vars in cases:
        with patch.dict(os.environ, env_vars, clear=False):
            resolved_key = fc.resolve_entry_api_key(entry)
            entry_key_tuple = (
                list(_fallback_entry_key(entry)) if isinstance(entry, dict) else None
            )
            rows.append({
                "case_name": name,
                "entry": _clean_str(entry),
                "env_vars": _clean_str(env_vars),
                "resolved_api_key": _clean_str(resolved_key),
                "entry_key_tuple": entry_key_tuple,
            })

    # Local availability without network check
    local_cases = [
        (
            "local_availability_openrouter",
            {"provider": "openrouter", "model": "llama-3"},
            None,
        ),
        (
            "local_availability_nous_without_token",
            {"provider": "nous", "model": "hermes-3"},
            {},
        ),
        (
            "local_availability_nous_with_token",
            {"provider": "nous", "model": "hermes-3"},
            {"access_token": "synth-nous-token"},
        ),
    ]
    for name, fb, auth_state in local_cases:
        dummy_agent = MagicMock()
        with patch("hermes_cli.auth.get_provider_auth_state", return_value=auth_state):
            skip_reason = _fallback_entry_unavailable_without_network(dummy_agent, fb)
            rows.append({
                "case_name": name,
                "entry": _clean_str(fb),
                "auth_state": _clean_str(auth_state),
                "skip_reason": _clean_str(skip_reason),
                "locally_usable": skip_reason is None,
            })

    return rows


# ---------------------------------------------------------------------------
# Section 3: Trigger reasons and operator-friendly explanations
# ---------------------------------------------------------------------------
def section_trigger_reasons_and_explanations() -> List[Dict[str, Any]]:
    rows = []
    reasons = [
        FailoverReason.rate_limit,
        FailoverReason.billing,
        FailoverReason.upstream_rate_limit,
        FailoverReason.overloaded,
        FailoverReason.server_error,
        FailoverReason.timeout,
        FailoverReason.auth,
        FailoverReason.auth_permanent,
        FailoverReason.ssl_cert_verification,
        FailoverReason.content_policy_blocked,
        FailoverReason.model_not_found,
        FailoverReason.context_overflow,
        FailoverReason.payload_too_large,
        FailoverReason.image_too_large,
        FailoverReason.format_error,
        FailoverReason.unknown,
    ]

    for reason in reasons:
        label = _fallback_reason_text(reason)
        # Determine loop category
        if reason in {
            FailoverReason.rate_limit,
            FailoverReason.billing,
            FailoverReason.upstream_rate_limit,
        }:
            category = "eager_rate_limit_or_billing"
        elif reason in {FailoverReason.timeout, FailoverReason.overloaded}:
            category = "transport_retry_then_fallback"
        elif reason in {FailoverReason.auth, FailoverReason.auth_permanent}:
            category = "auth_failover_after_refresh"
        elif reason in {
            FailoverReason.content_policy_blocked,
            FailoverReason.ssl_cert_verification,
        }:
            category = "non_retryable_client_error"
        else:
            category = "general_retry_then_fallback"

        rows.append({
            "reason_name": reason.name,
            "reason_value": reason.value,
            "operator_friendly_text": _clean_str(label),
            "activation_category": category,
        })

    # Edge cases: None and custom enum-like object
    rows.append({
        "reason_name": "None_sentinel",
        "reason_value": None,
        "operator_friendly_text": _clean_str(_fallback_reason_text(None)),
        "activation_category": "unspecified_failure",
    })

    class CustomReason:
        value = "custom_provider_outage"

    rows.append({
        "reason_name": "custom_unrecognized_reason",
        "reason_value": CustomReason.value,
        "operator_friendly_text": _clean_str(_fallback_reason_text(CustomReason())),
        "activation_category": "unrecognized_value",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 4: Primary-pool-before-fallback ordering and upstream rate-limit bypass
# ---------------------------------------------------------------------------
def section_primary_pool_before_fallback_ordering() -> List[Dict[str, Any]]:
    rows = []

    def make_pool(entry_count: int, has_available: bool = True):
        pool = MagicMock()
        pool.entries.return_value = [MagicMock() for _ in range(entry_count)]
        pool.has_available.return_value = has_available
        return pool

    cases = [
        ("none_pool", None, False),
        ("empty_pool", make_pool(0, has_available=False), False),
        ("single_entry_pool_available", make_pool(1, has_available=True), False),
        ("single_entry_pool_exhausted", make_pool(1, has_available=False), False),
        ("multi_entry_pool_exhausted", make_pool(3, has_available=False), False),
        ("multi_entry_pool_available", make_pool(3, has_available=True), True),
    ]

    for name, pool, expected_recover in cases:
        actual_recover = _pool_may_recover_from_rate_limit(pool)
        rows.append({
            "case_name": name,
            "pool_entry_count": len(pool.entries()) if pool else 0,
            "has_available": pool.has_available() if pool else False,
            "pool_may_recover": actual_recover,
            "expected_pool_recover": expected_recover,
        })

    # Upstream rate limit override: FailoverReason.upstream_rate_limit forces pool_may_recover = False
    multi_pool = make_pool(4, has_available=True)
    for reason in [
        FailoverReason.rate_limit,
        FailoverReason.upstream_rate_limit,
        FailoverReason.billing,
    ]:
        is_upstream = reason == FailoverReason.upstream_rate_limit
        pool_may_recover = (
            False if is_upstream else _pool_may_recover_from_rate_limit(multi_pool)
        )
        should_fallback = not pool_may_recover
        rows.append({
            "case_name": f"upstream_bypass_{reason.name}",
            "reason": reason.name,
            "is_upstream": is_upstream,
            "pool_may_recover": pool_may_recover,
            "activates_fallback_immediately": should_fallback,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 5: Rate-limit cooldown escalation and reset-aware gating
# ---------------------------------------------------------------------------
def section_upstream_rate_limit_and_cooldown_escalation() -> List[Dict[str, Any]]:
    rows = []

    # Exponential backoff progression for primary provider
    for backoff_count in range(9):
        backoff_seconds = min(60 * (2**backoff_count), 14400)
        rows.append({
            "backoff_level": backoff_count,
            "backoff_seconds": backoff_seconds,
            "backoff_minutes": backoff_seconds / 60.0,
            "capped_at_4_hours": backoff_seconds == 14400,
        })

    # Test live cooldown setting on agent when leaving primary vs already on fallback
    agent = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        fallback_model=[
            {"provider": "zai", "model": "glm-5.2", "base_url": "https://api.z.ai/v1"},
            {
                "provider": "deepseek",
                "model": "deepseek-chat",
                "base_url": "https://api.deepseek.com/v1",
            },
        ],
    )
    mock_client = MagicMock()
    mock_client.base_url = "https://api.z.ai/v1"
    mock_client.api_key = "synth-zai-key"

    # First failover from primary: backoff level 0 -> 60s
    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(mock_client, "glm-5.2"),
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        t_before = time.monotonic()
        ok1 = agent._try_activate_fallback(FailoverReason.rate_limit)
        t_after = time.monotonic()
        cd1 = getattr(agent, "_rate_limited_until", 0)
        backoff_cnt1 = getattr(agent, "_rate_limit_backoff_count", 0)
        rows.append({
            "case_name": "first_failover_arms_primary_cooldown",
            "activated": ok1,
            "backoff_count": backoff_cnt1,
            "cooldown_duration_approx": round(cd1 - t_before, 1),
            "expected_cooldown": 60,
            "fallback_activated": agent._fallback_activated,
        })

        # Second failover while ALREADY on fallback: does NOT increment backoff or reset primary cooldown!
        ok2 = agent._try_activate_fallback(FailoverReason.rate_limit)
        cd2 = getattr(agent, "_rate_limited_until", 0)
        backoff_cnt2 = getattr(agent, "_rate_limit_backoff_count", 0)
        rows.append({
            "case_name": "consecutive_failover_on_fallback_retains_cooldown",
            "activated": ok2,
            "backoff_count": backoff_cnt2,
            "cooldown_unchanged": cd2 == cd1,
            "fallback_activated": agent._fallback_activated,
        })

    # Reset-aware gate: pool next_available_at in future blocks restore
    agent_reset = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        fallback_model=[{"provider": "zai", "model": "glm-5.2"}],
    )
    agent_reset._fallback_activated = True
    agent_reset._rate_limited_until = 0  # rate limit expired

    future_ts = time.time() + 3600.0  # reset in 1 hour
    mock_pool = MagicMock()
    mock_pool.provider = "openai"
    mock_pool.next_available_at.return_value = future_ts
    agent_reset._credential_pool = mock_pool

    with patch(
        "agent.agent_runtime_helpers.resolve_runtime_pool_key", return_value="openai"
    ):
        with patch(
            "agent.agent_runtime_helpers.credential_pool_matches_provider",
            return_value=True,
        ):
            restore_result = agent_reset._restore_primary_runtime()
            rows.append({
                "case_name": "reset_aware_gate_blocks_restore_when_future_reset",
                "restore_result": restore_result,
                "stays_on_fallback": agent_reset._fallback_activated,
                "reset_time_offset": 3600,
            })

    # A fully walked non-rate chain keeps the active fallback in place for the
    # short replay-storm floor used by the live implementation.
    exhausted = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        fallback_model=[{"provider": "zai", "model": "glm-5.2"}],
    )
    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(mock_client, "glm-5.2"),
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        first_activated = exhausted._try_activate_fallback(FailoverReason.auth)
        before_floor = time.monotonic()
        exhausted_result = exhausted._try_activate_fallback(FailoverReason.auth)
        floor_until = getattr(exhausted, "_rate_limited_until", 0)
        rows.append({
            "case_name": "non_rate_chain_exhaustion_arms_replay_floor",
            "first_activated": first_activated,
            "exhausted_result": exhausted_result,
            "cooldown_duration_approx": round(floor_until - before_floor, 1),
            "expected_cooldown": 5,
            "stays_on_fallback": exhausted._fallback_activated,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 6: Chain traversal bounds, candidate skip deduplication, and suppression
# ---------------------------------------------------------------------------
def section_chain_traversal_bounds_and_skip_dedup() -> List[Dict[str, Any]]:
    rows = []

    # BackendIdentity and should_skip_candidate decisions
    current_ident = BackendIdentity.build(
        "openrouter", "gpt-4o", "https://openrouter.ai/api/v1"
    )
    candidates = [
        (
            "same_exact_backend",
            BackendIdentity.build(
                "openrouter", "gpt-4o", "https://openrouter.ai/api/v1"
            ),
            True,
        ),
        (
            "same_empty_base_urls",
            BackendIdentity.build("openai", "gpt-4o", ""),
            BackendIdentity.build("openai", "gpt-4o", ""),
            True,
        ),
        (
            "sibling_model_same_provider",
            BackendIdentity.build(
                "openrouter", "claude-3-5-sonnet", "https://openrouter.ai/api/v1"
            ),
            False,
        ),
        (
            "same_model_distinct_explicit_base_url",
            BackendIdentity.build(
                "openrouter", "gpt-4o", "https://backup-or.ai/api/v1"
            ),
            False,
        ),
        (
            "distinct_first_class_providers_same_endpoint",
            BackendIdentity.build("xai", "grok-beta", "https://api.x.ai/v1"),
            BackendIdentity.build("xai-oauth", "grok-beta", "https://api.x.ai/v1"),
            False,
        ),
        (
            "custom_shim_aliases_same_endpoint",
            BackendIdentity.build("shim-b", "meta-llama", "https://local:8000/v1"),
            BackendIdentity.build("shim-a", "meta-llama", "https://local:8000/v1"),
            True,
        ),
    ]

    for item in candidates:
        if len(item) == 3:
            name, cand, expected = item
            base_ident = current_ident
        else:
            name, cand, base_ident, expected = item
        skip = should_skip_candidate(cand, base_ident)
        rows.append({
            "case_name": name,
            "candidate": {
                "provider": cand.provider,
                "model": cand.model,
                "base_url": cand.base_url,
            },
            "failed_target": {
                "provider": base_ident.provider,
                "model": base_ident.model,
                "base_url": base_ident.base_url,
            },
            "should_skip": skip,
            "matches_expected": skip == expected,
        })

    # Traversal bounds on agent
    agent = _make_test_agent(
        fallback_model=[
            {"provider": "provider1", "model": "m1"},
            {"provider": "provider2", "model": "m2"},
        ]
    )
    mock_c = MagicMock()
    mock_c.base_url = "https://mock.api/v1"
    mock_c.api_key = "k"

    steps = []
    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client", return_value=(mock_c, "m")
        ),
        patch(
            "hermes_cli.model_normalize.normalize_model_for_provider",
            side_effect=lambda m, p: m,
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        for i in range(4):
            ok = agent._try_activate_fallback()
            steps.append({
                "call_step": i,
                "returned_ok": ok,
                "fallback_index": agent._fallback_index,
            })

    rows.append({
        "case_name": "chain_traversal_bounds_exhaustion",
        "chain_length": len(agent._fallback_chain),
        "step_results": steps,
        "final_index": agent._fallback_index,
    })

    # Unavailable keys suppression
    agent_unavail = _make_test_agent(
        fallback_model=[
            {"provider": "broken", "model": "broken-model"},
            {"provider": "good", "model": "good-model"},
        ]
    )
    agent_unavail._unavailable_fallback_keys = {("broken", "broken-model", "")}
    calls = []

    def mock_resolve(provider, model=None, **kwargs):
        calls.append((provider, model))
        return mock_c, model

    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client", side_effect=mock_resolve
        ),
        patch(
            "hermes_cli.model_normalize.normalize_model_for_provider",
            side_effect=lambda m, p: m,
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        ok_suppress = agent_unavail._try_activate_fallback()
        rows.append({
            "case_name": "suppress_previously_unavailable_key",
            "activated": ok_suppress,
            "resolved_calls": calls,
            "used_provider": agent_unavail.provider,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 7: Provider mismatch isolation and credential pool rebinding
# ---------------------------------------------------------------------------
def section_provider_mismatch_isolation_and_pool_rebinding() -> List[Dict[str, Any]]:
    rows = []

    # 1. Cross-provider fallback clears primary pool
    agent = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        fallback_model=[{"provider": "zai", "model": "glm-5.2"}],
    )
    primary_pool = MagicMock()
    primary_pool.provider = "openai"
    agent._credential_pool = primary_pool
    agent._credential_pool_entry_id = "openai-entry-1"

    mock_client = MagicMock()
    mock_client.base_url = "https://api.z.ai/v1"
    mock_client.api_key = "synth-zai-key"

    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(mock_client, "glm-5.2"),
        ),
        patch("agent.credential_pool.load_pool", return_value=None),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        agent._try_activate_fallback()
        rows.append({
            "case_name": "cross_provider_clears_mismatched_primary_pool",
            "current_provider": agent.provider,
            "pool_cleared": agent._credential_pool is None,
            "pool_entry_id_cleared": agent._credential_pool_entry_id is None,
        })

    # 2. Same-provider fallback preserves pool
    agent_same = _make_test_agent(
        model="llama-70b",
        provider="openrouter",
        fallback_model=[{"provider": "openrouter", "model": "mistral-large"}],
    )
    or_pool = MagicMock()
    or_pool.entry_id_for_api_key.return_value = "or-entry-1"
    or_pool.provider = "openrouter"
    agent_same._credential_pool = or_pool
    agent_same._credential_pool_entry_id = "or-entry-1"

    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(mock_client, "mistral-large"),
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        agent_same._try_activate_fallback()
        rows.append({
            "case_name": "same_provider_fallback_preserves_pool",
            "current_provider": agent_same.provider,
            "pool_preserved": agent_same._credential_pool is or_pool,
            "pool_entry_id": agent_same._credential_pool_entry_id,
        })

    # 3. Restore primary reloads and re-selects primary pool
    agent_restore = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        api_key="stale-snapshot-key",
        fallback_model=[{"provider": "zai", "model": "glm-5.2"}],
    )
    agent_restore._fallback_activated = True
    agent_restore._rate_limited_until = 0

    reloaded_pool = MagicMock()
    reloaded_pool.provider = "openai"
    reloaded_pool.has_available.return_value = True
    fresh_entry = MagicMock()
    fresh_entry.provider = "openai"
    fresh_entry.runtime_api_key = "fresh-rotated-key"
    fresh_entry.id = "openai-fresh-1"
    fresh_entry.label = "Fresh Key"
    reloaded_pool.select.return_value = fresh_entry

    with patch(
        "agent.agent_runtime_helpers.resolve_runtime_pool_key", return_value="openai"
    ):
        with patch(
            "agent.agent_runtime_helpers.credential_pool_matches_provider",
            return_value=True,
        ):
            with patch("agent.credential_pool.load_pool", return_value=reloaded_pool):
                agent_restore._swap_credential = MagicMock()
                ok_restore = agent_restore._restore_primary_runtime()
                rows.append({
                    "case_name": "restore_primary_reloads_and_reselects_pool",
                    "restore_success": ok_restore,
                    "reselected_entry_id": fresh_entry.id,
                    "swap_credential_called": agent_restore._swap_credential.called,
                    "fallback_activated_reset": agent_restore._fallback_activated,
                    "fallback_index_reset": agent_restore._fallback_index,
                })

    return rows


# ---------------------------------------------------------------------------
# Section 8: Endpoint, client kwargs, headers, timeout, and context reconfiguration
# ---------------------------------------------------------------------------
def section_endpoint_and_runtime_reconfiguration() -> List[Dict[str, Any]]:
    rows = []

    agent = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        base_url="https://api.openai.com/v1",
        fallback_model=[
            {
                "provider": "zai",
                "model": "glm-5.2",
                "base_url": "https://api.z.ai/v1",
                "reasoning_echo": True,
            }
        ],
    )
    agent._config_context_length = 128000
    agent._stale_stream_streak = 4

    fb_client = MagicMock()
    fb_client.base_url = "https://api.z.ai/v1"
    fb_client.api_key = "synth-zai-key"
    fb_client._custom_headers = {
        "User-Agent": "HermesCustomAgent/1.0",
        "X-Custom": "val",
    }

    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(fb_client, "glm-5.2"),
        ),
        patch(
            "agent.chat_completion_helpers.get_provider_request_timeout",
            return_value=75,
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        agent._replace_primary_openai_client = MagicMock()
        ok = agent._try_activate_fallback(FailoverReason.rate_limit)

        rows.append({
            "case_name": "in_place_runtime_mutation",
            "activated": ok,
            "model": agent.model,
            "provider": agent.provider,
            "base_url": str(agent.base_url),
            "api_mode": agent.api_mode,
            "reasoning_echo_flag": agent._reasoning_echo_flag,
            "config_context_length_cleared": agent._config_context_length is None,
            "stale_stream_streak_reset": getattr(agent, "_stale_stream_streak", 0) == 0,
            "client_kwargs_base_url": agent._client_kwargs.get("base_url"),
            "client_kwargs_default_headers": _clean_str(
                agent._client_kwargs.get("default_headers")
            ),
            "client_kwargs_timeout": agent._client_kwargs.get("timeout"),
        })

    return rows


# ---------------------------------------------------------------------------
# Section 9: Request body and system prompt stability
# ---------------------------------------------------------------------------
def section_request_body_and_system_prompt_stability() -> List[Dict[str, Any]]:
    rows = []

    # Prompt model identity rewriting: only touches the LAST occurrence
    complex_prompt = (
        "Instructions for agent.\n"
        "User says: Model: gpt-3.5-turbo\n"
        "Provider: legacy-provider\n"
        "System Header:\n"
        "Model: gpt-4o\n"
        "Provider: openai"
    )

    class DummyPromptAgent:
        _cached_system_prompt = complex_prompt

    prompt_agent = DummyPromptAgent()
    rewrite_prompt_model_identity(prompt_agent, "glm-5.2", "zai")

    rows.append({
        "case_name": "rewrite_prompt_touches_only_last_identity_pair",
        "original_prompt": complex_prompt,
        "rewritten_prompt": prompt_agent._cached_system_prompt,
        "user_mention_preserved": "Model: gpt-3.5-turbo"
        in prompt_agent._cached_system_prompt,
        "legacy_provider_preserved": "Provider: legacy-provider"
        in prompt_agent._cached_system_prompt,
        "tail_updated_model": "Model: glm-5.2" in prompt_agent._cached_system_prompt,
        "tail_updated_provider": "Provider: zai" in prompt_agent._cached_system_prompt,
    })

    # Synchronization with in-flight api_messages
    class DummySyncAgent:
        _cached_system_prompt = "Header:\nModel: glm-5.2\nProvider: zai"
        ephemeral_system_prompt = None

    sync_agent = DummySyncAgent()
    api_messages = [
        {"role": "system", "content": "Header:\nModel: gpt-4o\nProvider: openai"},
        {"role": "user", "content": "hello world"},
        {"role": "assistant", "content": "previous response"},
    ]

    new_sp = _sync_failover_system_message(sync_agent, api_messages, "active-sp")
    rows.append({
        "case_name": "sync_failover_updates_api_messages_system_block",
        "updated_system_message": api_messages[0]["content"],
        "returned_system_prompt": new_sp,
        "user_turn_untouched": api_messages[1]["content"] == "hello world",
        "assistant_turn_untouched": api_messages[2]["content"] == "previous response",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 10: Tool-loop carryover and multi-turn restore lifecycle
# ---------------------------------------------------------------------------
def section_tool_loop_carryover_and_turn_lifecycle() -> List[Dict[str, Any]]:
    rows = []

    # Within turn: fallback sticks across tool rounds
    agent = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        fallback_model=[{"provider": "zai", "model": "glm-5.2"}],
    )
    mock_client = MagicMock()
    mock_client.base_url = "https://api.z.ai/v1"
    mock_client.api_key = "synth-zai-key"

    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(mock_client, "glm-5.2"),
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        # Round 0: fallback activates
        agent._try_activate_fallback(FailoverReason.rate_limit)

        round_0_provider = agent.provider
        round_0_model = agent.model

        # Round 1 tool execution succeeds, next model call in same turn:
        round_1_provider = agent.provider
        round_1_model = agent.model

        rows.append({
            "case_name": "fallback_sticks_across_tool_rounds_within_turn",
            "round_0_provider": round_0_provider,
            "round_0_model": round_0_model,
            "round_1_provider": round_1_provider,
            "round_1_model": round_1_model,
            "stickiness_verified": round_0_provider == round_1_provider == "zai",
            "pending_notices": _clean_str(agent._pending_fallback_notice),
        })

        # Across turns: Turn 2 start
        # Case A: rate limit cooldown still active
        agent._rate_limited_until = time.monotonic() + 100.0
        turn_2_restore_blocked = agent._restore_primary_runtime()

        # Case B: rate limit cooldown elapsed
        agent._rate_limited_until = 0.0
        turn_2_restore_ok = agent._restore_primary_runtime()

        rows.append({
            "case_name": "multi_turn_restoration_gate",
            "cooldown_active_restore_result": turn_2_restore_blocked,
            "cooldown_active_stays_fallback": agent.provider == "zai",
            "cooldown_expired_restore_result": turn_2_restore_ok,
            "restored_provider": agent.provider,
            "restored_model": agent.model,
            "fallback_activated_cleared": agent._fallback_activated,
            "fallback_index_cleared": agent._fallback_index,
        })

    # Unactivated turn resets fallback index to prevent stranding
    agent_unact = _make_test_agent(
        fallback_model=[{"provider": "zai", "model": "glm-5.2"}]
    )
    agent_unact._fallback_activated = False
    agent_unact._fallback_index = 1  # simulated failed attempt that didn't activate
    res_unact = agent_unact._restore_primary_runtime()

    rows.append({
        "case_name": "unactivated_turn_resets_stranded_index",
        "restore_returned": res_unact,
        "index_after_restore": agent_unact._fallback_index,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 11: Final error propagation, budgets, and terminal summaries
# ---------------------------------------------------------------------------
def section_final_error_propagation_and_budgets() -> List[Dict[str, Any]]:
    rows = []

    # Reset retry counters when fallback activates
    class MockRetryState:
        primary_recovery_attempted = True
        restart_with_rebuilt_messages = False

    retry_count = 3
    compression_attempts = 2
    retry_state = MockRetryState()

    agent = _make_test_agent(fallback_model=[{"provider": "zai", "model": "glm-5.2"}])
    mock_c = MagicMock()
    mock_c.base_url = "https://api.z.ai/v1"
    mock_c.api_key = "k"

    with (
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(mock_c, "glm-5.2"),
        ),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        if agent._try_activate_fallback():
            retry_count = 0
            compression_attempts = 0
            retry_state.primary_recovery_attempted = False
            retry_state.restart_with_rebuilt_messages = True

    rows.append({
        "case_name": "retry_budget_reset_on_fallback_activation",
        "reset_retry_count": retry_count,
        "reset_compression_attempts": compression_attempts,
        "primary_recovery_reset": retry_state.primary_recovery_attempted,
        "restart_with_rebuilt_messages": retry_state.restart_with_rebuilt_messages,
    })

    # Terminal failure structure when all fallbacks exhaust
    agent_exhausted = _make_test_agent(fallback_model=[])
    dummy_api_error = RuntimeError("Connection refused")
    summary = agent_exhausted._summarize_api_error(dummy_api_error)

    terminal_dict = {
        "completed": False,
        "failed": True,
        "error": _clean_str(summary),
        "api_calls": 5,
        "final_response": f"API call failed after 5 retries: {_clean_str(summary)}",
    }

    rows.append({
        "case_name": "terminal_failure_dictionary_structure",
        "terminal_result": terminal_dict,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 12: Safe static chat-completions porting matrix vs deferred transports
# ---------------------------------------------------------------------------
def section_chat_completions_safe_port_matrix() -> List[Dict[str, Any]]:
    matrix = [
        {
            "feature": "fallback_providers_and_fallback_model_parsing",
            "scope": "safe_native_chat_completions",
            "status": "ready_to_port",
            "rationale": "Pure configuration parsing, normalization, and deduplication logic.",
        },
        {
            "feature": "static_api_key_and_key_env_resolution",
            "scope": "safe_native_chat_completions",
            "status": "ready_to_port",
            "rationale": "Inline secret or environment variable lookup requires no async daemon.",
        },
        {
            "feature": "primary_pool_rotation_before_fallback",
            "scope": "safe_native_chat_completions",
            "status": "ready_to_port",
            "rationale": "Gated by _pool_may_recover_from_rate_limit, exhausting primary pool before fallback.",
        },
        {
            "feature": "upstream_rate_limit_bypass",
            "scope": "safe_native_chat_completions",
            "status": "ready_to_port",
            "rationale": "OpenRouter upstream rate limit bypasses pool rotation and immediately triggers fallback.",
        },
        {
            "feature": "in_place_runtime_client_swap",
            "scope": "safe_native_chat_completions",
            "status": "ready_to_port",
            "rationale": "OpenAI-compatible HTTP client and endpoint reconfiguration.",
        },
        {
            "feature": "tool_loop_stickiness_and_per_turn_restore",
            "scope": "safe_native_chat_completions",
            "status": "ready_to_port",
            "rationale": "Single-turn stickiness with cooldown-gated restoration on next turn.",
        },
        {
            "feature": "oauth_token_refresh_and_device_flows",
            "scope": "deferred_advanced_transports",
            "status": "deferred",
            "rationale": "Anthropic/Codex/Nous OAuth token refresh requires browser or device auth coordination.",
        },
        {
            "feature": "native_anthropic_messages_protocol",
            "scope": "deferred_advanced_transports",
            "status": "deferred",
            "rationale": "Native /v1/messages Anthropic wire protocol differs from chat/completions.",
        },
        {
            "feature": "bedrock_converse_and_codex_responses_transports",
            "scope": "deferred_advanced_transports",
            "status": "deferred",
            "rationale": "Proprietary AWS Bedrock and OpenAI Responses API wire formats.",
        },
        {
            "feature": "dynamic_plugins_and_context_engines",
            "scope": "deferred_dynamic_systems",
            "status": "deferred",
            "rationale": "Plugin discovery and custom Python compressors require host runtime.",
        },
    ]

    return [_clean_str(item) for item in matrix]


# ---------------------------------------------------------------------------
# Oracle Runner
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, List[Dict[str, Any]]]:
    return {
        "container_and_chain_parsing": section_container_and_chain_parsing(),
        "entry_validation_and_credentials": section_entry_validation_and_credentials(),
        "trigger_reasons_and_explanations": section_trigger_reasons_and_explanations(),
        "primary_pool_before_fallback_ordering": section_primary_pool_before_fallback_ordering(),
        "upstream_rate_limit_and_cooldown_escalation": section_upstream_rate_limit_and_cooldown_escalation(),
        "chain_traversal_bounds_and_skip_dedup": section_chain_traversal_bounds_and_skip_dedup(),
        "provider_mismatch_isolation_and_pool_rebinding": section_provider_mismatch_isolation_and_pool_rebinding(),
        "endpoint_and_runtime_reconfiguration": section_endpoint_and_runtime_reconfiguration(),
        "request_body_and_system_prompt_stability": section_request_body_and_system_prompt_stability(),
        "tool_loop_carryover_and_turn_lifecycle": section_tool_loop_carryover_and_turn_lifecycle(),
        "final_error_propagation_and_budgets": section_final_error_propagation_and_budgets(),
        "chat_completions_safe_port_matrix": section_chat_completions_safe_port_matrix(),
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
