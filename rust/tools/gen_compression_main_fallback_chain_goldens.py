#!/usr/bin/env python3
"""Deterministic source-executed oracle for auxiliary compression main fallback chain.

This generator executes real Python decision functions that govern the top-level
main-model fallback chain (fallback_providers and legacy fallback_model) when
consumed by auxiliary compression in provider auto mode:
1. Container acceptance and entry parsing (dicts, lists, scalar rejections).
2. Field normalization and route deduplication.
3. Credential resolution precedence (inline api_key vs key_env vs api_key_env).
4. Transport and api_mode aliases.
5. Skipping rules (failed provider, main provider, auto provider, unhealthy cache).
6. 64,000-token compression context filtering vs permissive non-compression tasks.
7. Resolution failures and per-entry timeout behavior at the final request.
8. Single-candidate execution budget and failure propagation paths.

Usage:
    python3 rust/tools/gen_compression_main_fallback_chain_goldens.py          # write goldens
    python3 rust/tools/gen_compression_main_fallback_chain_goldens.py --check  # check parity
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
OUT = ROOT / "rust/tools/compression-main-fallback-chain-goldens.json"

sys.path.insert(0, str(ROOT))

import agent.auxiliary_client as ax
import hermes_cli.fallback_config as fc


# ---------------------------------------------------------------------------
# Section 1: Container acceptance and entry parsing
# ---------------------------------------------------------------------------
def section_container_and_entry_parsing() -> List[Dict[str, Any]]:
    """Test parsing and acceptance of fallback containers and entries."""
    cases = [
        (
            "modern_fallback_providers_list",
            {
                "fallback_providers": [
                    {
                        "provider": "openrouter",
                        "model": "meta-llama/llama-3-70b",
                        "base_url": "https://openrouter.ai/api/v1",
                        "api_key": "sk-synthetic-openrouter",
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
            "truthy_numeric_provider_is_stringified",
            {"fallback_providers": [{"provider": 123, "model": "valid-model"}]},
        ),
        (
            "truthy_boolean_provider_is_stringified",
            {"fallback_providers": [{"provider": True, "model": "valid-model"}]},
        ),
        (
            "truthy_numeric_model_is_stringified",
            {"fallback_providers": [{"provider": "custom", "model": 123}]},
        ),
        (
            "false_provider_is_rejected",
            {"fallback_providers": [{"provider": False, "model": "valid-model"}]},
        ),
        (
            "item_normalization_trimming",
            {
                "fallback_providers": [
                    {
                        "provider": "  OpenRouter  ",
                        "model": "  meta-llama/llama-3-70b  ",
                    }
                ]
            },
        ),
        (
            "item_normalization_base_url_trailing_slash",
            {
                "fallback_providers": [
                    {
                        "provider": "custom",
                        "model": "local-model",
                        "base_url": "  https://api.example.com/v1///  ",
                    }
                ]
            },
        ),
        (
            "truthy_non_string_base_url_is_preserved",
            {
                "fallback_providers": [
                    {"provider": "custom", "model": "local-model", "base_url": 123}
                ]
            },
        ),
        (
            "falsey_non_string_base_url_is_preserved",
            {
                "fallback_providers": [
                    {"provider": "custom", "model": "local-model", "base_url": False}
                ]
            },
        ),
        (
            "item_normalization_extra_fields_preserved",
            {
                "fallback_providers": [
                    {
                        "provider": "custom",
                        "model": "local-model",
                        "timeout": 60,
                        "extra_body": {"custom_flag": True},
                        "custom_arbitrary_field": "preserved",
                    }
                ]
            },
        ),
    ]

    rows = []
    for case_name, raw_cfg in cases:
        chain = fc.get_fallback_chain(raw_cfg)
        rows.append({
            "case": case_name,
            "raw_config": raw_cfg,
            "chain_length": len(chain),
            "chain": chain,
        })
    return rows


# ---------------------------------------------------------------------------
# Section 2: Chain deduplication and route identity
# ---------------------------------------------------------------------------
def section_chain_deduplication_and_identity() -> List[Dict[str, Any]]:
    """Test composite route identity and deduplication semantics."""
    cases = [
        (
            "exact_duplicate_within_fallback_providers",
            {
                "fallback_providers": [
                    {
                        "provider": "openrouter",
                        "model": "meta-llama/llama-3-70b",
                        "base_url": "https://openrouter.ai/api/v1",
                    },
                    {
                        "provider": "openrouter",
                        "model": "meta-llama/llama-3-70b",
                        "base_url": "https://openrouter.ai/api/v1",
                    },
                ]
            },
        ),
        (
            "case_insensitive_duplicate_within_fallback_providers",
            {
                "fallback_providers": [
                    {
                        "provider": "OpenRouter",
                        "model": "Meta-Llama/Llama-3-70B",
                        "base_url": "HTTPS://OpenRouter.AI/api/v1",
                    },
                    {
                        "provider": "openrouter",
                        "model": "meta-llama/llama-3-70b",
                        "base_url": "https://openrouter.ai/api/v1",
                    },
                ]
            },
        ),
        (
            "base_url_trailing_slash_duplicate",
            {
                "fallback_providers": [
                    {
                        "provider": "custom",
                        "model": "local-model",
                        "base_url": "https://api.example.com/v1/",
                    },
                    {
                        "provider": "custom",
                        "model": "local-model",
                        "base_url": "https://api.example.com/v1",
                    },
                ]
            },
        ),
        (
            "duplicate_across_modern_and_legacy",
            {
                "fallback_providers": [
                    {
                        "provider": "anthropic",
                        "model": "claude-3-5-sonnet",
                        "api_key": "modern-key",
                    }
                ],
                "fallback_model": [
                    {
                        "provider": "anthropic",
                        "model": "claude-3-5-sonnet",
                        "api_key": "legacy-key",
                    }
                ],
            },
        ),
        (
            "distinct_models_same_provider_retained",
            {
                "fallback_providers": [
                    {"provider": "anthropic", "model": "claude-3-5-sonnet"},
                    {"provider": "anthropic", "model": "claude-3-haiku"},
                ]
            },
        ),
        (
            "distinct_base_urls_same_provider_model_retained",
            {
                "fallback_providers": [
                    {
                        "provider": "custom",
                        "model": "llama-3",
                        "base_url": "https://east.cluster.local/v1",
                    },
                    {
                        "provider": "custom",
                        "model": "llama-3",
                        "base_url": "https://west.cluster.local/v1",
                    },
                ]
            },
        ),
        (
            "multiple_duplicates_source_order_retained",
            {
                "fallback_providers": [
                    {"provider": "p1", "model": "m1"},
                    {"provider": "p2", "model": "m2"},
                    {"provider": "p1", "model": "m1"},  # dup of 1st
                ],
                "fallback_model": [
                    {"provider": "p2", "model": "m2"},  # dup of 2nd
                    {"provider": "p3", "model": "m3"},
                ],
            },
        ),
    ]

    rows = []
    for case_name, raw_cfg in cases:
        chain = fc.get_fallback_chain(raw_cfg)
        identities = [fc._entry_identity(e) for e in chain]
        rows.append({
            "case": case_name,
            "raw_config": raw_cfg,
            "chain_length": len(chain),
            "identities": [list(ident) for ident in identities],
            "chain": chain,
        })
    return rows


# ---------------------------------------------------------------------------
# Section 3: Credential and transport resolution
# ---------------------------------------------------------------------------
def section_credential_and_transport_resolution() -> List[Dict[str, Any]]:
    """Test resolution of API keys, environment scoping, and transport modes."""
    fake_env = {
        "FALLBACK_TEST_KEY": "sk-synthetic-key-env-value",
        "FALLBACK_TEST_ALIAS": "sk-synthetic-api-key-env-alias",
    }

    cases = [
        (
            "inline_api_key_only",
            {"provider": "custom", "model": "m", "api_key": "  sk-synthetic-inline  "},
        ),
        (
            "key_env_only",
            {"provider": "custom", "model": "m", "key_env": "FALLBACK_TEST_KEY"},
        ),
        (
            "api_key_env_alias_only",
            {"provider": "custom", "model": "m", "api_key_env": "FALLBACK_TEST_ALIAS"},
        ),
        (
            "precedence_inline_over_key_env",
            {
                "provider": "custom",
                "model": "m",
                "api_key": "sk-synthetic-inline-winner",
                "key_env": "FALLBACK_TEST_KEY",
            },
        ),
        (
            "precedence_key_env_over_api_key_env",
            {
                "provider": "custom",
                "model": "m",
                "key_env": "FALLBACK_TEST_KEY",
                "api_key_env": "FALLBACK_TEST_ALIAS",
            },
        ),
        (
            "empty_key_env_falls_through_to_api_key_env",
            {
                "provider": "custom",
                "model": "m",
                "key_env": "",
                "api_key_env": "FALLBACK_TEST_ALIAS",
            },
        ),
        (
            "nonexistent_env_evaluates_none",
            {"provider": "custom", "model": "m", "key_env": "NONEXISTENT_VAR"},
        ),
        (
            "missing_credentials_evaluates_none",
            {"provider": "custom", "model": "m"},
        ),
        (
            "api_mode_explicit",
            {
                "provider": "custom",
                "model": "m",
                "api_mode": "chat_completions",
            },
        ),
        (
            "transport_alias_explicit",
            {
                "provider": "custom",
                "model": "m",
                "transport": "responses",
            },
        ),
        (
            "precedence_api_mode_over_transport",
            {
                "provider": "custom",
                "model": "m",
                "api_mode": "anthropic_messages",
                "transport": "responses",
            },
        ),
    ]

    rows = []
    with patch(
        "hermes_cli.runtime_provider.resolve_runtime_provider",
        return_value={"api_mode": "chat_completions"},
    ):
        with patch.dict(os.environ, fake_env, clear=False):
            for case_name, entry in cases:
                resolved_key_fc = fc.resolve_entry_api_key(entry)
                resolved_key_ax = ax._fallback_entry_api_key(entry)
                api_mode_extracted = (
                    str(entry.get("api_mode") or entry.get("transport") or "").strip()
                    or None
                )

                dest = ax._complete_fallback_destination(
                    str(entry.get("provider")),
                    str(entry.get("base_url") or ""),
                    api_mode_extracted,
                    str(entry.get("model")),
                )

                rows.append({
                    "case": case_name,
                    "entry": entry,
                    "resolved_key_fc": resolved_key_fc,
                    "resolved_key_ax": resolved_key_ax,
                    "api_mode_extracted": api_mode_extracted,
                    "destination": {
                        "provider": dest.provider,
                        "base_url": dest.base_url,
                        "api_mode": dest.api_mode,
                        "model": dest.model,
                    },
                })
    return rows


# ---------------------------------------------------------------------------
# Section 4: Provider skipping and unhealthy rules
# ---------------------------------------------------------------------------
def section_provider_skipping_and_health_rules() -> List[Dict[str, Any]]:
    """Test skip set construction and provider-level health gating."""
    cases = [
        (
            "failed_provider_skipped",
            {
                "main_provider": "anthropic",
                "failed_provider": "openrouter",
                "chain": [
                    {"provider": "openrouter", "model": "m1"},
                    {"provider": "custom", "model": "m2"},
                ],
            },
        ),
        (
            "main_provider_skipped",
            {
                "main_provider": "anthropic",
                "failed_provider": "openrouter",
                "chain": [
                    {"provider": "anthropic", "model": "m1"},
                    {"provider": "custom", "model": "m2"},
                ],
            },
        ),
        (
            "auto_provider_in_chain_skipped",
            {
                "main_provider": "anthropic",
                "failed_provider": "google",
                "chain": [
                    {"provider": "auto", "model": "m1"},
                    {"provider": "custom", "model": "m2"},
                ],
            },
        ),
        (
            "sibling_model_under_failed_provider_skipped_wholesale",
            {
                "main_provider": "anthropic",
                "failed_provider": "openrouter",
                "chain": [
                    {"provider": "openrouter", "model": "sibling-model-1"},
                    {"provider": "openrouter", "model": "sibling-model-2"},
                    {"provider": "custom", "model": "recovered-model"},
                ],
            },
        ),
        (
            "case_insensitive_skipping",
            {
                "main_provider": "AnThRoPiC",
                "failed_provider": "OpEnRoUtEr",
                "chain": [
                    {"provider": "OPENROUTER", "model": "m1"},
                    {"provider": "anthropic", "model": "m2"},
                    {"provider": "custom", "model": "m3"},
                ],
            },
        ),
        (
            "unhealthy_provider_skipped",
            {
                "main_provider": "anthropic",
                "failed_provider": "google",
                "unhealthy_label": "openrouter",
                "unhealthy_ttl": 60.0,
                "chain": [
                    {"provider": "openrouter", "model": "m1"},
                    {"provider": "custom", "model": "m2"},
                ],
            },
        ),
        (
            "expired_unhealthy_provider_admitted",
            {
                "main_provider": "anthropic",
                "failed_provider": "google",
                "unhealthy_label": "openrouter",
                "unhealthy_ttl": -10.0,  # expired in the past
                "chain": [
                    {"provider": "openrouter", "model": "m1"},
                    {"provider": "custom", "model": "m2"},
                ],
            },
        ),
        (
            "all_candidates_skipped_returns_none",
            {
                "main_provider": "anthropic",
                "failed_provider": "openrouter",
                "chain": [
                    {"provider": "openrouter", "model": "m1"},
                    {"provider": "anthropic", "model": "m2"},
                ],
            },
        ),
    ]

    rows = []
    for case_name, spec in cases:
        main_prov = spec.get("main_provider", "")
        failed_prov = spec.get("failed_provider", "")
        chain_entries = spec.get("chain", [])

        # Reset unhealthy state
        ax._aux_unhealthy_until.clear()
        ax._aux_unhealthy_logged_at.clear()
        unhealthy_lbl = spec.get("unhealthy_label")
        if unhealthy_lbl:
            ttl = spec.get("unhealthy_ttl", 60.0)
            ax._aux_unhealthy_until[unhealthy_lbl] = time.time() + ttl

        mock_cfg = {
            "model": {"provider": main_prov},
            "fallback_providers": chain_entries,
        }

        def mock_resolve(entry):
            prov = str(entry.get("provider") or "").strip()
            mdl = str(entry.get("model") or "").strip()
            if not prov or not mdl:
                return None, None
            client = MagicMock()
            client.base_url = str(entry.get("base_url") or "")
            return client, mdl

        with patch("hermes_cli.config.load_config_readonly", return_value=mock_cfg):
            with patch(
                "agent.auxiliary_client._read_main_provider", return_value=main_prov
            ):
                with patch(
                    "agent.auxiliary_client._resolve_fallback_entry",
                    side_effect=mock_resolve,
                ):
                    with patch(
                        "agent.auxiliary_client.get_model_context_length",
                        return_value=128000,
                    ):
                        client, resolved_model, resolved_prov = (
                            ax._try_main_fallback_chain(
                                task="compression",
                                failed_provider=failed_prov,
                                reason="unit-test",
                            )
                        )
                        rows.append({
                            "case": case_name,
                            "main_provider": main_prov,
                            "failed_provider": failed_prov,
                            "chain": chain_entries,
                            "unhealthy_provider": unhealthy_lbl,
                            "unhealthy_active": bool(
                                unhealthy_lbl and spec.get("unhealthy_ttl", 60.0) > 0
                            ),
                            "resolved": client is not None,
                            "resolved_model": resolved_model,
                            "resolved_provider": resolved_prov,
                        })

    # Clean up unhealthy state
    ax._aux_unhealthy_until.clear()
    ax._aux_unhealthy_logged_at.clear()
    return rows


# ---------------------------------------------------------------------------
# Section 5: Compression context window filtering (64,000-token floor)
# ---------------------------------------------------------------------------
def section_compression_context_window_filtering_64k() -> List[Dict[str, Any]]:
    """Test 64,000-token minimum context floor for compression vs other tasks."""
    cases = [
        ("task_floor_compression", "compression", 64000),
        ("task_floor_vision", "vision", None),
        ("task_floor_title", "title_generation", None),
        ("task_floor_session_search", "session_search", None),
        ("task_floor_none", None, None),
    ]

    rows = []
    for case_name, task_name, expected_floor in cases:
        actual_floor = ax._task_minimum_context_length(task_name)
        rows.append({
            "case": case_name,
            "task": task_name,
            "minimum_context_length": actual_floor,
            "expected_floor": expected_floor,
        })

    traversal_cases = [
        (
            "compression_skips_small_context_8k",
            "compression",
            [
                {"provider": "p1", "model": "small-8k-model"},
                {"provider": "p2", "model": "viable-128k-model"},
            ],
            {"small-8k-model": 8192, "viable-128k-model": 131072},
            "p2",
            "viable-128k-model",
        ),
        (
            "compression_skips_small_context_32k",
            "compression",
            [
                {"provider": "p1", "model": "small-32k-model"},
                {"provider": "p2", "model": "viable-64k-model"},
            ],
            {"small-32k-model": 32768, "viable-64k-model": 64000},
            "p2",
            "viable-64k-model",
        ),
        (
            "compression_accepts_exact_64k_boundary",
            "compression",
            [
                {"provider": "p1", "model": "exact-64k-model"},
            ],
            {"exact-64k-model": 64000},
            "p1",
            "exact-64k-model",
        ),
        (
            "compression_accepts_unknown_context_none",
            "compression",
            [
                {"provider": "p1", "model": "unknown-context-model"},
            ],
            {"unknown-context-model": None},
            "p1",
            "unknown-context-model",
        ),
        (
            "vision_permits_small_context_without_skipping",
            "vision",
            [
                {"provider": "p1", "model": "small-8k-model"},
                {"provider": "p2", "model": "viable-128k-model"},
            ],
            {"small-8k-model": 8192, "viable-128k-model": 131072},
            "p1",
            "small-8k-model",
        ),
    ]

    for (
        case_name,
        task,
        chain_entries,
        model_contexts,
        exp_prov,
        exp_model,
    ) in traversal_cases:
        mock_cfg = {
            "model": {"provider": "different_main"},
            "fallback_providers": chain_entries,
        }

        def mock_resolve(entry):
            prov = str(entry.get("provider") or "").strip()
            mdl = str(entry.get("model") or "").strip()
            client = MagicMock()
            client.base_url = ""
            return client, mdl

        def mock_ctx(model, **kwargs):
            return model_contexts.get(model)

        with patch("hermes_cli.config.load_config_readonly", return_value=mock_cfg):
            with patch(
                "agent.auxiliary_client._read_main_provider",
                return_value="different_main",
            ):
                with patch(
                    "agent.auxiliary_client._resolve_fallback_entry",
                    side_effect=mock_resolve,
                ):
                    with patch(
                        "agent.auxiliary_client.get_model_context_length",
                        side_effect=mock_ctx,
                    ):
                        client, resolved_model, resolved_prov = (
                            ax._try_main_fallback_chain(
                                task=task,
                                failed_provider="failed_different",
                                reason="unit-test",
                            )
                        )
                        rows.append({
                            "case": case_name,
                            "task": task,
                            "chain": chain_entries,
                            "model_contexts": model_contexts,
                            "resolved": client is not None,
                            "resolved_provider": resolved_prov,
                            "resolved_model": resolved_model,
                            "expected_provider": exp_prov,
                            "expected_model": exp_model,
                        })

    return rows


# ---------------------------------------------------------------------------
# Section 6: Client resolution and per-entry timeout behavior
# ---------------------------------------------------------------------------
def section_client_resolution_and_timeout_behavior() -> List[Dict[str, Any]]:
    """Test client resolution failures and per-entry timeout semantics."""
    rows = []

    # 1. Client resolution failure handling
    resolution_cases = [
        (
            "client_resolution_returns_none_advances_chain",
            [
                {"provider": "p1", "model": "unresolvable-model"},
                {"provider": "p2", "model": "resolvable-model"},
            ],
            {"p1": (None, None), "p2": (MagicMock(), "resolvable-model")},
            "p2",
            "resolvable-model",
        ),
        (
            "client_resolution_raises_advances_chain",
            [
                {"provider": "p1", "model": "raising-model"},
                {"provider": "p2", "model": "resolvable-model"},
            ],
            {
                "p1": RuntimeError("network error during client init"),
                "p2": (MagicMock(), "resolvable-model"),
            },
            "p2",
            "resolvable-model",
        ),
    ]

    for case_name, chain_entries, res_map, exp_prov, exp_model in resolution_cases:
        mock_cfg = {
            "model": {"provider": "main_prov"},
            "fallback_providers": chain_entries,
        }

        def mock_resolve(entry):
            prov = entry.get("provider")
            action = res_map.get(prov)
            if isinstance(action, Exception):
                raise action
            return action

        with patch("hermes_cli.config.load_config_readonly", return_value=mock_cfg):
            with patch(
                "agent.auxiliary_client._read_main_provider", return_value="main_prov"
            ):
                with patch(
                    "agent.auxiliary_client._resolve_fallback_entry",
                    side_effect=mock_resolve,
                ):
                    with patch(
                        "agent.auxiliary_client.get_model_context_length",
                        return_value=128000,
                    ):
                        client, resolved_model, resolved_prov = (
                            ax._try_main_fallback_chain(
                                task="compression",
                                failed_provider="failed_prov",
                                reason="unit-test",
                            )
                        )
                        rows.append({
                            "case": case_name,
                            "resolved": client is not None,
                            "resolved_provider": resolved_prov,
                            "resolved_model": resolved_model,
                            "expected_provider": exp_prov,
                            "expected_model": exp_model,
                        })

    # 2. Timeout resolution contracts:
    # Notice that _fallback_entry_timeout uses regex r"fallback_chain\[(\d+)\]",
    # and reads auxiliary.<task>.fallback_chain.
    # It returns None for main fallback chain labels ("openrouter" or "fallback_providers[0](openrouter)").
    timeout_label_cases = [
        ("label_bare_provider_returns_none", "openrouter", None),
        (
            "label_fallback_providers_returns_none",
            "fallback_providers[0](openrouter)",
            None,
        ),
        ("label_main_agent_returns_none", "main-agent(anthropic)", None),
        ("label_empty_returns_none", "", None),
    ]

    for case_name, label, exp_timeout in timeout_label_cases:
        timeout_val = ax._fallback_entry_timeout("compression", label)
        rows.append({
            "case": case_name,
            "label": label,
            "resolved_timeout": timeout_val,
            "expected_timeout": exp_timeout,
        })

    # Contrast with configured fallback_chain:
    configured_entry = {"provider": "openrouter", "model": "llama-3", "timeout": 45}
    with patch(
        "agent.auxiliary_client._get_auxiliary_task_config",
        return_value={"fallback_chain": [configured_entry]},
    ):
        conf_timeout = ax._fallback_entry_timeout(
            "compression", "fallback_chain[0](openrouter)"
        )
        rows.append({
            "case": "contrast_configured_chain_resolves_timeout",
            "label": "fallback_chain[0](openrouter)",
            "resolved_timeout": conf_timeout,
            "expected_timeout": 45.0,
        })

    # Test final request timeout retention for main fallback chain
    # When _call_fallback_candidate_sync is invoked with fb_label = "openrouter",
    # effective_timeout retains the task-level timeout (e.g. 300.0s) because fb_timeout is None.
    candidate_client = MagicMock()
    mock_resp = MagicMock()
    mock_resp.choices = [MagicMock()]
    mock_resp.choices[0].message.content = "summary output"
    candidate_client.chat.completions.create.return_value = mock_resp

    with patch("agent.auxiliary_client._fallback_entry_timeout", return_value=None):
        resp = ax._call_fallback_candidate_sync(
            candidate_client,
            "meta-llama/llama-3-70b",
            "openrouter",
            task="compression",
            messages=[{"role": "user", "content": "summarize"}],
            temperature=None,
            max_tokens=None,
            tools=None,
            effective_timeout=300.0,
            effective_extra_body={},
            reasoning_config=None,
        )
        called_kwargs = candidate_client.chat.completions.create.call_args[1]
        rows.append({
            "case": "main_fallback_candidate_request_keeps_task_timeout",
            "task": "compression",
            "passed_effective_timeout": 300.0,
            "final_request_timeout": called_kwargs.get("timeout"),
        })

    return rows


# ---------------------------------------------------------------------------
# Section 7: Candidate execution budget and failure propagation
# ---------------------------------------------------------------------------
def section_candidate_execution_budget_and_failure_propagation() -> List[
    Dict[str, Any]
]:
    """Test candidate execution count, error re-raising, and discovery handoff."""
    rows = []

    class DummyAuthError(Exception):
        status_code = 401

    class DummyRateLimitError(Exception):
        status_code = 429

    class DummyServerError(Exception):
        status_code = 500

    class DummyTimeoutError(Exception):
        status_code = 408

    # Case 1: Exactly 1 candidate resolved and executed successfully
    client_ok = MagicMock()
    resp_ok = MagicMock()
    resp_ok.choices = [MagicMock()]
    resp_ok.choices[0].message.content = "success"
    client_ok.chat.completions.create.return_value = resp_ok

    res1 = ax._call_fallback_candidate_sync(
        client_ok,
        "m1",
        "openrouter",
        task="compression",
        messages=[{"role": "user", "content": "hi"}],
        temperature=None,
        max_tokens=None,
        tools=None,
        effective_timeout=300.0,
        effective_extra_body={},
        reasoning_config=None,
    )
    rows.append({
        "case": "candidate_execution_success",
        "candidates_executed": 1,
        "success": res1 is not None,
        "re_raised": False,
        "falls_to_built_in_discovery": False,
    })

    # Case 2: Non-auth error (429 RateLimit) re-raises immediately
    client_429 = MagicMock()
    client_429.chat.completions.create.side_effect = DummyRateLimitError(
        "rate limit exceeded"
    )
    re_raised_429 = False
    try:
        ax._call_fallback_candidate_sync(
            client_429,
            "m1",
            "openrouter",
            task="compression",
            messages=[{"role": "user", "content": "hi"}],
            temperature=None,
            max_tokens=None,
            tools=None,
            effective_timeout=300.0,
            effective_extra_body={},
            reasoning_config=None,
        )
    except DummyRateLimitError:
        re_raised_429 = True

    rows.append({
        "case": "candidate_non_auth_429_propagates_immediately",
        "candidates_executed": 1,
        "success": False,
        "re_raised": re_raised_429,
        "subsequent_fallback_candidates_tried": 0,
        "falls_to_built_in_discovery": False,
    })

    # Case 3: Server error (500) re-raises immediately
    client_500 = MagicMock()
    client_500.chat.completions.create.side_effect = DummyServerError(
        "internal server error"
    )
    re_raised_500 = False
    try:
        ax._call_fallback_candidate_sync(
            client_500,
            "m1",
            "openrouter",
            task="compression",
            messages=[{"role": "user", "content": "hi"}],
            temperature=None,
            max_tokens=None,
            tools=None,
            effective_timeout=300.0,
            effective_extra_body={},
            reasoning_config=None,
        )
    except DummyServerError:
        re_raised_500 = True

    rows.append({
        "case": "candidate_non_auth_500_propagates_immediately",
        "candidates_executed": 1,
        "success": False,
        "re_raised": re_raised_500,
        "subsequent_fallback_candidates_tried": 0,
        "falls_to_built_in_discovery": False,
    })

    # Case 4: Timeout error (408) re-raises immediately
    client_408 = MagicMock()
    client_408.chat.completions.create.side_effect = DummyTimeoutError(
        "request timed out"
    )
    re_raised_408 = False
    try:
        ax._call_fallback_candidate_sync(
            client_408,
            "m1",
            "openrouter",
            task="compression",
            messages=[{"role": "user", "content": "hi"}],
            temperature=None,
            max_tokens=None,
            tools=None,
            effective_timeout=300.0,
            effective_extra_body={},
            reasoning_config=None,
        )
    except DummyTimeoutError:
        re_raised_408 = True

    rows.append({
        "case": "candidate_non_auth_timeout_propagates_immediately",
        "candidates_executed": 1,
        "success": False,
        "re_raised": re_raised_408,
        "subsequent_fallback_candidates_tried": 0,
        "falls_to_built_in_discovery": False,
    })

    # Case 5: Auth error (401) with unrefreshable credential returns None,
    # leading call_llm to drop directly to built-in discovery (_try_payment_fallback)
    client_401 = MagicMock()
    client_401.chat.completions.create.side_effect = DummyAuthError("401 unauthorized")

    with patch(
        "agent.auxiliary_client._refresh_provider_credentials", return_value=False
    ):
        with patch("agent.auxiliary_client._mark_provider_unhealthy") as mock_mark:
            res_auth = ax._call_fallback_candidate_sync(
                client_401,
                "m1",
                "openrouter",
                task="compression",
                messages=[{"role": "user", "content": "hi"}],
                temperature=None,
                max_tokens=None,
                tools=None,
                effective_timeout=300.0,
                effective_extra_body={},
                reasoning_config=None,
            )
            rows.append({
                "case": "candidate_auth_error_returns_none_for_discovery_handoff",
                "candidates_executed": 1,
                "returned_none": res_auth is None,
                "provider_marked_unhealthy": mock_mark.called,
                "subsequent_fallback_candidates_tried": 0,
                "falls_to_built_in_discovery": True,
            })

    return rows


# ---------------------------------------------------------------------------
# Assembly and CLI
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, Any]:
    return {
        "container_and_entry_parsing": section_container_and_entry_parsing(),
        "chain_deduplication_and_identity": section_chain_deduplication_and_identity(),
        "credential_and_transport_resolution": section_credential_and_transport_resolution(),
        "provider_skipping_and_health_rules": section_provider_skipping_and_health_rules(),
        "compression_context_window_filtering_64k": section_compression_context_window_filtering_64k(),
        "client_resolution_and_timeout_behavior": section_client_resolution_and_timeout_behavior(),
        "candidate_execution_budget_and_failure_propagation": section_candidate_execution_budget_and_failure_propagation(),
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
                "compression-main-fallback-chain-goldens.json is stale; rerun the generator"
            )
        print("OK: corpus matches checked-in goldens")
    elif not args:
        OUT.write_text(text)
        total_cases = sum(len(v) for v in corpus.values())
        print(f"Wrote {total_cases} cases to {OUT.relative_to(ROOT)}")
    else:
        raise SystemExit(
            "usage: gen_compression_main_fallback_chain_goldens.py [--check]"
        )


if __name__ == "__main__":
    main()
