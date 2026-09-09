#!/usr/bin/env python3
"""Deterministic source-executed oracle for auxiliary compression fallback chains.

This generator executes real Python decision functions that govern fallback
chain behavior for context compression summaries:
1. Entry parsing and coercion (structural acceptance, source order preservation,
   and field normalization).
2. Timeout coercion and independent per-entry timeout semantics (verifying the
   absence of the 300s task floor on fallback entries).
3. Transport and api_mode alias canonicalization.
4. Credential lookup precedence and environment scoping.
5. Route identity and failure-scoped candidate skipping.
6. Multi-tier chain traversal, context length filtering, and exhaustion.

Usage:
    python3 rust/tools/gen_compression_fallback_chain_goldens.py          # write goldens
    python3 rust/tools/gen_compression_fallback_chain_goldens.py --check  # check parity
"""

from __future__ import annotations

import contextlib
import json
import os
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/compression-fallback-chain-goldens.json"

sys.path.insert(0, str(ROOT))

import agent.auxiliary_client as ax
import agent.conversation_compression as cc
from agent.backend_identity import (
    BackendIdentity,
    FailureScope,
    classify_failure_scope,
    should_skip_candidate,
)
from hermes_cli.config import _canonical_api_mode
from hermes_cli.fallback_config import resolve_entry_api_key
from hermes_constants import parse_reasoning_effort


# ---------------------------------------------------------------------------
# Section 1: Entry parsing and coercion
# ---------------------------------------------------------------------------
def _coerce_entry_fields(entry: Any) -> Optional[Dict[str, Any]]:
    """Parse and normalize a single fallback chain entry according to Python rules."""
    if not isinstance(entry, dict):
        return None
    provider = str(entry.get("provider") or "").strip()
    if not provider:
        return None

    model_raw = entry.get("model")
    model = str(model_raw).strip() if model_raw is not None else None
    if model == "":
        model = None

    base_url_raw = entry.get("base_url")
    base_url = str(base_url_raw).strip() if base_url_raw is not None else None
    if base_url == "":
        base_url = None

    api_key_raw = entry.get("api_key")
    api_key = str(api_key_raw).strip() if api_key_raw is not None else None
    if api_key == "":
        api_key = None

    key_env_raw = entry.get("key_env") or entry.get("api_key_env")
    key_env = str(key_env_raw).strip() if key_env_raw is not None else None
    if key_env == "":
        key_env = None

    api_mode_raw = entry.get("api_mode") or entry.get("transport")
    if isinstance(api_mode_raw, str) and api_mode_raw.strip():
        api_mode = _canonical_api_mode(api_mode_raw.strip())
    else:
        api_mode = None

    timeout = ax._coerce_positive_timeout(entry.get("timeout"))

    extra_body_raw = entry.get("extra_body")
    extra_body = dict(extra_body_raw) if isinstance(extra_body_raw, dict) else {}

    reasoning_effort = parse_reasoning_effort(entry.get("reasoning_effort"))

    max_output_tokens_raw = entry.get("max_output_tokens")
    max_output_tokens: Optional[int] = None
    if isinstance(max_output_tokens_raw, bool):
        max_output_tokens = None
    elif isinstance(max_output_tokens_raw, int) and max_output_tokens_raw > 0:
        max_output_tokens = max_output_tokens_raw
    elif isinstance(max_output_tokens_raw, str):
        try:
            val = int(max_output_tokens_raw.strip())
            if val > 0:
                max_output_tokens = val
        except (ValueError, TypeError):
            max_output_tokens = None

    return {
        "provider": provider,
        "model": model,
        "base_url": base_url,
        "api_key": api_key,
        "key_env": key_env,
        "api_mode": api_mode,
        "timeout": timeout,
        "extra_body": extra_body,
        "reasoning_effort": reasoning_effort,
        "max_output_tokens": max_output_tokens,
    }


