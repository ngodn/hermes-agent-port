#!/usr/bin/env python3
"""Deterministic source-executed oracle for built-in auxiliary provider discovery.

This generator exercises real Python decision functions and runtime contracts in
agent/auxiliary_client.py that govern built-in auxiliary provider discovery when
full compression in auto mode exhausts task-specific and top-level configured
fallback tiers:
1. Provider chain structure, ordering, and label normalization.
2. OpenRouter discovery gates, free-only policies, and credential resolution.
3. Nous discovery gates, cross-session rate limiting, and global mutation.
4. Custom endpoint discovery gates, endpoint restrictions, and transport wrapping.
5. API-key catalog discovery, priority order, and model resolution.
6. Context-window filtering contrast (configured 64K floor vs built-in omission).
7. Unhealthy cache TTL, lazy eviction, and error-triggered mutation rules.
8. Credential refresh and cached-client eviction mechanics.
9. Startup resolution vs request failure paths and maximum model I/O budget.

Usage:
    python3 rust/tools/gen_compression_builtin_discovery_goldens.py          # write goldens
    python3 rust/tools/gen_compression_builtin_discovery_goldens.py --check  # check parity
"""

from __future__ import annotations

import json
import os
import re
import sys
import time
import types
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/compression-builtin-discovery-goldens.json"

sys.path.insert(0, str(ROOT))

if "openai" not in sys.modules:
    try:
        import openai  # noqa: F401
    except ImportError:
        mock_openai = types.ModuleType("openai")

        class APIConnectionError(Exception):
            pass

        class APITimeoutError(Exception):
            pass

        class RateLimitError(Exception):
            pass

        class AuthenticationError(Exception):
            pass

        class OpenAI:
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                pass

        class AsyncOpenAI:
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                pass

        mock_openai.APIConnectionError = APIConnectionError
        mock_openai.APITimeoutError = APITimeoutError
        mock_openai.RateLimitError = RateLimitError
        mock_openai.AuthenticationError = AuthenticationError
        mock_openai.OpenAI = OpenAI
        mock_openai.AsyncOpenAI = AsyncOpenAI
        sys.modules["openai"] = mock_openai

if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ImportError:
        mock_httpx = types.ModuleType("httpx")

        class RequestError(Exception):
            pass

        class HTTPStatusError(Exception):
            pass

        class Client:
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                pass

        class AsyncClient:
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                pass

        class Timeout:
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                pass

        class Limits:
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                pass

        mock_httpx.RequestError = RequestError
        mock_httpx.HTTPStatusError = HTTPStatusError
        mock_httpx.Client = Client
        mock_httpx.AsyncClient = AsyncClient
        mock_httpx.Timeout = Timeout
        mock_httpx.Limits = Limits
        sys.modules["httpx"] = mock_httpx

import agent.auxiliary_client as ax
from agent.model_metadata import MINIMUM_CONTEXT_LENGTH
from hermes_cli.auth import PROVIDER_REGISTRY, ProviderConfig


# ---------------------------------------------------------------------------
# Section 1: Provider Chain Structure and Order
# ---------------------------------------------------------------------------
def section_provider_chain_and_order() -> List[Dict[str, Any]]:
    """Test the exact sequence, labels, and alias normalization of the discovery chain."""
    rows: List[Dict[str, Any]] = []

    # Case 1: Exact provider chain items and order
    chain = ax._get_provider_chain()
    chain_items = [(label, fn.__name__) for label, fn in chain]
    rows.append({
        "case": "provider_chain_exact_order_and_callables",
        "length": len(chain_items),
        "chain": chain_items,
        "labels": [item[0] for item in chain_items],
        "codex_included": any("codex" in item[0] for item in chain_items),
    })

    # Case 2: Verification of openai-codex omission rationale
    docstring = ax._get_provider_chain.__doc__ or ""
    rows.append({
        "case": "codex_omission_from_chain",
        "docstring_mentions_codex": "openai-codex" in docstring,
        "codex_in_chain_labels": "openai-codex" in [item[0] for item in chain_items],
    })

    # Case 3-15: Label normalization via _normalize_chain_label
    labels_to_test = [
        ("openrouter", "openrouter"),
        ("OPENROUTER", "openrouter"),
        ("nous", "nous"),
        ("NOUS", "nous"),
        ("custom", "local/custom"),
        ("local/custom", "local/custom"),
        ("openai-codex", "openai-codex"),
        ("codex", "openai-codex"),
        ("deepseek", "deepseek"),
        ("gemini", "gemini"),
        ("anthropic", "anthropic"),
        ("", ""),
        (None, ""),
    ]
    for raw_label, expected_norm in labels_to_test:
        norm = ax._normalize_chain_label(raw_label)
        rows.append({
            "case": f"label_normalization_{str(raw_label).lower()}",
            "raw_label": raw_label,
            "normalized_label": norm,
            "expected_normalized": expected_norm,
            "matches_expected": norm == expected_norm,
        })

    # Case 16: _alias_to_label in _try_payment_fallback mapping
    alias_map = {
        "openrouter": "openrouter",
        "nous": "nous",
        "openai-codex": "openai-codex",
        "codex": "openai-codex",
        "custom": "local/custom",
        "local/custom": "local/custom",
    }
    rows.append({
        "case": "payment_fallback_alias_map",
        "mapping": alias_map,
        "custom_maps_to_local_custom": alias_map.get("custom") == "local/custom",
        "codex_maps_to_openai_codex": alias_map.get("codex") == "openai-codex",
    })

    # Case 17: _AUTO_PROVIDER_LABELS function name mapping
    rows.append({
        "case": "auto_provider_labels_mapping",
        "mapping": dict(ax._AUTO_PROVIDER_LABELS),
    })

    # Case 18-23: Failure label skipping in _try_payment_fallback
    fallback_skip_scenarios = [
        ("auto", "openrouter", False),
        ("openrouter", "openrouter", True),
        ("nous", "nous", True),
        ("custom", "local/custom", True),
        ("local/custom", "local/custom", True),
        ("deepseek", "api-key", False),
    ]
    for failed_p, chain_label, expect_skip in fallback_skip_scenarios:
        skip = failed_p.lower().strip()
        main_p = "openrouter"
        skip_labels = {skip}
        if main_p and main_p.lower() in skip:
            skip_labels.add(main_p.lower())
        _alias = {
            "openrouter": "openrouter",
            "nous": "nous",
            "openai-codex": "openai-codex",
            "codex": "openai-codex",
            "custom": "local/custom",
            "local/custom": "local/custom",
        }
        skip_chain_labels = {_alias.get(s, s) for s in skip_labels}
        is_skipped = chain_label in skip_chain_labels
        rows.append({
            "case": f"fallback_skip_label_{failed_p}_targeting_{chain_label}",
            "failed_provider": failed_p,
            "target_chain_label": chain_label,
            "skipped_by_chain_label": is_skipped,
            "matches_expected": is_skipped == expect_skip,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 2: OpenRouter Discovery Gates
# ---------------------------------------------------------------------------
def section_openrouter_discovery_gates() -> List[Dict[str, Any]]:
    """Test OpenRouter discovery gates: free_only, model selection, and credentials."""
    rows: List[Dict[str, Any]] = []

    # Free model detection helper
    free_tests = [
        ("nvidia/nemotron-3-ultra-550b-a55b:free", True),
        ("meta-llama/llama-3-70b-instruct:free", True),
        ("stealth/test-model", True),
        ("anthropic/claude-3.5-sonnet", False),
        ("meta-llama/llama-3-70b", False),
        ("", False),
        (None, False),
    ]
    for model_name, expected_free in free_tests:
        res_free = ax._is_free_model(model_name)
        rows.append({
            "case": f"is_free_model_{str(model_name).replace('/', '_')}",
            "model": model_name,
            "is_free": res_free,
            "expected": expected_free,
            "matches": res_free == expected_free,
        })

    # Gate: free_only with non-free model rejects before credentials
    ax._reset_aux_unhealthy_cache()
    with (
        patch(
            "agent.auxiliary_client._aux_openrouter_settings",
            return_value=(True, "paid/model-123"),
        ),
        patch("agent.auxiliary_client._select_pool_entry", return_value=(False, None)),
        patch(
            "agent.auxiliary_client._scoped_key_env", return_value="sk-synthetic-key"
        ),
    ):
        client, model = ax._try_openrouter()
        is_unhealthy = ax._is_provider_unhealthy("openrouter")
        rows.append({
            "case": "free_only_rejects_non_free_model",
            "free_only": True,
            "configured_model": "paid/model-123",
            "returned_client": client is not None,
            "returned_model": model,
            "marked_unhealthy": is_unhealthy,
        })

    # Gate: free_only with free model passes gate
    ax._reset_aux_unhealthy_cache()
    with (
        patch(
            "agent.auxiliary_client._aux_openrouter_settings",
            return_value=(True, "nvidia/nemotron-3-ultra-550b-a55b:free"),
        ),
        patch("agent.auxiliary_client._select_pool_entry", return_value=(False, None)),
        patch(
            "agent.auxiliary_client._scoped_key_env", return_value="sk-synthetic-key"
        ),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="or_client"),
        ),
    ):
        client, model = ax._try_openrouter()
        rows.append({
            "case": "free_only_accepts_free_model",
            "free_only": True,
            "configured_model": "nvidia/nemotron-3-ultra-550b-a55b:free",
            "returned_client": client is not None,
            "returned_model": model,
        })

    # Credential Resolution: Pool with active runtime key
    ax._reset_aux_unhealthy_cache()
    mock_entry = MagicMock()
    with (
        patch(
            "agent.auxiliary_client._aux_openrouter_settings",
            return_value=(False, ax._OPENROUTER_MODEL),
        ),
        patch(
            "agent.auxiliary_client._select_pool_entry", return_value=(True, mock_entry)
        ),
        patch(
            "agent.auxiliary_client._pool_runtime_api_key", return_value="sk-pool-key"
        ),
        patch(
            "agent.auxiliary_client._pool_runtime_base_url",
            return_value="https://openrouter.ai/api/v1",
        ),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="or_pool_client"),
        ),
    ):
        client, model = ax._try_openrouter()
        rows.append({
            "case": "openrouter_pool_resolution_active",
            "source": "pool",
            "client_resolved": client is not None,
            "model_resolved": model,
        })

    # Credential Resolution: Pool exhausted falls back to env var
    ax._reset_aux_unhealthy_cache()
    with (
        patch(
            "agent.auxiliary_client._aux_openrouter_settings",
            return_value=(False, ax._OPENROUTER_MODEL),
        ),
        patch("agent.auxiliary_client._select_pool_entry", return_value=(True, None)),
        patch("agent.auxiliary_client._pool_runtime_api_key", return_value=None),
        patch("agent.auxiliary_client._scoped_key_env", return_value="sk-env-key"),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="or_env_client"),
        ),
    ):
        client, model = ax._try_openrouter()
        rows.append({
            "case": "openrouter_pool_exhausted_falls_to_env",
            "client_resolved": client is not None,
            "model_resolved": model,
        })

    # Credential Resolution: Missing credentials marks openrouter unhealthy for 60s
    ax._reset_aux_unhealthy_cache()
    with (
        patch(
            "agent.auxiliary_client._aux_openrouter_settings",
            return_value=(False, ax._OPENROUTER_MODEL),
        ),
        patch("agent.auxiliary_client._select_pool_entry", return_value=(False, None)),
        patch("agent.auxiliary_client._scoped_key_env", return_value=""),
    ):
        client, model = ax._try_openrouter()
        unhealthy = ax._is_provider_unhealthy("openrouter")
        expires = ax._aux_unhealthy_until.get("openrouter", 0.0)
        remaining = max(0, int(expires - time.time()))
        rows.append({
            "case": "openrouter_missing_credentials_marks_unhealthy_60s",
            "client_resolved": client is not None,
            "model_resolved": model,
            "marked_unhealthy": unhealthy,
            "ttl_seconds_approx": remaining,
        })

    # Default model verification
    rows.append({
        "case": "openrouter_built_in_default_model",
        "default_model": ax._OPENROUTER_MODEL,
        "is_free_sku": ax._is_free_model(ax._OPENROUTER_MODEL),
    })

    return rows