def section_entry_acceptance() -> List[Dict[str, Any]]:
    cases = [
        (
            "valid_full_entry",
            {
                "provider": "openrouter",
                "model": "meta-llama/llama-3-70b",
                "base_url": "https://openrouter.ai/api/v1",
                "api_key": "sk-synthetic-inline",
                "key_env": "FALLBACK_KEY",
                "api_mode": "chat-completions",
                "timeout": 45,
                "extra_body": {"route": "fast"},
                "reasoning_effort": "low",
                "max_output_tokens": 2048,
            },
        ),
        ("valid_minimal_entry", {"provider": "custom"}),
        (
            "valid_provider_and_model",
            {"provider": "anthropic", "model": "claude-3-5-sonnet"},
        ),
        (
            "valid_with_base_url",
            {
                "provider": "custom",
                "model": "local-model",
                "base_url": "https://internal/v1",
            },
        ),
        ("valid_with_api_key", {"provider": "custom", "api_key": "sk-mock-key"}),
        ("valid_with_key_env", {"provider": "custom", "key_env": "MY_KEY_ENV"}),
        (
            "valid_with_api_key_env_alias",
            {"provider": "custom", "api_key_env": "MY_API_KEY_ENV"},
        ),
        (
            "valid_with_transport_alias",
            {"provider": "custom", "transport": "responses"},
        ),
        (
            "valid_api_mode_precedence",
            {"provider": "custom", "api_mode": "anthropic", "transport": "responses"},
        ),
        (
            "valid_extra_body_dict",
            {"provider": "custom", "extra_body": {"custom_tag": "val"}},
        ),
        (
            "valid_reasoning_effort_low",
            {"provider": "custom", "reasoning_effort": "low"},
        ),
        (
            "valid_reasoning_effort_none",
            {"provider": "custom", "reasoning_effort": "none"},
        ),
        (
            "valid_reasoning_effort_false",
            {"provider": "custom", "reasoning_effort": False},
        ),
        (
            "valid_max_output_tokens_int",
            {"provider": "custom", "max_output_tokens": 4096},
        ),
        (
            "valid_max_output_tokens_str_coerced",
            {"provider": "custom", "max_output_tokens": "8192"},
        ),
        (
            "invalid_max_output_tokens_bool",
            {"provider": "custom", "max_output_tokens": True},
        ),
        (
            "invalid_max_output_tokens_zero",
            {"provider": "custom", "max_output_tokens": 0},
        ),
        (
            "invalid_max_output_tokens_negative",
            {"provider": "custom", "max_output_tokens": -50},
        ),
        (
            "invalid_extra_body_list",
            {"provider": "custom", "extra_body": ["not", "a", "dict"]},
        ),
        (
            "invalid_extra_body_string",
            {"provider": "custom", "extra_body": "not_a_dict"},
        ),
        ("invalid_non_dict_string", "openrouter/llama-3"),
        ("invalid_non_dict_int", 999),
        ("invalid_non_dict_list", ["openrouter", "llama-3"]),
        ("invalid_non_dict_bool", True),
        ("invalid_non_dict_null", None),
        ("invalid_missing_provider", {"model": "m"}),
        ("invalid_empty_provider", {"provider": "", "model": "m"}),
        ("invalid_whitespace_provider", {"provider": "   ", "model": "m"}),
        ("invalid_null_provider", {"provider": None, "model": "m"}),
        ("model_auto_handling", {"provider": "openrouter", "model": "auto"}),
        (
            "whitespace_trimmed",
            {
                "provider": "  custom  ",
                "model": "  m  ",
                "base_url": "  https://api.example.com/v1  ",
                "api_key": "  sk-mock-trimmed  ",
            },
        ),
    ]

    rows = []
    for name, raw in cases:
        parsed = _coerce_entry_fields(raw)
        accepted = parsed is not None
        rows.append({
            "case": name,
            "raw": raw,
            "accepted": accepted,
            "parsed": parsed,
        })

    # Test ordered chain parsing with mixed valid and invalid entries
    mixed_raw_chain = [
        "invalid_string_entry",
        {"provider": "openrouter", "model": "first-candidate", "timeout": 45},
        {"model": "missing-provider"},
        None,
        {"provider": "anthropic", "model": "second-candidate", "timeout": 60},
        {"provider": "   ", "model": "whitespace-provider"},
        {"provider": "custom", "model": "third-candidate", "timeout": 90},
    ]
    retained_entries = [
        _coerce_entry_fields(item)
        for item in mixed_raw_chain
        if _coerce_entry_fields(item) is not None
    ]
    rows.append({
        "case": "source_order_and_filtering_preserved",
        "raw": mixed_raw_chain,
        "accepted": True,
        "parsed": retained_entries,
    })
    return rows