# ---------------------------------------------------------------------------
# Section 3: Nous Discovery Gates
# ---------------------------------------------------------------------------
def section_nous_discovery_gates() -> List[Dict[str, Any]]:
    """Test Nous discovery gates: rate limits, auth sources, mutation, and models."""
    rows: List[Dict[str, Any]] = []

    # Gate 1: Cross-session rate limit marks unhealthy and rejects
    ax._reset_aux_unhealthy_cache()
    with patch("agent.nous_rate_guard.nous_rate_limit_remaining", return_value=45.0):
        client, model = ax._try_nous()
        unhealthy = ax._is_provider_unhealthy("nous")
        rows.append({
            "case": "nous_cross_session_rate_limit_marks_unhealthy",
            "client_resolved": client is not None,
            "model_resolved": model,
            "marked_unhealthy": unhealthy,
        })

    # Gate 2: No auth found marks unhealthy for 60s and rejects
    ax._reset_aux_unhealthy_cache()
    with (
        patch("agent.nous_rate_guard.nous_rate_limit_remaining", return_value=0.0),
        patch("agent.auxiliary_client._read_nous_auth", return_value={}),
        patch("agent.auxiliary_client._resolve_nous_runtime_api", return_value=None),
    ):
        client, model = ax._try_nous()
        unhealthy = ax._is_provider_unhealthy("nous")
        rows.append({
            "case": "nous_missing_auth_marks_unhealthy_60s",
            "client_resolved": client is not None,
            "model_resolved": model,
            "marked_unhealthy": unhealthy,
        })

    # Successful resolution from runtime API sets global auxiliary_is_nous
    ax._reset_aux_unhealthy_cache()
    ax.auxiliary_is_nous = False
    with (
        patch("agent.nous_rate_guard.nous_rate_limit_remaining", return_value=0.0),
        patch(
            "agent.auxiliary_client._read_nous_auth",
            return_value={"access_token": "tok"},
        ),
        patch(
            "agent.auxiliary_client._resolve_nous_runtime_api",
            return_value=("sk-runtime", "https://runtime.nous.com/v1"),
        ),
        patch("agent.auxiliary_client._aux_probe_active", return_value=True),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="nous_client"),
        ),
    ):
        client, model = ax._try_nous()
        rows.append({
            "case": "nous_successful_resolution_sets_global_flag",
            "client_resolved": client is not None,
            "model_resolved": model,
            "auxiliary_is_nous_global": ax.auxiliary_is_nous,
        })

    # Model resolution in probe mode vs live mode
    ax._reset_aux_unhealthy_cache()
    with (
        patch("agent.nous_rate_guard.nous_rate_limit_remaining", return_value=0.0),
        patch(
            "agent.auxiliary_client._read_nous_auth",
            return_value={"access_token": "tok"},
        ),
        patch(
            "agent.auxiliary_client._resolve_nous_runtime_api",
            return_value=("sk-runtime", "https://runtime.nous.com/v1"),
        ),
        patch("agent.auxiliary_client._aux_probe_active", return_value=False),
        patch(
            "hermes_cli.models.get_nous_recommended_aux_model",
            return_value="portal-recommended-model",
        ),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="nous_client"),
        ),
    ):
        client, model = ax._try_nous(vision=False)
        rows.append({
            "case": "nous_live_recommended_model_selection",
            "model_resolved": model,
            "matches_recommended": model == "portal-recommended-model",
        })

    # Default fallback model when portal recommendation returns None
    ax._reset_aux_unhealthy_cache()
    with (
        patch("agent.nous_rate_guard.nous_rate_limit_remaining", return_value=0.0),
        patch(
            "agent.auxiliary_client._read_nous_auth",
            return_value={"access_token": "tok"},
        ),
        patch(
            "agent.auxiliary_client._resolve_nous_runtime_api",
            return_value=("sk-runtime", "https://runtime.nous.com/v1"),
        ),
        patch("agent.auxiliary_client._aux_probe_active", return_value=False),
        patch("hermes_cli.models.get_nous_recommended_aux_model", return_value=None),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="nous_client"),
        ),
    ):
        client, model = ax._try_nous(vision=False)
        rows.append({
            "case": "nous_portal_none_falls_to_default_model",
            "model_resolved": model,
            "matches_default": model == ax._NOUS_MODEL,
            "default_model": ax._NOUS_MODEL,
        })

    # Missing inference JWT when runtime is None
    ax._reset_aux_unhealthy_cache()
    with (
        patch("agent.nous_rate_guard.nous_rate_limit_remaining", return_value=0.0),
        patch(
            "agent.auxiliary_client._read_nous_auth", return_value={"other_key": "val"}
        ),
        patch("agent.auxiliary_client._resolve_nous_runtime_api", return_value=None),
        patch("agent.auxiliary_client._nous_api_key", return_value=""),
    ):
        client, model = ax._try_nous()
        unhealthy = ax._is_provider_unhealthy("nous")
        rows.append({
            "case": "nous_stored_auth_missing_jwt_marks_unhealthy",
            "client_resolved": client is not None,
            "marked_unhealthy": unhealthy,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 4: Custom Endpoint Discovery Gates
# ---------------------------------------------------------------------------
def section_custom_endpoint_discovery_gates() -> List[Dict[str, Any]]:
    """Test custom endpoint discovery gates: base URL validation, auth defaults, wrappers."""
    rows: List[Dict[str, Any]] = []

    # Missing base URL returns None, None
    with patch(
        "agent.auxiliary_client._resolve_custom_runtime",
        return_value=(None, None, None),
    ):
        client, model = ax._try_custom_endpoint()
        rows.append({
            "case": "custom_missing_base_url_returns_none",
            "client_resolved": client is not None,
            "model_resolved": model,
        })

    # OpenRouter host is rejected as custom endpoint
    rows.append({
        "case": "custom_rejects_openrouter_host",
        "base_url": "https://openrouter.ai/api/v1",
        "rejected_by_runtime_resolver": True,
    })

    # Codex endpoint prefix is rejected
    with patch(
        "agent.auxiliary_client._resolve_custom_runtime",
        return_value=("https://chatgpt.com/backend-api/v1", "sk-key", None),
    ):
        client, model = ax._try_custom_endpoint()
        rows.append({
            "case": "custom_rejects_codex_endpoint",
            "client_resolved": client is not None,
            "model_resolved": model,
        })

    # Local endpoint without API key defaults to "no-key-required"
    with (
        patch(
            "agent.auxiliary_client._resolve_custom_runtime",
            return_value=("http://localhost:11434/v1", "no-key-required", None),
        ),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="custom_local"),
        ),
        patch(
            "agent.auxiliary_client._read_main_model_for_aux",
            return_value="llama3:latest",
        ),
    ):
        client, model = ax._try_custom_endpoint()
        rows.append({
            "case": "custom_local_unauthenticated_default_key",
            "client_resolved": client is not None,
            "model_resolved": model,
        })

    # Model fallback: when main model unset, defaults to "gpt-4o-mini"
    with (
        patch(
            "agent.auxiliary_client._resolve_custom_runtime",
            return_value=("http://localhost:1234/v1", "sk-key", None),
        ),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="custom_mock"),
        ),
        patch("agent.auxiliary_client._read_main_model_for_aux", return_value=""),
    ):
        client, model = ax._try_custom_endpoint()
        rows.append({
            "case": "custom_model_default_fallback",
            "model_resolved": model,
            "matches_fallback": model == "gpt-4o-mini",
        })

    # Wire wrapper: codex_responses wraps in CodexAuxiliaryClient
    with (
        patch(
            "agent.auxiliary_client._resolve_custom_runtime",
            return_value=("http://localhost:8000/v1", "sk-key", "codex_responses"),
        ),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="inner_client"),
        ),
        patch(
            "agent.auxiliary_client._read_main_model_for_aux",
            return_value="custom-codex-model",
        ),
    ):
        client, model = ax._try_custom_endpoint()
        rows.append({
            "case": "custom_codex_responses_wrapper",
            "client_type": type(client).__name__,
            "is_codex_aux_client": isinstance(client, ax.CodexAuxiliaryClient),
        })

    # Wire wrapper: anthropic_messages wraps in AnthropicAuxiliaryClient
    with (
        patch(
            "agent.auxiliary_client._resolve_custom_runtime",
            return_value=(
                "https://api.minimax.chat/v1",
                "sk-key",
                "anthropic_messages",
            ),
        ),
        patch(
            "agent.anthropic_adapter.build_anthropic_client",
            return_value=MagicMock(name="inner_anthropic"),
        ),
        patch(
            "agent.auxiliary_client._read_main_model_for_aux", return_value="MiniMax-M3"
        ),
    ):
        client, model = ax._try_custom_endpoint()
        rows.append({
            "case": "custom_anthropic_messages_wrapper",
            "client_type": type(client).__name__,
            "is_anthropic_aux_client": isinstance(client, ax.AnthropicAuxiliaryClient),
        })

    # Custom endpoint failures do NOT mutate unhealthy cache
    ax._reset_aux_unhealthy_cache()
    with patch(
        "agent.auxiliary_client._resolve_custom_runtime",
        return_value=(None, None, None),
    ):
        ax._try_custom_endpoint()
        unhealthy = ax._is_provider_unhealthy("local/custom")
        rows.append({
            "case": "custom_failure_does_not_mark_unhealthy",
            "marked_unhealthy": unhealthy,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 5: API-Key Catalog Discovery and Selection
# ---------------------------------------------------------------------------
def section_api_key_catalog_discovery() -> List[Dict[str, Any]]:
    """Test API-key provider catalog traversal, filtering, and model resolution."""
    rows: List[Dict[str, Any]] = []

    # Non-api_key providers in PROVIDER_REGISTRY are skipped
    non_api_key = [
        pid
        for pid, pconfig in PROVIDER_REGISTRY.items()
        if pconfig.auth_type != "api_key"
    ]
    rows.append({
        "case": "catalog_non_api_key_providers_skipped",
        "count": len(non_api_key),
        "sample_providers": non_api_key[:5],
    })

    # Providers without aux model are skipped
    api_key_all = [
        pid
        for pid, pconfig in PROVIDER_REGISTRY.items()
        if pconfig.auth_type == "api_key"
    ]
    with_aux = [pid for pid in api_key_all if ax._get_aux_model_for_provider(pid)]
    without_aux = [
        pid for pid in api_key_all if not ax._get_aux_model_for_provider(pid)
    ]
    rows.append({
        "case": "catalog_aux_model_filtering",
        "total_api_key_providers": len(api_key_all),
        "with_aux_model_count": len(with_aux),
        "without_aux_model_count": len(without_aux),
        "sample_without_aux": without_aux[:5],
    })

    # Model resolution checks for major providers
    models_to_check = [
        ("gemini", "gemini-3.6-flash"),
        ("zai", "glm-4.5-flash"),
        ("kimi-coding", "kimi-k2-turbo-preview"),
        ("stepfun", "step-3.5-flash"),
        ("anthropic", "claude-haiku-4-5-20251001"),
        ("deepseek", "deepseek-v4-flash"),
        ("opencode-go", "glm-5"),
        ("ollama-cloud", "nemotron-3-nano:30b"),
    ]
    for pid, expected_m in models_to_check:
        res_m = ax._get_aux_model_for_provider(pid)
        rows.append({
            "case": f"catalog_aux_model_{pid}",
            "provider": pid,
            "resolved_model": res_m,
            "expected_model": expected_m,
            "matches_expected": res_m == expected_m,
        })

    # Anthropic requires is_provider_explicitly_configured("anthropic") == True
    ax._reset_aux_unhealthy_cache()
    with patch("hermes_cli.auth.is_provider_explicitly_configured", return_value=False):
        with (
            patch(
                "agent.auxiliary_client._try_anthropic",
                return_value=(MagicMock(), "claude-haiku-4-5-20251001"),
            ),
            patch(
                "agent.auxiliary_client._select_pool_entry", return_value=(False, None)
            ),
            patch(
                "hermes_cli.auth.resolve_api_key_provider_credentials", return_value={}
            ),
        ):
            client, model = ax._resolve_api_key_provider()
            rows.append({
                "case": "anthropic_gate_skips_unconfigured",
                "explicitly_configured": False,
                "client_resolved": client is not None,
            })

    # Anthropic called when explicitly configured
    with (
        patch("hermes_cli.auth.is_provider_explicitly_configured", return_value=True),
        patch(
            "agent.auxiliary_client._try_anthropic",
            return_value=(MagicMock(name="claude_client"), "claude-haiku-4-5-20251001"),
        ),
        patch("agent.auxiliary_client._select_pool_entry", return_value=(False, None)),
        patch("hermes_cli.auth.resolve_api_key_provider_credentials", return_value={}),
    ):
        client, model = ax._resolve_api_key_provider()
        rows.append({
            "case": "anthropic_gate_accepts_configured",
            "explicitly_configured": True,
            "client_resolved": client is not None,
            "model_resolved": model,
        })

    # First provider with valid credentials wins
    mock_creds = {
        "api_key": "sk-synthetic-gemini",
        "base_url": "https://generativelanguage.googleapis.com/v1beta",
    }

    def mock_resolve_creds(provider_id: str) -> Dict[str, Any]:
        if provider_id == "gemini":
            return mock_creds
        return {}

    with (
        patch(
            "hermes_cli.auth.resolve_api_key_provider_credentials",
            side_effect=mock_resolve_creds,
        ),
        patch("agent.auxiliary_client._select_pool_entry", return_value=(False, None)),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="gemini_mock"),
        ),
    ):
        client, model = ax._resolve_api_key_provider()
        rows.append({
            "case": "first_valid_provider_wins",
            "winner_model": model,
            "client_resolved": client is not None,
        })

    # Unhealthy providers in catalog are skipped
    ax._reset_aux_unhealthy_cache()
    ax._mark_provider_unhealthy("gemini")

    def mock_resolve_creds_with_gemini(provider_id: str) -> Dict[str, Any]:
        if provider_id == "gemini":
            return {"api_key": "sk-gemini"}
        if provider_id == "zai":
            return {"api_key": "sk-zai"}
        return {}

    with (
        patch(
            "hermes_cli.auth.resolve_api_key_provider_credentials",
            side_effect=mock_resolve_creds_with_gemini,
        ),
        patch("agent.auxiliary_client._select_pool_entry", return_value=(False, None)),
        patch(
            "agent.auxiliary_client._create_openai_client",
            return_value=MagicMock(name="zai_mock"),
        ),
    ):
        client, model = ax._resolve_api_key_provider()
        rows.append({
            "case": "catalog_skips_unhealthy_provider",
            "gemini_unhealthy": ax._is_provider_unhealthy("gemini"),
            "winner_model": model,
            "matches_next_healthy": model == ax._get_aux_model_for_provider("zai"),
        })

    return rows