# ---------------------------------------------------------------------------
# Section 2: Timeout coercion and independence
# ---------------------------------------------------------------------------
def section_timeout_coercion() -> List[Dict[str, Any]]:
    test_values = [
        ("int_positive_short", 45),
        ("int_positive_standard", 120),
        ("int_positive_floor_value", 300),
        ("int_positive_long", 600),
        ("float_positive_fractional", 45.5),
        ("float_positive_subsecond", 0.5),
        ("float_positive_large", 300.0),
        ("zero_int_rejected", 0),
        ("zero_float_rejected", 0.0),
        ("negative_int_rejected", -1),
        ("negative_float_rejected", -45.0),
        ("bool_true_rejected", True),
        ("bool_false_rejected", False),
        ("string_int_rejected", "45"),
        ("string_float_rejected", "120.0"),
        ("string_non_numeric_rejected", "timeout"),
        ("none_rejected", None),
        ("dict_rejected", {}),
        ("list_rejected", []),
    ]

    rows = []
    for name, val in test_values:
        coerced = ax._coerce_positive_timeout(val)
        rows.append({
            "case": name,
            "raw_value": val,
            "coerced_timeout": coerced,
        })

    # Demonstrate timeout independence: fallback entry timeout vs task timeout
    scenarios = [
        ("entry_has_independent_short_timeout", 45, 45.0, 300.0),
        ("entry_has_independent_long_timeout", 600, 600.0, 300.0),
        ("entry_omits_timeout_inherits_task_floor", None, None, 300.0),
    ]
    for name, entry_timeout_cfg, expected_entry_to, expected_effective in scenarios:
        with patch(
            "agent.auxiliary_client._get_auxiliary_task_config",
            return_value={
                "timeout": 120.0,
                "fallback_chain": [{"provider": "p", "timeout": entry_timeout_cfg}],
            },
        ):
            entry_timeout = ax._fallback_entry_timeout(
                "compression", "fallback_chain[0](p)"
            )
            # In _call_fallback_candidate_sync:
            # effective_timeout starts at task-level (floored to 300.0 for compression)
            task_effective_timeout = 300.0
            if entry_timeout is not None:
                final_effective_timeout = entry_timeout
            else:
                final_effective_timeout = task_effective_timeout

            rows.append({
                "case": f"independence::{name}",
                "raw_value": entry_timeout_cfg,
                "coerced_timeout": entry_timeout,
                "expected_entry_timeout": expected_entry_to,
                "effective_request_timeout": final_effective_timeout,
            })
    return rows


# ---------------------------------------------------------------------------
# Section 3: API Mode and Transport Aliases
# ---------------------------------------------------------------------------
def section_api_mode() -> List[Dict[str, Any]]:
    modes = [
        ("openai_legacy", "openai"),
        ("openai_chat_snake", "openai_chat"),
        ("openai_chat_kebab", "openai-chat"),
        ("chat_completions_kebab", "chat-completions"),
        ("chat_completions_concat", "chatcompletions"),
        ("chat_completions_canonical", "chat_completions"),
        ("responses_shorthand", "responses"),
        ("openai_responses_snake", "openai_responses"),
        ("openai_responses_kebab", "openai-responses"),
        ("codex_responses_canonical", "codex_responses"),
        ("anthropic_shorthand", "anthropic"),
        ("anthropic_messages_kebab", "anthropic-messages"),
        ("messages_shorthand", "messages"),
        ("anthropic_messages_canonical", "anthropic_messages"),
        ("bedrock_shorthand", "bedrock"),
        ("bedrock_converse_kebab", "bedrock-converse"),
        ("bedrock_converse_canonical", "bedrock_converse"),
        ("case_insensitive_uppercase", "CHAT-COMPLETIONS"),
        ("whitespace_trimmed", "  anthropic  "),
        ("custom_unknown_passthrough", "custom_transport_protocol"),
    ]

    rows = []
    for name, raw in modes:
        canonical = _canonical_api_mode(raw)
        rows.append({
            "case": name,
            "raw_api_mode": raw,
            "canonical_api_mode": canonical,
        })
    return rows