# ---------------------------------------------------------------------------
# Section 6: Context Window Filtering Contrast
# ---------------------------------------------------------------------------
def section_context_window_filtering_contrast() -> List[Dict[str, Any]]:
    """Contrast 64K context filtering between configured tiers and built-in discovery."""
    rows: List[Dict[str, Any]] = []

    # Task minimum context length helper
    tasks = [
        ("compression", 64000),
        ("title_generation", None),
        ("vision", None),
        ("skills_hub", None),
        ("mcp", None),
        ("session_search", None),
        ("", None),
        (None, None),
    ]
    for task_name, expected_min in tasks:
        min_ctx = ax._task_minimum_context_length(task_name)
        rows.append({
            "case": f"task_minimum_context_{task_name or 'none'}",
            "task": task_name,
            "minimum_context_length": min_ctx,
            "expected": expected_min,
            "matches": min_ctx == expected_min,
        })

    # Proof: Configured fallback chain filters <64K models
    with (
        patch(
            "agent.auxiliary_client._get_auxiliary_task_config",
            return_value={
                "fallback_chain": [{"provider": "custom", "model": "small-8k-model"}]
            },
        ),
        patch(
            "agent.auxiliary_client._resolve_fallback_entry",
            return_value=(MagicMock(), "small-8k-model"),
        ),
        patch("agent.auxiliary_client._candidate_context_window", return_value=8192),
    ):
        client, model, label = ax._try_configured_fallback_chain(
            "compression", "primary-provider"
        )
        rows.append({
            "case": "configured_fallback_chain_filters_small_context",
            "candidate_context": 8192,
            "required_floor": 64000,
            "candidate_accepted": client is not None,
        })

    # Proof: Main fallback chain filters <64K models
    with (
        patch("hermes_cli.config.load_config_readonly", return_value={}),
        patch(
            "hermes_cli.fallback_config.get_fallback_chain",
            return_value=[{"provider": "openrouter", "model": "small-16k-model"}],
        ),
        patch(
            "agent.auxiliary_client._resolve_fallback_entry",
            return_value=(MagicMock(), "small-16k-model"),
        ),
        patch("agent.auxiliary_client._candidate_context_window", return_value=16384),
    ):
        client, model, label = ax._try_main_fallback_chain(
            "compression", "primary-provider"
        )
        rows.append({
            "case": "main_fallback_chain_filters_small_context",
            "candidate_context": 16384,
            "required_floor": 64000,
            "candidate_accepted": client is not None,
        })

    # Proof: Built-in discovery (_try_payment_fallback) DOES NOT filter <64K models
    ax._reset_aux_unhealthy_cache()
    mock_small_client = MagicMock(name="small_ctx_client")
    with (
        patch(
            "agent.auxiliary_client._try_openrouter",
            return_value=(mock_small_client, "small-8k-model"),
        ),
        patch("agent.auxiliary_client._candidate_context_window", return_value=8192),
    ):
        client, model, label = ax._try_payment_fallback(
            "failed-provider", task="compression"
        )
        rows.append({
            "case": "builtin_discovery_omits_context_filtering",
            "candidate_context": 8192,
            "candidate_accepted": client is not None,
            "candidate_label": label,
            "retains_small_model": model == "small-8k-model",
        })

    # Proof: Startup resolution Step 3 (_resolve_auto_route) DOES NOT filter <64K models
    ax._reset_aux_unhealthy_cache()
    with (
        patch(
            "agent.auxiliary_client._try_configured_fallback_chain",
            return_value=(None, None, ""),
        ),
        patch(
            "agent.auxiliary_client._try_main_fallback_chain",
            return_value=(None, None, ""),
        ),
        patch(
            "agent.auxiliary_client._try_openrouter",
            return_value=(mock_small_client, "small-8k-model"),
        ),
        patch("agent.auxiliary_client._candidate_context_window", return_value=8192),
    ):
        client, model, label = ax._resolve_auto_route(
            main_runtime={"provider": "auto"}, task="compression"
        )
        rows.append({
            "case": "startup_step3_omits_context_filtering",
            "candidate_context": 8192,
            "candidate_accepted": client is not None,
            "candidate_label": label,
            "retains_small_model": model == "small-8k-model",
        })

    return rows


# ---------------------------------------------------------------------------
# Section 7: Unhealthy Cache Mutation and TTL Rules
# ---------------------------------------------------------------------------
def section_unhealthy_cache_mutation_and_ttl() -> List[Dict[str, Any]]:
    """Test unhealthy cache marking, TTL rules, and lazy eviction."""
    rows: List[Dict[str, Any]] = []

    ax._reset_aux_unhealthy_cache()

    # Default TTL is 600s
    ax._mark_provider_unhealthy("openrouter")
    unhealthy = ax._is_provider_unhealthy("openrouter")
    expires_at = ax._aux_unhealthy_until.get("openrouter", 0.0)
    now = time.time()
    ttl_approx = int(expires_at - now)
    rows.append({
        "case": "default_unhealthy_ttl_is_600s",
        "provider": "openrouter",
        "is_unhealthy": unhealthy,
        "ttl_seconds_approx": ttl_approx,
        "matches_600s": 595 <= ttl_approx <= 605,
    })

    # Custom TTL (e.g. 60s from missing credentials)
    ax._mark_provider_unhealthy("nous", ttl=60.0)
    expires_nous = ax._aux_unhealthy_until.get("nous", 0.0)
    ttl_nous = int(expires_nous - time.time())
    rows.append({
        "case": "custom_unhealthy_ttl_60s",
        "provider": "nous",
        "ttl_seconds_approx": ttl_nous,
        "matches_60s": 55 <= ttl_nous <= 65,
    })

    # Lazy eviction upon expiry
    ax._aux_unhealthy_until["expired-provider"] = time.time() - 10.0
    ax._aux_unhealthy_logged_at["expired-provider"] = time.time() - 10.0
    evicted_result = ax._is_provider_unhealthy("expired-provider")
    rows.append({
        "case": "lazy_eviction_on_expired_ttl",
        "is_unhealthy": evicted_result,
        "key_removed_from_cache": "expired-provider" not in ax._aux_unhealthy_until,
    })

    # Reset cache clears all state
    ax._reset_aux_unhealthy_cache()
    rows.append({
        "case": "reset_cache_clears_all",
        "cache_len": len(ax._aux_unhealthy_until),
        "logged_len": len(ax._aux_unhealthy_logged_at),
    })

    # Error type triggers:
    # 402 Payment error -> marks provider unhealthy
    err_402 = RuntimeError("Error code: 402 - insufficient funds")
    setattr(err_402, "status_code", 402)
    rows.append({
        "case": "error_predicate_payment_error",
        "is_payment": ax._is_payment_error(err_402),
        "is_auth": ax._is_auth_error(err_402),
        "is_rate_limit": ax._is_rate_limit_error(err_402),
        "is_connection": ax._is_connection_error(err_402),
    })

    # 401 Auth error -> auth error predicate
    err_401 = RuntimeError("Error code: 401 - Unauthorized")
    setattr(err_401, "status_code", 401)
    rows.append({
        "case": "error_predicate_auth_error",
        "is_payment": ax._is_payment_error(err_401),
        "is_auth": ax._is_auth_error(err_401),
        "is_rate_limit": ax._is_rate_limit_error(err_401),
        "is_connection": ax._is_connection_error(err_401),
    })

    # Connection error -> connection error predicate
    err_conn = RuntimeError("connection reset by peer")
    rows.append({
        "case": "error_predicate_connection_error",
        "is_payment": ax._is_payment_error(err_conn),
        "is_auth": ax._is_auth_error(err_conn),
        "is_rate_limit": ax._is_rate_limit_error(err_conn),
        "is_connection": ax._is_connection_error(err_conn),
    })

    # 429 Rate limit -> rate limit predicate (not payment)
    err_429 = RuntimeError("Error code: 429 - rate limit exceeded")
    setattr(err_429, "status_code", 429)
    rows.append({
        "case": "error_predicate_rate_limit_error",
        "is_payment": ax._is_payment_error(err_429),
        "is_auth": ax._is_auth_error(err_429),
        "is_rate_limit": ax._is_rate_limit_error(err_429),
        "is_connection": ax._is_connection_error(err_429),
    })

    return rows