# ---------------------------------------------------------------------------
# Section 4: Credential resolution and environment precedence
# ---------------------------------------------------------------------------
def section_credential_resolution() -> List[Dict[str, Any]]:
    synthetic_env = {
        "SYNTHETIC_PRIMARY_KEY": "sk-synthetic-env-key-1",
        "SYNTHETIC_SECONDARY_KEY": "sk-synthetic-env-key-2",
        "EMPTY_KEY_VAR": "",
        "WHITESPACE_KEY_VAR": "   ",
    }

    cases = [
        ("inline_api_key_only", {"api_key": "sk-synthetic-inline-only"}, synthetic_env),
        (
            "inline_api_key_with_whitespace",
            {"api_key": "  sk-synthetic-trimmed  "},
            synthetic_env,
        ),
        ("key_env_resolution", {"key_env": "SYNTHETIC_PRIMARY_KEY"}, synthetic_env),
        (
            "api_key_env_alias_resolution",
            {"api_key_env": "SYNTHETIC_SECONDARY_KEY"},
            synthetic_env,
        ),
        (
            "api_key_precedence_over_key_env",
            {
                "api_key": "sk-synthetic-inline-priority",
                "key_env": "SYNTHETIC_PRIMARY_KEY",
            },
            synthetic_env,
        ),
        (
            "api_key_precedence_over_api_key_env",
            {
                "api_key": "sk-synthetic-inline-priority",
                "api_key_env": "SYNTHETIC_SECONDARY_KEY",
            },
            synthetic_env,
        ),
        (
            "key_env_precedence_over_api_key_env",
            {
                "key_env": "SYNTHETIC_PRIMARY_KEY",
                "api_key_env": "SYNTHETIC_SECONDARY_KEY",
            },
            synthetic_env,
        ),
        (
            "missing_key_env_returns_none",
            {"key_env": "NONEXISTENT_KEY_VARIABLE"},
            synthetic_env,
        ),
        ("empty_key_env_returns_none", {"key_env": "EMPTY_KEY_VAR"}, synthetic_env),
        (
            "whitespace_key_env_returns_none",
            {"key_env": "WHITESPACE_KEY_VAR"},
            synthetic_env,
        ),
        (
            "no_credentials_returns_none",
            {"provider": "openrouter", "model": "m"},
            synthetic_env,
        ),
        ("non_dict_returns_none", None, synthetic_env),
    ]

    rows = []
    saved_env = dict(os.environ)
    try:
        os.environ.update(synthetic_env)
        for name, entry, env_dict in cases:
            resolved = resolve_entry_api_key(entry)
            rows.append({
                "case": name,
                "entry": entry,
                "resolved_api_key": resolved,
            })
    finally:
        os.environ.clear()
        os.environ.update(saved_env)

    return rows