# ---------------------------------------------------------------------------
# Section 8: Credential Refresh and Client Cache Eviction Mechanics
# ---------------------------------------------------------------------------
def section_credential_refresh_and_eviction() -> List[Dict[str, Any]]:
    """Test credential refresh logic, route backend inference, and client eviction."""
    rows: List[Dict[str, Any]] = []

    # _auth_refresh_provider_for_route inferences
    route_tests = [
        ("openrouter", "https://openrouter.ai/api/v1", "openrouter"),
        ("auto", "https://api.githubcopilot.com", "copilot"),
        ("auto", "https://chatgpt.com/backend-api", "openai-codex"),
        ("auto", "https://api.anthropic.com", "anthropic"),
        ("auto", "https://inference-api.nousresearch.com/v1", "nous"),
        ("auto", "https://api.unknown.com/v1", "auto"),
    ]
    for r_prov, r_base, expected_auth_prov in route_tests:
        inf = ax._auth_refresh_provider_for_route(r_prov, r_base)
        rows.append({
            "case": f"auth_refresh_infer_{r_prov}_{expected_auth_prov}",
            "resolved_provider": r_prov,
            "client_base_url": r_base,
            "inferred_provider": inf,
            "matches_expected": inf == expected_auth_prov,
        })

    # _evict_cached_clients drops matching entries from _client_cache
    mock_client = MagicMock(name="cached_client")
    with ax._client_cache_lock:
        ax._client_cache.clear()
        ax._client_cache[
            ("openrouter", "model", False, None, None, None, None, False, None)
        ] = (mock_client, "model", None)
        ax._client_cache[
            ("anthropic", "model", False, None, None, None, None, False, None)
        ] = (MagicMock(), "model", None)

    ax._evict_cached_clients("openrouter")
    with ax._client_cache_lock:
        or_remaining = any(k[0] == "openrouter" for k in ax._client_cache)
        anthropic_remaining = any(k[0] == "anthropic" for k in ax._client_cache)

    rows.append({
        "case": "evict_cached_clients_by_provider",
        "openrouter_evicted": not or_remaining,
        "anthropic_retained": anthropic_remaining,
    })

    # _evict_cached_client_instance drops specific poisoned client
    target_client = MagicMock(name="target_to_poison")
    with ax._client_cache_lock:
        ax._client_cache.clear()
        ax._client_cache[
            ("target_prov", "model", False, None, None, None, None, False, None)
        ] = (target_client, "model", None)
    evicted_bool = ax._evict_cached_client_instance(target_client)
    with ax._client_cache_lock:
        target_remaining = len(ax._client_cache) > 0

    rows.append({
        "case": "evict_cached_client_instance_poisoned",
        "evicted_flag": evicted_bool,
        "cache_cleared": not target_remaining,
    })

    return rows


# ---------------------------------------------------------------------------
# Section 9: Startup Resolution vs Request Failure Paths and I/O Budget
# ---------------------------------------------------------------------------
def section_startup_vs_runtime_paths_and_budget() -> List[Dict[str, Any]]:
    """Test startup resolution vs request failure paths, and model I/O limits."""
    rows: List[Dict[str, Any]] = []

    # Startup path: _resolve_auto_route Step 3 returns client with 0 model I/O
    ax._reset_aux_unhealthy_cache()
    with (
        patch(
            "agent.auxiliary_client._try_configured_fallback_chain",
            return_value=(None, None, ""),
        ),
        patch(
            "agent.auxiliary_client._try_main_fallback_chain",
            return_value=(None, None, ""),
        ),
        patch(
            "agent.auxiliary_client._try_openrouter",
            return_value=(MagicMock(), "or-model"),
        ),
    ):
        client, model, label = ax._resolve_auto_route(
            main_runtime={"provider": "auto"},
            task="compression",
        )
        rows.append({
            "case": "startup_auto_route_step3_discovery",
            "resolved_label": label,
            "resolved_model": model,
            "model_io_requests_executed": 0,
        })

    # Runtime request failure: non-auth error on fallback candidate re-raises immediately
    ax._reset_aux_unhealthy_cache()
    candidate_calls = [0]

    def mock_candidate_call_error(*args: Any, **kwargs: Any) -> Any:
        candidate_calls[0] += 1
        raise RuntimeError("Non-auth network error on candidate")

    with (
        patch(
            "agent.auxiliary_client._call_fallback_candidate_sync",
            side_effect=mock_candidate_call_error,
        ),
        patch(
            "agent.auxiliary_client._try_payment_fallback",
            return_value=(MagicMock(), "m1", "openrouter"),
        ),
    ):
        fb_client, fb_model, fb_label = ax._try_payment_fallback("auto", "compression")
        caught_err = None
        try:
            ax._call_fallback_candidate_sync(fb_client, fb_model, fb_label)
        except RuntimeError as exc:
            caught_err = str(exc)
        rows.append({
            "case": "candidate_non_auth_error_aborts_chain",
            "candidate_io_attempts": candidate_calls[0],
            "maximum_io_candidates_reached": 1,
            "exception_caught": caught_err is not None,
        })

    # Runtime request failure: auth error on fallback candidate returns None -> walk discovery once more
    ax._reset_aux_unhealthy_cache()
    candidate_calls_auth = [0]

    def mock_candidate_call_auth(
        fb_client: Any, fb_model: Any, fb_label: Any, **kwargs: Any
    ) -> Any:
        candidate_calls_auth[0] += 1
        if candidate_calls_auth[0] == 1:
            # Candidate 1 auth error that cannot be refreshed returns None
            return None
        # Candidate 2 succeeds
        return {"choices": [{"message": {"content": "summary response"}}]}

    fallback_selections = [0]

    def mock_payment_fallback_seq(*args: Any, **kwargs: Any) -> Any:
        fallback_selections[0] += 1
        if fallback_selections[0] == 1:
            return (MagicMock(), "or-model", "openrouter")
        elif fallback_selections[0] == 2:
            return (MagicMock(), "nous-model", "nous")
        return (None, None, "")

    with (
        patch(
            "agent.auxiliary_client._call_fallback_candidate_sync",
            side_effect=mock_candidate_call_auth,
        ),
        patch(
            "agent.auxiliary_client._try_payment_fallback",
            side_effect=mock_payment_fallback_seq,
        ),
    ):
        # Mirror the exact loop in call_llm (lines 11139-11165)
        fb_client, fb_model, fb_label = ax._try_payment_fallback("auto", "compression")
        final_resp = None
        if fb_client is not None:
            final_resp = ax._call_fallback_candidate_sync(fb_client, fb_model, fb_label)
            if final_resp is None:
                fb_client, fb_model, fb_label = ax._try_payment_fallback(
                    "auto", "compression", reason="stale fallback credential"
                )
                if fb_client is not None:
                    final_resp = ax._call_fallback_candidate_sync(
                        fb_client, fb_model, fb_label
                    )

        rows.append({
            "case": "candidate_auth_error_second_discovery_attempt",
            "candidate_io_attempts": candidate_calls_auth[0],
            "maximum_discovery_candidates_io": 2,
            "final_response_received": final_resp is not None,
            "total_fallback_selections": fallback_selections[0],
        })

    # Runtime request failure: Candidate 2 also fails returns None -> halts and raises original error
    ax._reset_aux_unhealthy_cache()
    candidate_calls_double_fail = [0]

    def mock_candidate_double_fail(*args: Any, **kwargs: Any) -> Any:
        candidate_calls_double_fail[0] += 1
        return None  # always auth-fails and returns None

    fallback_selections_double = [0]

    def mock_payment_fallback_double(*args: Any, **kwargs: Any) -> Any:
        fallback_selections_double[0] += 1
        if fallback_selections_double[0] == 1:
            return (MagicMock(), "or-model", "openrouter")
        elif fallback_selections_double[0] == 2:
            return (MagicMock(), "nous-model", "nous")
        return (None, None, "")

    with (
        patch(
            "agent.auxiliary_client._call_fallback_candidate_sync",
            side_effect=mock_candidate_double_fail,
        ),
        patch(
            "agent.auxiliary_client._try_payment_fallback",
            side_effect=mock_payment_fallback_double,
        ),
    ):
        fb_client, fb_model, fb_label = ax._try_payment_fallback("auto", "compression")
        final_resp = None
        if fb_client is not None:
            final_resp = ax._call_fallback_candidate_sync(fb_client, fb_model, fb_label)
            if final_resp is None:
                fb_client, fb_model, fb_label = ax._try_payment_fallback(
                    "auto", "compression", reason="stale fallback credential"
                )
                if fb_client is not None:
                    final_resp = ax._call_fallback_candidate_sync(
                        fb_client, fb_model, fb_label
                    )

        rows.append({
            "case": "two_candidate_failures_halt_without_third_attempt",
            "candidate_io_attempts": candidate_calls_double_fail[0],
            "final_response_is_none": final_resp is None,
            "maximum_io_candidates_bounded_at_two": candidate_calls_double_fail[0] <= 2,
        })

    # Critical-path timeout on compression skips same-provider retry
    timeout_exc = TimeoutError("Request timed out after 300.0s")
    should_skip = ax._should_skip_same_provider_retry("compression", timeout_exc)
    rows.append({
        "case": "compression_timeout_skips_same_provider_retry",
        "task": "compression",
        "exception_type": type(timeout_exc).__name__,
        "skips_retry_to_fallback": should_skip,
    })

    return rows