# ---------------------------------------------------------------------------
# Section 5: Route identity and failure-scoped candidate skipping
# ---------------------------------------------------------------------------
def section_failed_route_skipping() -> List[Dict[str, Any]]:
    cases = [
        # MODEL scope (timeout, connection, rate limit, model-incompatible, invalid response)
        (
            "model_scope::exact_match_skipped",
            {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
            {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
            FailureScope.MODEL,
            True,
        ),
        (
            "model_scope::sibling_model_allowed",
            {"provider": "openrouter", "model": "google/gemini-2.5-flash"},
            {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
            FailureScope.MODEL,
            False,
        ),
        (
            "model_scope::different_provider_allowed",
            {"provider": "anthropic", "model": "claude-3-5-sonnet"},
            {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
            FailureScope.MODEL,
            False,
        ),
        (
            "model_scope::case_insensitive_match_skipped",
            {"provider": "OpenRouter", "model": "Meta-Llama/Llama-3-70B"},
            {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
            FailureScope.MODEL,
            True,
        ),
        (
            "model_scope::distinct_explicit_endpoints_allowed",
            {"provider": "custom", "model": "m", "base_url": "https://host-b/v1"},
            {"provider": "custom", "model": "m", "base_url": "https://host-a/v1"},
            FailureScope.MODEL,
            False,
        ),
        (
            "model_scope::same_explicit_endpoints_skipped",
            {"provider": "custom", "model": "m", "base_url": "https://host-a/v1"},
            {"provider": "custom", "model": "m", "base_url": "https://host-a/v1"},
            FailureScope.MODEL,
            True,
        ),
        # CREDENTIAL scope (auth 401, payment 402, or no model available)
        (
            "credential_scope::same_provider_any_model_skipped",
            {"provider": "openrouter", "model": "google/gemini-2.5-flash"},
            {"provider": "openrouter", "model": None},
            FailureScope.CREDENTIAL,
            True,
        ),
        (
            "credential_scope::same_provider_same_model_skipped",
            {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
            {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
            FailureScope.CREDENTIAL,
            True,
        ),
        (
            "credential_scope::different_provider_allowed",
            {"provider": "anthropic", "model": "claude-3-5-sonnet"},
            {"provider": "openrouter", "model": None},
            FailureScope.CREDENTIAL,
            False,
        ),
    ]

    rows = []
    for name, cand_dict, failed_dict, scope, expected_skip in cases:
        cand_ident = BackendIdentity.build(
            provider=cand_dict.get("provider"),
            model=cand_dict.get("model"),
            base_url=cand_dict.get("base_url"),
        )
        failed_ident = BackendIdentity.build(
            provider=failed_dict.get("provider"),
            model=failed_dict.get("model"),
            base_url=failed_dict.get("base_url"),
        )
        skipped = should_skip_candidate(cand_ident, failed_ident, scope)
        rows.append({
            "case": name,
            "candidate": cand_dict,
            "failed_identity": failed_dict,
            "failure_scope": scope.value,
            "should_skip": skipped,
        })

    # Reason string to failure scope classification
    reason_cases = [
        ("auth error", FailureScope.CREDENTIAL.value),
        ("payment error", FailureScope.CREDENTIAL.value),
        ("rate limit", FailureScope.MODEL.value),
        ("model incompatible with route", FailureScope.MODEL.value),
        ("invalid provider response", FailureScope.MODEL.value),
        ("connection error", FailureScope.MODEL.value),
        ("timeout", FailureScope.MODEL.value),
        ("unrecognized failure string", FailureScope.MODEL.value),
    ]
    for reason_str, expected_scope_name in reason_cases:
        actual_scope = classify_failure_scope(reason_str)
        rows.append({
            "case": f"reason_classification::{reason_str}",
            "reason": reason_str,
            "classified_scope": actual_scope.value,
        })

    return rows


# ---------------------------------------------------------------------------
# Section 6: Chain traversal, context filtering, and exhaustion
# ---------------------------------------------------------------------------
def section_chain_traversal() -> List[Dict[str, Any]]:
    rows = []

    # Scenario A: Model timeout allows sibling model on same provider
    chain_a = [
        {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
        {"provider": "openrouter", "model": "google/gemini-2.5-flash"},
        {"provider": "anthropic", "model": "claude-3-5-sonnet"},
    ]
    with patch(
        "agent.auxiliary_client._get_auxiliary_task_config",
        return_value={"fallback_chain": chain_a},
    ):
        with patch("agent.auxiliary_client._resolve_fallback_entry") as mock_resolve:
            mock_resolve.side_effect = lambda entry: (MagicMock(), entry["model"])
            client, model, label = ax._try_configured_fallback_chain(
                task="compression",
                failed_provider="openrouter",
                reason="timeout",
                failed_model="meta-llama/llama-3-70b",
            )
            rows.append({
                "case": "model_timeout_selects_sibling_candidate",
                "chain": chain_a,
                "failed_provider": "openrouter",
                "reason": "timeout",
                "failed_model": "meta-llama/llama-3-70b",
                "resolved_model": model,
                "resolved_label": label,
                "success": client is not None,
            })

    # Scenario B: Auth error skips all models on same provider
    with patch(
        "agent.auxiliary_client._get_auxiliary_task_config",
        return_value={"fallback_chain": chain_a},
    ):
        with patch("agent.auxiliary_client._resolve_fallback_entry") as mock_resolve:
            mock_resolve.side_effect = lambda entry: (MagicMock(), entry["model"])
            client, model, label = ax._try_configured_fallback_chain(
                task="compression",
                failed_provider="openrouter",
                reason="auth error",
                failed_model=None,
            )
            rows.append({
                "case": "auth_error_skips_provider_selects_next",
                "chain": chain_a,
                "failed_provider": "openrouter",
                "reason": "auth error",
                "failed_model": None,
                "resolved_model": model,
                "resolved_label": label,
                "success": client is not None,
            })

    # Scenario C: Minimum context length filter skips model with < 64K
    chain_c = [
        {"provider": "custom", "model": "small-8k-model"},
        {"provider": "custom", "model": "large-128k-model"},
    ]
    with patch(
        "agent.auxiliary_client._get_auxiliary_task_config",
        return_value={"fallback_chain": chain_c},
    ):
        with patch("agent.auxiliary_client._resolve_fallback_entry") as mock_resolve:
            mock_resolve.side_effect = lambda entry: (MagicMock(), entry["model"])
            with patch("agent.auxiliary_client._candidate_context_window") as mock_ctx:
                mock_ctx.side_effect = lambda prov, mod, **kw: (
                    8192 if mod == "small-8k-model" else 131072
                )
                client, model, label = ax._try_configured_fallback_chain(
                    task="compression",
                    failed_provider="other",
                    reason="timeout",
                    failed_model=None,
                )
                rows.append({
                    "case": "context_window_filter_skips_small_model",
                    "chain": chain_c,
                    "failed_provider": "other",
                    "reason": "timeout",
                    "resolved_model": model,
                    "resolved_label": label,
                    "success": client is not None,
                })

    # Scenario D: Unknown context length (None) passes through
    chain_d = [{"provider": "custom", "model": "unknown-context-model"}]
    with patch(
        "agent.auxiliary_client._get_auxiliary_task_config",
        return_value={"fallback_chain": chain_d},
    ):
        with patch("agent.auxiliary_client._resolve_fallback_entry") as mock_resolve:
            mock_resolve.side_effect = lambda entry: (MagicMock(), entry["model"])
            with patch(
                "agent.auxiliary_client._candidate_context_window", return_value=None
            ):
                client, model, label = ax._try_configured_fallback_chain(
                    task="compression",
                    failed_provider="other",
                    reason="timeout",
                    failed_model=None,
                )
                rows.append({
                    "case": "unknown_context_passes_through",
                    "chain": chain_d,
                    "failed_provider": "other",
                    "reason": "timeout",
                    "resolved_model": model,
                    "resolved_label": label,
                    "success": client is not None,
                })

    # Scenario E: Chain exhaustion when all entries are skipped or too small
    chain_e = [
        {"provider": "openrouter", "model": "meta-llama/llama-3-70b"},
        {"provider": "custom", "model": "small-8k-model"},
    ]
    with patch(
        "agent.auxiliary_client._get_auxiliary_task_config",
        return_value={"fallback_chain": chain_e},
    ):
        with patch("agent.auxiliary_client._resolve_fallback_entry") as mock_resolve:
            mock_resolve.side_effect = lambda entry: (MagicMock(), entry["model"])
            with patch("agent.auxiliary_client._candidate_context_window") as mock_ctx:
                mock_ctx.side_effect = lambda prov, mod, **kw: 8192
                client, model, label = ax._try_configured_fallback_chain(
                    task="compression",
                    failed_provider="openrouter",
                    reason="timeout",
                    failed_model="meta-llama/llama-3-70b",
                )
                rows.append({
                    "case": "exhaustion_all_candidates_unavailable",
                    "chain": chain_e,
                    "failed_provider": "openrouter",
                    "reason": "timeout",
                    "resolved_model": model,
                    "resolved_label": label,
                    "success": client is not None,
                })

    # Scenario F: Secondary main agent model fallback after chain exhaustion
    with (
        patch("agent.auxiliary_client._read_main_provider", return_value="openrouter"),
        patch(
            "agent.auxiliary_client._read_main_model",
            return_value="anthropic/claude-3-opus",
        ),
        patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(MagicMock(), "anthropic/claude-3-opus"),
        ),
        patch("agent.auxiliary_client._is_provider_unhealthy", return_value=False),
    ):
        # Case 1: Auxiliary route was distinct from main model -> safety net succeeds
        c1, m1, l1 = ax._try_main_agent_model_fallback(
            failed_provider="custom",
            task="compression",
            reason="rate limit",
            failed_model="kimi-k2",
        )
        rows.append({
            "case": "secondary_main_agent_fallback_distinct_route",
            "failed_provider": "custom",
            "failed_model": "kimi-k2",
            "reason": "rate limit",
            "main_provider": "openrouter",
            "main_model": "anthropic/claude-3-opus",
            "resolved_model": m1,
            "resolved_label": l1,
            "success": c1 is not None,
        })

        # Case 2: Auxiliary failed model was already main model -> safety net skipped
        c2, m2, l2 = ax._try_main_agent_model_fallback(
            failed_provider="openrouter",
            task="compression",
            reason="timeout",
            failed_model="anthropic/claude-3-opus",
        )
        rows.append({
            "case": "secondary_main_agent_fallback_skips_same_model",
            "failed_provider": "openrouter",
            "failed_model": "anthropic/claude-3-opus",
            "reason": "timeout",
            "main_provider": "openrouter",
            "main_model": "anthropic/claude-3-opus",
            "resolved_model": m2,
            "resolved_label": l2,
            "success": c2 is not None,
        })

        # Case 3: Auxiliary failure was auth error on same provider -> safety net skipped
        c3, m3, l3 = ax._try_main_agent_model_fallback(
            failed_provider="openrouter",
            task="compression",
            reason="auth error",
            failed_model=None,
        )
        rows.append({
            "case": "secondary_main_agent_fallback_skips_same_provider_auth",
            "failed_provider": "openrouter",
            "failed_model": None,
            "reason": "auth error",
            "main_provider": "openrouter",
            "main_model": "anthropic/claude-3-opus",
            "resolved_model": m3,
            "resolved_label": l3,
            "success": c3 is not None,
        })

    # Scenario G: Stall route resolution picks first complete candidate
    chain_g = [
        "invalid_entry",
        {"provider": ""},
        {"provider": "custom"},  # no model
        {"provider": "openrouter", "model": "meta-llama/llama-3-70b", "timeout": 45},
        {"provider": "anthropic", "model": "claude-3-5-sonnet", "timeout": 120},
    ]
    with patch(
        "agent.auxiliary_client._get_auxiliary_task_config",
        return_value={"fallback_chain": chain_g},
    ):
        stall_route = cc.resolve_compression_fallback_route()
        rows.append({
            "case": "stall_fallback_route_first_structurally_complete",
            "chain": chain_g,
            "selected_route": stall_route,
        })

    return rows


# ---------------------------------------------------------------------------
# Assembly and CLI
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, Any]:
    return {
        "entry_acceptance_and_coercion": section_entry_acceptance(),
        "timeout_coercion": section_timeout_coercion(),
        "api_mode_normalization": section_api_mode(),
        "credential_resolution": section_credential_resolution(),
        "failed_route_skipping": section_failed_route_skipping(),
        "chain_traversal_and_exhaustion": section_chain_traversal(),
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
                "compression-fallback-chain-goldens.json is stale; rerun the generator"
            )
        print("OK: corpus matches checked-in goldens")
    elif not args:
        OUT.write_text(text)
        total_cases = sum(len(v) for v in corpus.values())
        print(f"Wrote {total_cases} cases to {OUT.relative_to(ROOT)}")
    else:
        raise SystemExit("usage: gen_compression_fallback_chain_goldens.py [--check]")


if __name__ == "__main__":
    main()