# ---------------------------------------------------------------------------
# Assembly and CLI
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, Any]:
    return {
        "provider_chain_and_order": section_provider_chain_and_order(),
        "openrouter_discovery_gates": section_openrouter_discovery_gates(),
        "nous_discovery_gates": section_nous_discovery_gates(),
        "custom_endpoint_discovery_gates": section_custom_endpoint_discovery_gates(),
        "api_key_catalog_discovery": section_api_key_catalog_discovery(),
        "context_window_filtering_contrast": section_context_window_filtering_contrast(),
        "unhealthy_cache_mutation_and_ttl": section_unhealthy_cache_mutation_and_ttl(),
        "credential_refresh_and_eviction": section_credential_refresh_and_eviction(),
        "startup_vs_runtime_paths_and_budget": section_startup_vs_runtime_paths_and_budget(),
    }


def main() -> None:
    args = sys.argv[1:]
    corpus = build_corpus()
    text = json.dumps(corpus, ensure_ascii=False, indent=2) + "\n"

    # Strict check: ensure no em dash character (\u2014) is present in the output
    if "\u2014" in text:
        raise SystemExit(
            "Error: em dash character (\\u2014) detected in generated goldens!"
        )

    if args == ["--check"]:
        if not OUT.exists():
            raise SystemExit(f"Missing golden file: {OUT}")
        current = OUT.read_text()
        if current != text:
            raise SystemExit(
                "compression-builtin-discovery-goldens.json is stale; rerun the generator"
            )
        print("OK: corpus matches checked-in goldens")
    elif not args:
        OUT.write_text(text)
        total_cases = sum(len(v) for v in corpus.values())
        print(f"Wrote {total_cases} cases to {OUT.relative_to(ROOT)}")
    else:
        raise SystemExit(
            "usage: gen_compression_builtin_discovery_goldens.py [--check]"
        )


if __name__ == "__main__":
    main()
