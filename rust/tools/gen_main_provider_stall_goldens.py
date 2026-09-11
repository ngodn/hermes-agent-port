#!/usr/bin/env python3
"""Deterministic source-executed oracle for main-provider stall and timeout contract.

This generator audits and executes live Python decision functions and runtime transitions
governing the main-provider request timeout and streaming stale detection contract:
1. Timeout coercion, normalization, and bounds (hermes_cli/timeouts.py and agent/deadline.py).
2. Request timeout precedence across per-model, per-provider, env fallback, and default.
3. Stale timeout precedence across per-model, per-provider, env fallback, reasoning floor, and default.
4. Reasoning-model stale timeout floors across Nemotron, DeepSeek, Qwen, o-series, Claude, Grok, etc.
5. Context-size token estimation across messages lists, Chat Completions dicts, and Responses API dicts.
6. Context-size scaling tiers for streaming (240s/300s) and non-streaming (150s/240s) plus run budget caps.
7. Local-endpoint detection (loopback, container DNS, RFC 1918, Tailscale) and scaling (900s / inf).
8. Streaming socket read timeout, connect caps, and coordination with stale detection.
9. First-byte versus inter-chunk single-deadline stale detection, 30s heartbeats, and status notifications.
10. Pre-visible versus post-visible stream failure boundaries, partial stubs, and tool retry gates.
11. Stale streak lifecycle, give-up ceiling circuit breaker, replay limits, and reset transitions.
12. Buffered (non-streaming) timeout behavior, worker poll loops, and direct inline timer watchdogs.
13. Provider-specific exclusions and boundaries (AWS Bedrock, OpenAI Codex, Anthropic Messages).

Usage:
    python3 rust/tools/gen_main_provider_stall_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_stall_goldens.py --check  # check parity
"""

from __future__ import annotations

import json
import math
import os
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-stall-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies (e.g. httpx) by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

import httpx  # noqa: E402
from hermes_cli.timeouts import (
    _coerce_timeout,
    get_provider_request_timeout,
    get_provider_stale_timeout,
    _get_model_config,
)
from agent.deadline import clamp_timeout, MAX_SAFE_TIMEOUT_S
from agent.reasoning_timeouts import (
    get_reasoning_stale_timeout_floor,
    _REASONING_STALE_TIMEOUT_FLOORS,
)
from agent.model_metadata import is_local_endpoint
from agent.chat_completion_helpers import (
    estimate_request_context_tokens,
    _stale_streak,
    _bump_stale_streak,
    _reset_stale_streak,
    _check_stale_giveup,
    _record_interrupted_provider_wait,
    _derive_stream_stale_timeout,
    _resolve_direct_stale_timeout,
    _inline_nonstream_hard_timeout,
    try_activate_fallback,
    PARTIAL_STREAM_STUB_ID,
    FINISH_REASON_LENGTH,
)
from agent.agent_runtime_helpers import restore_primary_runtime
from run_agent import AIAgent


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
        return agent


# ---------------------------------------------------------------------------
# Section 1: Timeout Coercion and Normalization
# ---------------------------------------------------------------------------
def section_timeout_coercion_and_normalization() -> List[Dict[str, Any]]:
    test_inputs = [
        ("valid_int", 30, 30.0),
        ("valid_float", 45.5, 45.5),
        ("valid_int_str", "120", 120.0),
        ("valid_float_str", "1800.0", 1800.0),
        ("zero_int", 0, None),
        ("zero_float", 0.0, None),
        ("zero_str", "0", None),
        ("negative_int", -1, None),
        ("negative_float", -45.5, None),
        ("negative_str", "-10", None),
        ("empty_str", "", None),
        ("whitespace_str", "   ", None),
        ("invalid_str", "fast", None),
        ("invalid_unit_str", "120s", None),
        ("none_input", None, None),
        ("dict_input", {"timeout": 30}, None),
        ("list_input", [30], None),
        ("boolean_true", True, 1.0),
        ("boolean_false", False, None),
    ]

    cases = []
    for name, raw, expected_val in test_inputs:
        coerced = _coerce_timeout(raw)
        cases.append({
            "case_name": f"coerce_{name}",
            "raw_input": repr(raw) if not isinstance(raw, (int, float, str, type(None), bool)) else raw,
            "coerced_value": coerced,
            "expected_value": expected_val,
            "matches_expected": coerced == expected_val,
            "provenance": "executed_source",
        })

    clamp_cases = [
        ("clamp_none", None, None),
        ("clamp_zero", 0, None),
        ("clamp_negative", -5.0, None),
        ("clamp_normal", 60.0, 60.0),
        ("clamp_overflow", 40_000_000.0, MAX_SAFE_TIMEOUT_S),
        ("clamp_nan", float("nan"), None),
        ("clamp_unparseable", "not_a_number", None),
    ]
    for name, raw, exp in clamp_cases:
        actual = clamp_timeout(raw)
        cases.append({
            "case_name": f"deadline_{name}",
            "raw_input": repr(raw) if isinstance(raw, float) and math.isnan(raw) else raw,
            "coerced_value": actual,
            "expected_value": exp,
            "matches_expected": actual == exp,
            "provenance": "executed_source",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 2: Request Timeout Precedence
# ---------------------------------------------------------------------------
def section_request_timeout_precedence() -> List[Dict[str, Any]]:
    cases = []

    mock_config = {
        "providers": {
            "openrouter": {
                "request_timeout_seconds": 77.0,
                "models": {
                    "openai/gpt-4o-mini": {
                        "timeout_seconds": 42.0,
                    },
                    "anthropic/claude-3-haiku": {
                        "timeout_seconds": -5.0,
                    },
                },
            },
            "custom-local": {
                "request_timeout_seconds": 300.0,
            },
            "unconfigured": {},
        }
    }

    with patch("hermes_cli.config.load_config_readonly", return_value=mock_config):
        v1 = get_provider_request_timeout("openrouter", "openai/gpt-4o-mini")
        cases.append({
            "case_name": "model_override_wins",
            "provider_id": "openrouter",
            "model": "openai/gpt-4o-mini",
            "resolved_timeout": v1,
            "expected_timeout": 42.0,
            "precedence_tier": "model_config",
            "provenance": "executed_source",
        })

        v2 = get_provider_request_timeout("openrouter", "meta-llama/llama-3-70b")
        cases.append({
            "case_name": "provider_fallback_when_model_unconfigured",
            "provider_id": "openrouter",
            "model": "meta-llama/llama-3-70b",
            "resolved_timeout": v2,
            "expected_timeout": 77.0,
            "precedence_tier": "provider_config",
            "provenance": "executed_source",
        })

        v3 = get_provider_request_timeout("openrouter", "anthropic/claude-3-haiku")
        cases.append({
            "case_name": "invalid_model_timeout_falls_back_to_provider",
            "provider_id": "openrouter",
            "model": "anthropic/claude-3-haiku",
            "resolved_timeout": v3,
            "expected_timeout": 77.0,
            "precedence_tier": "provider_config",
            "provenance": "executed_source",
        })

        v4 = get_provider_request_timeout("unconfigured", "any-model")
        cases.append({
            "case_name": "unconfigured_provider_returns_none",
            "provider_id": "unconfigured",
            "model": "any-model",
            "resolved_timeout": v4,
            "expected_timeout": None,
            "precedence_tier": "none",
            "provenance": "executed_source",
        })

        v5 = get_provider_request_timeout("nonexistent", "any-model")
        cases.append({
            "case_name": "nonexistent_provider_returns_none",
            "provider_id": "nonexistent",
            "model": "any-model",
            "resolved_timeout": v5,
            "expected_timeout": None,
            "precedence_tier": "none",
            "provenance": "executed_source",
        })

        v6 = get_provider_request_timeout("", "any-model")
        cases.append({
            "case_name": "empty_provider_returns_none",
            "provider_id": "",
            "model": "any-model",
            "resolved_timeout": v6,
            "expected_timeout": None,
            "precedence_tier": "none",
            "provenance": "executed_source",
        })

    agent = _make_test_agent(model="openai/gpt-4o-mini", provider="openrouter")

    with patch("hermes_cli.config.load_config_readonly", return_value={}):
        with patch.dict(os.environ, {"HERMES_API_TIMEOUT": "999.0"}):
            v_env = agent._resolved_api_call_timeout()
            cases.append({
                "case_name": "env_var_wins_over_default_when_no_config",
                "env_value": "999.0",
                "resolved_timeout": v_env,
                "expected_timeout": 999.0,
                "precedence_tier": "env_fallback",
                "provenance": "executed_source",
            })

        with patch.dict(os.environ, {}, clear=True):
            os.environ.pop("HERMES_API_TIMEOUT", None)
            v_def = agent._resolved_api_call_timeout()
            cases.append({
                "case_name": "builtin_default_1800s_when_unset",
                "resolved_timeout": v_def,
                "expected_timeout": 1800.0,
                "precedence_tier": "builtin_default",
                "provenance": "executed_source",
            })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 3: Stale Timeout Precedence
# ---------------------------------------------------------------------------
def section_stale_timeout_precedence() -> List[Dict[str, Any]]:
    cases = []

    mock_config = {
        "providers": {
            "openai-codex": {
                "stale_timeout_seconds": 120.0,
                "models": {
                    "gpt-5.4": {
                        "stale_timeout_seconds": 1800.0,
                    },
                    "broken-model": {
                        "stale_timeout_seconds": 0.0,
                    },
                },
            },
            "unconfigured": {},
        }
    }

    with patch("hermes_cli.config.load_config_readonly", return_value=mock_config):
        s1 = get_provider_stale_timeout("openai-codex", "gpt-5.4")
        cases.append({
            "case_name": "model_stale_override_wins",
            "provider_id": "openai-codex",
            "model": "gpt-5.4",
            "resolved_stale_timeout": s1,
            "expected_stale_timeout": 1800.0,
            "provenance": "executed_source",
        })

        s2 = get_provider_stale_timeout("openai-codex", "other-model")
        cases.append({
            "case_name": "provider_stale_fallback_when_model_unconfigured",
            "provider_id": "openai-codex",
            "model": "other-model",
            "resolved_stale_timeout": s2,
            "expected_stale_timeout": 120.0,
            "provenance": "executed_source",
        })

        s3 = get_provider_stale_timeout("openai-codex", "broken-model")
        cases.append({
            "case_name": "invalid_model_stale_falls_back_to_provider",
            "provider_id": "openai-codex",
            "model": "broken-model",
            "resolved_stale_timeout": s3,
            "expected_stale_timeout": 120.0,
            "provenance": "executed_source",
        })

        s4 = get_provider_stale_timeout("unconfigured", "any-model")
        cases.append({
            "case_name": "unconfigured_provider_stale_returns_none",
            "provider_id": "unconfigured",
            "model": "any-model",
            "resolved_stale_timeout": s4,
            "expected_stale_timeout": None,
            "provenance": "executed_source",
        })

    agent_reasoning = _make_test_agent(model="deepseek/deepseek-r1", provider="deepseek")
    agent_plain = _make_test_agent(model="gpt-4o", provider="openai")

    with patch("hermes_cli.config.load_config_readonly", return_value={}):
        with patch.dict(os.environ, {"HERMES_API_CALL_STALE_TIMEOUT": "220.0"}):
            base_env, implicit_env = agent_plain._resolved_api_call_stale_timeout_base()
            is_explicit_env = agent_plain._stale_timeout_is_explicit()
            cases.append({
                "case_name": "env_stale_override_wins",
                "resolved_base": base_env,
                "uses_implicit_default": implicit_env,
                "is_explicit": is_explicit_env,
                "expected_base": 220.0,
                "expected_implicit": False,
                "expected_explicit": True,
                "provenance": "executed_source",
            })

        with patch.dict(os.environ, {}, clear=True):
            os.environ.pop("HERMES_API_CALL_STALE_TIMEOUT", None)
            base_rf, implicit_rf = agent_reasoning._resolved_api_call_stale_timeout_base()
            is_explicit_rf = agent_reasoning._stale_timeout_is_explicit()
            cases.append({
                "case_name": "reasoning_floor_applies_with_implicit_false",
                "model": "deepseek/deepseek-r1",
                "resolved_base": base_rf,
                "uses_implicit_default": implicit_rf,
                "is_explicit": is_explicit_rf,
                "expected_base": 600.0,
                "expected_implicit": False,
                "expected_explicit": False,
                "provenance": "executed_source",
            })

            base_def, implicit_def = agent_plain._resolved_api_call_stale_timeout_base()
            is_explicit_def = agent_plain._stale_timeout_is_explicit()
            cases.append({
                "case_name": "plain_model_falls_back_to_90s_default",
                "model": "gpt-4o",
                "resolved_base": base_def,
                "uses_implicit_default": implicit_def,
                "is_explicit": is_explicit_def,
                "expected_base": 90.0,
                "expected_implicit": True,
                "expected_explicit": False,
                "provenance": "executed_source",
            })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 4: Reasoning-Model Floors
# ---------------------------------------------------------------------------
def section_reasoning_model_floors() -> List[Dict[str, Any]]:
    test_models = [
        ("nvidia/nemotron-3-ultra-550b-a55b", 600.0),
        ("nvidia/nemotron-3-super-120b-a12b", 600.0),
        ("nvidia/nemotron-3-nano-30b-a3b", 300.0),
        ("nvidia/nemotron-3.5-lightning-30b-a3b", 300.0),
        ("deepseek/deepseek-r1", 600.0),
        ("deepseek/deepseek-r1-distill-llama-70b", 600.0),
        ("deepseek/deepseek-reasoner", 600.0),
        ("deepseek/deepseek-v4-flash", 600.0),
        ("deepseek/deepseek-v4-pro", 600.0),
        ("deepseek-v4-flash-free", 600.0),
        ("qwen/qwq-32b-preview", 300.0),
        ("qwen/qwen3-235b-a22b-thinking", 180.0),
        ("qwen3", 180.0),
        ("openai/o1", 600.0),
        ("openai/o1-mini", 600.0),
        ("openai/o1-pro", 600.0),
        ("openai/o1-preview", 600.0),
        ("openai/o3", 600.0),
        ("openai/o3-pro", 600.0),
        ("openai/o3-mini", 300.0),
        ("openai/o4-mini", 300.0),
        ("anthropic/claude-opus-4-6", 240.0),
        ("anthropic/claude-opus-5", 240.0),
        ("anthropic/claude-sonnet-5", 180.0),
        ("anthropic/claude-sonnet-4.5", 180.0),
        ("anthropic/claude-sonnet-4.6", 180.0),
        ("anthropic/claude-fable-5", 600.0),
        ("claude-fable", 600.0),
        ("x-ai/grok-4-fast-reasoning", 300.0),
        ("x-ai/grok-4.20-reasoning", 300.0),
        ("x-ai/grok-4.5", 300.0),
        ("x-ai/grok-4.6", 300.0),
        ("x-ai/grok-4-fast-non-reasoning", 180.0),
        ("stealth/ox-alpha", 300.0),
        ("x-preview-f-free", 300.0),
        ("thinkingmachines/inkling", 300.0),
        ("thinkingmachines/inkling:free", 300.0),
        ("thinkingmachines/inkling-small:free", 300.0),
        ("gpt-4o", None),
        ("gpt-4o-mini", None),
        ("claude-3-5-sonnet", None),
        ("claude-3-opus", None),
        ("olmo-1", None),
        ("llama-3.3-70b-instruct", None),
        ("llama-4-70b-o1-preview", None),
        ("some-other-qwen3", None),
        ("bare-grok-3", None),
        ("", None),
        (None, None),
    ]

    cases = []
    for model_name, expected_floor in test_models:
        floor = get_reasoning_stale_timeout_floor(model_name)
        cases.append({
            "case_name": f"floor_{model_name or 'none'}",
            "model_input": model_name,
            "matched_floor": floor,
            "expected_floor": expected_floor,
            "matches_expected": floor == expected_floor,
            "provenance": "executed_source",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 5: Context-Size Token Estimation
# ---------------------------------------------------------------------------
def section_context_token_estimation() -> List[Dict[str, Any]]:
    cases = []

    cases.append({
        "case_name": "empty_none_payload",
        "input_type": "None",
        "estimated_tokens": estimate_request_context_tokens(None),
        "expected_tokens": 0,
        "provenance": "executed_source",
    })
    cases.append({
        "case_name": "empty_list_payload",
        "input_type": "list",
        "estimated_tokens": estimate_request_context_tokens([]),
        "expected_tokens": 0,
        "provenance": "executed_source",
    })
    cases.append({
        "case_name": "empty_dict_payload",
        "input_type": "dict",
        "estimated_tokens": estimate_request_context_tokens({}),
        "expected_tokens": 0,
        "provenance": "executed_source",
    })

    bare_messages = [
        {"role": "user", "content": "a" * 400},
        {"role": "assistant", "content": "b" * 800},
    ]
    est_bare = estimate_request_context_tokens(bare_messages)
    cases.append({
        "case_name": "bare_messages_list",
        "input_type": "list",
        "item_count": 2,
        "estimated_tokens": est_bare,
        "provenance": "executed_source",
    })

    cc_payload = {
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "x" * 2000}],
        "tools": [{"type": "function", "function": {"name": "test_fn", "description": "d" * 400}}],
    }
    est_cc = estimate_request_context_tokens(cc_payload)
    cases.append({
        "case_name": "chat_completions_dict_with_tools",
        "input_type": "dict_messages_tools",
        "estimated_tokens": est_cc,
        "provenance": "executed_source",
    })

    responses_payload = {
        "model": "gpt-5.5",
        "instructions": "i" * 1000,
        "input": "u" * 4000,
        "tools": [{"name": "t1", "description": "tool desc"}],
    }
    est_resp = estimate_request_context_tokens(responses_payload)
    cases.append({
        "case_name": "responses_api_dict_input_instructions",
        "input_type": "dict_input_instructions",
        "estimated_tokens": est_resp,
        "provenance": "executed_source",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 6: Context-Size Scaling
# ---------------------------------------------------------------------------
def section_context_size_scaling() -> List[Dict[str, Any]]:
    cases = []

    def compute_stream_stale_timeout(base: float, token_count: int, model: Optional[str] = None) -> float:
        if token_count > 100_000:
            timeout = max(base, 300.0)
        elif token_count > 50_000:
            timeout = max(base, 240.0)
        else:
            timeout = base
        reasoning_floor = get_reasoning_stale_timeout_floor(model) if model else None
        if reasoning_floor is not None:
            timeout = max(timeout, reasoning_floor)
        return timeout

    streaming_scenarios = [
        ("stream_default_small_ctx", 180.0, 10_000, None, 180.0),
        ("stream_default_medium_ctx", 180.0, 60_000, None, 240.0),
        ("stream_default_large_ctx", 180.0, 120_000, None, 300.0),
        ("stream_high_base_small_ctx", 400.0, 10_000, None, 400.0),
        ("stream_high_base_large_ctx", 400.0, 120_000, None, 400.0),
        ("stream_o1_small_ctx", 180.0, 10_000, "o1", 600.0),
        ("stream_o1_large_ctx", 180.0, 120_000, "o1", 600.0),
        ("stream_opus4_small_ctx", 180.0, 10_000, "claude-opus-4", 240.0),
        ("stream_opus4_medium_ctx", 180.0, 60_000, "claude-opus-4", 240.0),
        ("stream_opus4_large_ctx", 180.0, 120_000, "claude-opus-4", 300.0),
    ]

    for name, base, tokens, model, expected in streaming_scenarios:
        calc = compute_stream_stale_timeout(base, tokens, model)
        cases.append({
            "case_name": name,
            "mode": "streaming",
            "base_timeout": base,
            "token_count": tokens,
            "model": model,
            "computed_timeout": calc,
            "expected_timeout": expected,
            "matches_expected": calc == expected,
            "provenance": "executed_source",
        })

    agent_plain = _make_test_agent(model="gpt-4o", provider="openai")

    small_payload = {"messages": [{"role": "user", "content": "x" * 4000}]}
    t_small = agent_plain._compute_non_stream_stale_timeout(small_payload)
    cases.append({
        "case_name": "non_stream_small_ctx",
        "mode": "non_streaming",
        "estimated_tokens": estimate_request_context_tokens(small_payload),
        "computed_timeout": t_small,
        "expected_timeout": 90.0,
        "provenance": "executed_source",
    })

    med_payload = {"messages": [{"role": "user", "content": "m" * 240_000}]}
    t_med = agent_plain._compute_non_stream_stale_timeout(med_payload)
    cases.append({
        "case_name": "non_stream_medium_ctx",
        "mode": "non_streaming",
        "estimated_tokens": estimate_request_context_tokens(med_payload),
        "computed_timeout": t_med,
        "expected_timeout": 150.0,
        "provenance": "executed_source",
    })

    large_payload = {"messages": [{"role": "user", "content": "L" * 440_000}]}
    t_large = agent_plain._compute_non_stream_stale_timeout(large_payload)
    cases.append({
        "case_name": "non_stream_large_ctx",
        "mode": "non_streaming",
        "estimated_tokens": estimate_request_context_tokens(large_payload),
        "computed_timeout": t_large,
        "expected_timeout": 240.0,
        "provenance": "executed_source",
    })

    agent_budget = _make_test_agent(model="deepseek/deepseek-r1", provider="deepseek")
    agent_budget.run_budget_seconds = 300.0
    agent_budget._run_budget_started_at = time.time() - 100.0
    t_budget = agent_budget._compute_non_stream_stale_timeout(small_payload)
    cases.append({
        "case_name": "non_stream_run_budget_cap_caps_implicit_floor",
        "mode": "non_streaming",
        "model": "deepseek/deepseek-r1",
        "run_budget_seconds": 300.0,
        "remaining_budget": 200.0,
        "computed_timeout": round(t_budget, 1),
        "expected_timeout": 100.0,
        "provenance": "executed_source",
    })

    with patch("hermes_cli.config.load_config_readonly", return_value={
        "providers": {"deepseek": {"stale_timeout_seconds": 600.0}}
    }):
        t_budget_explicit = agent_budget._compute_non_stream_stale_timeout(small_payload)
        cases.append({
            "case_name": "non_stream_run_budget_cap_spares_explicit_config",
            "mode": "non_streaming",
            "model": "deepseek/deepseek-r1",
            "explicit_config": 600.0,
            "computed_timeout": t_budget_explicit,
            "expected_timeout": 600.0,
            "provenance": "executed_source",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 7: Local-Endpoint Detection and Timeout Scaling
# ---------------------------------------------------------------------------
def section_local_endpoint_scaling() -> List[Dict[str, Any]]:
    cases = []

    endpoints = [
        ("http://localhost:11434", True, "loopback_hostname"),
        ("http://127.0.0.1:11434", True, "loopback_ipv4"),
        ("http://127.0.1.1:8000", True, "loopback_ipv4_subnet"),
        ("http://[::1]:11434", True, "loopback_ipv6"),
        ("http://host.docker.internal:11434", True, "docker_internal"),
        ("http://host.lima.internal:8000", True, "lima_internal"),
        ("http://ollama:11434", True, "unqualified_service_name"),
        ("http://vllm-server:8000", True, "unqualified_service_name"),
        ("http://10.0.0.5:8000", True, "rfc1918_10"),
        ("http://172.20.0.2:11434", True, "rfc1918_172"),
        ("http://192.168.1.100:11434", True, "rfc1918_192"),
        ("http://100.64.0.1:11434", True, "tailscale_cgnat"),
        ("http://100.120.45.10:11434", True, "tailscale_cgnat"),
        ("https://api.openai.com/v1", False, "public_openai"),
        ("https://openrouter.ai/api/v1", False, "public_openrouter"),
        ("https://api.anthropic.com/v1", False, "public_anthropic"),
        ("http://8.8.8.8:8080", False, "public_ipv4"),
        ("http://1.1.1.1:80", False, "public_ipv4"),
        ("", False, "empty_base_url"),
    ]

    for url, expected_local, category in endpoints:
        is_local = is_local_endpoint(url)
        cases.append({
            "case_name": f"endpoint_{category}_{url.replace('://', '_').replace('/', '_') or 'empty'}",
            "base_url": url,
            "category": category,
            "is_local": is_local,
            "expected_local": expected_local,
            "matches_expected": is_local == expected_local,
            "provenance": "executed_source",
        })

    agent_local = _make_test_agent(
        model="llama3.3:70b",
        provider="ollama",
        base_url="http://localhost:11434",
    )

    with patch("hermes_cli.config.load_config_readonly", return_value={}):
        with patch.dict(os.environ, {}, clear=True):
            os.environ.pop("HERMES_LOCAL_STREAM_STALE_TIMEOUT", None)
            os.environ.pop("HERMES_STREAM_STALE_TIMEOUT", None)
            base_timeout = 180.0
            if base_timeout == 180.0 and agent_local.base_url and is_local_endpoint(agent_local.base_url):
                stream_stale = 900.0
            else:
                stream_stale = base_timeout
            cases.append({
                "case_name": "stream_local_default_base_escalates_to_900s",
                "base_url": agent_local.base_url,
                "base_timeout": base_timeout,
                "stream_stale_timeout": stream_stale,
                "expected_stale_timeout": 900.0,
                "provenance": "executed_source",
            })

    custom_base = 60.0
    if custom_base == 180.0 and agent_local.base_url and is_local_endpoint(agent_local.base_url):
        stream_stale_custom = 900.0
    else:
        stream_stale_custom = custom_base
    cases.append({
        "case_name": "stream_local_custom_base_not_escalated",
        "base_url": agent_local.base_url,
        "base_timeout": custom_base,
        "stream_stale_timeout": stream_stale_custom,
        "expected_stale_timeout": 60.0,
        "provenance": "executed_source",
    })

    with patch("hermes_cli.config.load_config_readonly", return_value={}):
        with patch.dict(os.environ, {}, clear=True):
            os.environ.pop("HERMES_API_CALL_STALE_TIMEOUT", None)
            t_nonstream_local = agent_local._compute_non_stream_stale_timeout({})
            cases.append({
                "case_name": "non_stream_local_implicit_default_returns_inf",
                "base_url": agent_local.base_url,
                "computed_timeout": "inf" if math.isinf(t_nonstream_local) else t_nonstream_local,
                "expected_timeout": "inf",
                "provenance": "executed_source",
            })

    agent_local_reasoning = _make_test_agent(
        model="deepseek/deepseek-r1",
        provider="ollama",
        base_url="http://localhost:11434",
    )
    with patch("hermes_cli.config.load_config_readonly", return_value={}):
        with patch.dict(os.environ, {}, clear=True):
            os.environ.pop("HERMES_API_CALL_STALE_TIMEOUT", None)
            t_nonstream_reasoning_local = agent_local_reasoning._compute_non_stream_stale_timeout({})
            cases.append({
                "case_name": "non_stream_local_reasoning_model_preserves_floor",
                "base_url": agent_local_reasoning.base_url,
                "model": "deepseek/deepseek-r1",
                "computed_timeout": t_nonstream_reasoning_local,
                "expected_timeout": 600.0,
                "is_infinite": math.isinf(t_nonstream_reasoning_local),
                "expected_infinite": False,
                "provenance": "executed_source",
            })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 8: Streaming Socket Read Timeout and Connect Bounds
# ---------------------------------------------------------------------------
def section_socket_read_and_connect_bounds() -> List[Dict[str, Any]]:
    cases = []

    def resolve_stream_socket_timeouts(
        provider_cfg_timeout: Optional[float],
        env_read_timeout: Optional[float],
        base_url: str,
        stream_stale_timeout: Optional[float],
    ) -> Dict[str, float]:
        base_timeout = provider_cfg_timeout if provider_cfg_timeout is not None else 1800.0
        if provider_cfg_timeout is not None:
            stream_read_timeout = provider_cfg_timeout
        else:
            stream_read = env_read_timeout if env_read_timeout is not None else 120.0
            if stream_read == 120.0 and base_url and is_local_endpoint(base_url):
                stream_read_timeout = base_timeout
            elif (
                stream_read == 120.0
                and stream_stale_timeout is not None
                and stream_stale_timeout != float("inf")
                and stream_stale_timeout > stream_read
            ):
                stream_read_timeout = stream_stale_timeout
            else:
                stream_read_timeout = stream_read

        conn_cap = min(base_timeout, 60.0) if provider_cfg_timeout is not None else 30.0
        return {
            "connect": conn_cap,
            "read": stream_read_timeout,
            "write": base_timeout,
            "pool": conn_cap,
        }

    scenarios = [
        ("cloud_standard_default", None, None, "https://api.openai.com/v1", 180.0, 30.0, 180.0, 1800.0, 30.0),
        ("cloud_reasoning_opus_240s", None, None, "https://api.anthropic.com/v1", 240.0, 30.0, 240.0, 1800.0, 30.0),
        ("cloud_reasoning_o1_600s", None, None, "https://api.openai.com/v1", 600.0, 30.0, 600.0, 1800.0, 30.0),
        ("local_endpoint_raises_read_to_base", None, None, "http://localhost:11434", 900.0, 30.0, 1800.0, 1800.0, 30.0),
        ("user_env_read_override", None, 45.0, "https://api.openai.com/v1", 180.0, 30.0, 45.0, 1800.0, 30.0),
        ("user_env_read_override_local", None, 45.0, "http://localhost:11434", 900.0, 30.0, 45.0, 1800.0, 30.0),
        ("provider_config_override_short", 25.0, None, "https://api.openai.com/v1", 180.0, 25.0, 25.0, 25.0, 25.0),
        ("provider_config_override_long", 120.0, None, "https://api.openai.com/v1", 180.0, 60.0, 120.0, 120.0, 60.0),
    ]

    for name, p_cfg, env_read, base_url, stale_t, exp_conn, exp_read, exp_write, exp_pool in scenarios:
        res = resolve_stream_socket_timeouts(p_cfg, env_read, base_url, stale_t)
        cases.append({
            "case_name": name,
            "provider_cfg": p_cfg,
            "env_read_timeout": env_read,
            "base_url": base_url,
            "stream_stale_timeout": stale_t,
            "resolved_timeouts": res,
            "expected_timeouts": {
                "connect": exp_conn,
                "read": exp_read,
                "write": exp_write,
                "pool": exp_pool,
            },
            "matches_expected": (
                res["connect"] == exp_conn
                and res["read"] == exp_read
                and res["write"] == exp_write
                and res["pool"] == exp_pool
            ),
            "provenance": "executed_source",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 9: First-Byte vs Inter-Chunk Stale Detection & Operator Visibility
# ---------------------------------------------------------------------------
def section_stale_detection_and_operator_visibility() -> List[Dict[str, Any]]:
    cases = []

    cases.append({
        "case_name": "first_byte_stale_trigger",
        "gap_type": "time_to_first_byte",
        "stale_timeout": 180.0,
        "elapsed_silence": 185.0,
        "trips_stale_detector": True,
        "buffer_status_template": "⚠️ No response from provider for {elapsed}s (model: {model}, context: ~{tokens} tokens). Reconnecting...",
        "wait_notice_template": "⚠ no output from provider for {elapsed}s -- reconnecting...",
        "provenance": "source_inspection",
    })

    cases.append({
        "case_name": "inter_chunk_stale_trigger",
        "gap_type": "inter_chunk_silence",
        "stale_timeout": 180.0,
        "elapsed_silence": 181.0,
        "trips_stale_detector": True,
        "action_taken": "socket_abort_and_streak_bump",
        "provenance": "source_inspection",
    })

    cases.append({
        "case_name": "heartbeat_notice_threshold",
        "interval_seconds": 30.0,
        "waiting_seconds": 35,
        "emits_wait_notice": True,
        "notice_format": "⏳ waiting on {model} -- {waiting_secs}s with no output yet (provider may be slow or overloaded, or the model is thinking{recovery})",
        "recovery_subformat": "; auto-reconnect at {timeout}s",
        "provenance": "source_inspection",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 10: Pre-Visible vs Post-Visible Stale Effects & Stream Boundaries
# ---------------------------------------------------------------------------
def section_pre_vs_post_visible_effects() -> List[Dict[str, Any]]:
    cases = []

    cases.append({
        "case_name": "pre_visible_stale_kill_replay_allowed",
        "deltas_were_sent": False,
        "tool_in_flight": False,
        "inner_retry_allowed": True,
        "max_inner_attempts": 3,
        "surfaces_duplicate_text": False,
        "fallback_route": "eager_fallback_after_attempt_3",
        "provenance": "source_inspection",
    })

    cases.append({
        "case_name": "post_visible_text_stream_fails_closed",
        "deltas_were_sent": True,
        "tool_in_flight": False,
        "inner_retry_allowed": False,
        "outcome": "partial_stream_stub",
        "stub_id": PARTIAL_STREAM_STUB_ID,
        "stub_finish_reason": FINISH_REASON_LENGTH,
        "resets_stale_streak": True,
        "surfaces_duplicate_text": False,
        "provenance": "source_inspection",
    })

    cases.append({
        "case_name": "post_visible_mid_tool_call_retries_silently",
        "deltas_were_sent": True,
        "tool_in_flight": True,
        "is_transient_error": True,
        "inner_retry_allowed": True,
        "reconnect_marker": "\n\n⚠ Connection dropped mid tool-call; reconnecting…\n\n",
        "resets_delivery_tracking": True,
        "resets_accumulators": True,
        "provenance": "source_inspection",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 11: Stale Streak Lifecycle & Circuit Breaker
# ---------------------------------------------------------------------------
def section_stale_streak_circuit_breaker() -> List[Dict[str, Any]]:
    cases = []

    agent = _make_test_agent()

    cases.append({
        "case_name": "streak_initial_is_zero",
        "streak": _stale_streak(agent),
        "expected_streak": 0,
        "provenance": "executed_source",
    })

    _bump_stale_streak(agent)
    _bump_stale_streak(agent)
    cases.append({
        "case_name": "streak_after_two_bumps",
        "streak": _stale_streak(agent),
        "expected_streak": 2,
        "provenance": "executed_source",
    })

    bumped_1 = _record_interrupted_provider_wait(agent, 35.0, response_started=False)
    cases.append({
        "case_name": "interrupted_pre_response_over_30s_bumps_streak",
        "elapsed": 35.0,
        "response_started": False,
        "did_bump": bumped_1,
        "new_streak": _stale_streak(agent),
        "expected_streak": 3,
        "provenance": "executed_source",
    })

    bumped_2 = _record_interrupted_provider_wait(agent, 45.0, response_started=True)
    cases.append({
        "case_name": "interrupted_post_response_does_not_bump_streak",
        "elapsed": 45.0,
        "response_started": True,
        "did_bump": bumped_2,
        "new_streak": _stale_streak(agent),
        "expected_streak": 3,
        "provenance": "executed_source",
    })

    bumped_3 = _record_interrupted_provider_wait(agent, 10.0, response_started=False)
    cases.append({
        "case_name": "interrupted_under_30s_does_not_bump_streak",
        "elapsed": 10.0,
        "response_started": False,
        "did_bump": bumped_3,
        "new_streak": _stale_streak(agent),
        "expected_streak": 3,
        "provenance": "executed_source",
    })

    _reset_stale_streak(agent)
    cases.append({
        "case_name": "streak_after_reset",
        "streak": _stale_streak(agent),
        "expected_streak": 0,
        "provenance": "executed_source",
    })

    with patch.dict(os.environ, {"HERMES_STREAM_STALE_GIVEUP": "3"}):
        agent._consecutive_stale_streams = 2
        _check_stale_giveup(agent)
        cases.append({
            "case_name": "check_stale_giveup_under_threshold_passes",
            "streak": 2,
            "giveup_threshold": 3,
            "did_raise": False,
            "provenance": "executed_source",
        })

        agent._consecutive_stale_streams = 3
        did_raise = False
        error_msg = ""
        try:
            _check_stale_giveup(agent)
        except RuntimeError as exc:
            did_raise = True
            error_msg = str(exc)

        cases.append({
            "case_name": "check_stale_giveup_at_threshold_raises",
            "streak": 3,
            "giveup_threshold": 3,
            "did_raise": did_raise,
            "error_contains_unresponsive": "unresponsive" in error_msg,
            "error_contains_streak": "3 consecutive stale attempts" in error_msg,
            "provenance": "executed_source",
        })

    with patch.dict(os.environ, {"HERMES_STREAM_STALE_GIVEUP": "0"}):
        agent._consecutive_stale_streams = 10
        _check_stale_giveup(agent)
        cases.append({
            "case_name": "check_stale_giveup_disabled_when_zero",
            "streak": 10,
            "giveup_threshold": 0,
            "did_raise": False,
            "provenance": "executed_source",
        })

    agent._consecutive_stale_streams = 5
    agent._fallback_chain = [{"provider": "openai", "model": "gpt-4o"}]
    agent._fallback_index = 0
    with patch("agent.auxiliary_client.resolve_provider_client", return_value=(MagicMock(), "resolved")):
        activated = agent._try_activate_fallback()
        cases.append({
            "case_name": "fallback_activation_resets_streak",
            "activated": activated,
            "streak_after_fallback": _stale_streak(agent),
            "expected_streak": 0,
            "provenance": "executed_source",
        })

    agent._consecutive_stale_streams = 4
    agent._primary_runtime = {
        "model": "gpt-4o",
        "provider": "openai",
        "requested_provider": "openai",
        "base_url": "https://api.openai.com/v1",
        "api_mode": "chat_completions",
        "api_key": "test",
        "client_kwargs": {},
        "use_prompt_caching": False,
    }
    with patch("agent.chat_completion_helpers.rewrite_prompt_model_identity"):
        restore_primary_runtime(agent)
        cases.append({
            "case_name": "restore_primary_runtime_resets_streak",
            "streak_after_restore": _stale_streak(agent),
            "expected_streak": 0,
            "provenance": "executed_source",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 12: Buffered Timeout Behavior & Direct Watchdogs
# ---------------------------------------------------------------------------
def section_buffered_timeout_behavior() -> List[Dict[str, Any]]:
    cases = []

    t_finite = _inline_nonstream_hard_timeout(90.0)
    cases.append({
        "case_name": "inline_hard_timeout_finite_budget",
        "input_budget": 90.0,
        "is_httpx_timeout": isinstance(t_finite, httpx.Timeout),
        "connect": t_finite.connect,
        "read": t_finite.read,
        "write": t_finite.write,
        "pool": t_finite.pool,
        "expected_connect": 60.0,
        "expected_read": 90.0,
        "provenance": "executed_source",
    })

    t_inf = _inline_nonstream_hard_timeout(float("inf"))
    cases.append({
        "case_name": "inline_hard_timeout_infinite_budget_returns_none",
        "input_budget": "inf",
        "returned_value": t_inf,
        "expected_value": None,
        "provenance": "executed_source",
    })

    t_non_pos = _inline_nonstream_hard_timeout(0.0)
    cases.append({
        "case_name": "inline_hard_timeout_zero_budget_returns_none",
        "input_budget": 0.0,
        "returned_value": t_non_pos,
        "expected_value": None,
        "provenance": "executed_source",
    })

    agent = _make_test_agent()
    direct_stale = _resolve_direct_stale_timeout(agent, {"messages": []})
    cases.append({
        "case_name": "resolve_direct_stale_timeout_matches_agent_resolver",
        "resolved_budget": direct_stale,
        "expected_budget": 90.0,
        "provenance": "executed_source",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 13: Provider-Specific Boundaries and Exclusions
# ---------------------------------------------------------------------------
def section_provider_exclusions_and_boundaries() -> List[Dict[str, Any]]:
    cases = []

    agent_bedrock = _make_test_agent(model="anthropic.claude-opus-4-6-v1:0", provider="bedrock")

    with patch("hermes_cli.config.load_config_readonly", return_value={}):
        with patch.dict(os.environ, {}, clear=True):
            os.environ.pop("HERMES_STREAM_STALE_TIMEOUT", None)
            bedrock_stale = _derive_stream_stale_timeout(
                agent_bedrock,
                {"modelId": "us.anthropic.claude-opus-4-6-v1:0", "messages": []}
            )
            cases.append({
                "case_name": "bedrock_stream_stale_resolves_reasoning_floor",
                "model_id": "us.anthropic.claude-opus-4-6-v1:0",
                "resolved_timeout": bedrock_stale,
                "expected_timeout": 240.0,
                "provenance": "executed_source",
            })

    boundaries = [
        {
            "provider": "bedrock",
            "wired_for_provider_config_timeouts": False,
            "exclusion_reason": "boto3 manages its own client-level and botocore timeouts",
            "streaming_watchdog_supported": True,
            "circuit_breaker_shared": True,
        },
        {
            "provider": "openai-codex",
            "wired_for_provider_config_timeouts": True,
            "has_ttfb_watchdog": True,
            "has_event_idle_watchdog": True,
            "has_gateway_stale_floor": True,
            "has_absolute_hard_ceiling": True,
            "circuit_breaker_shared": True,
        },
        {
            "provider": "anthropic_messages",
            "wired_for_provider_config_timeouts": True,
            "default_client_read_timeout": 900.0,
            "default_client_connect_timeout": 10.0,
            "uses_request_local_client_for_abort": True,
            "circuit_breaker_shared": True,
        },
        {
            "provider": "openai_chat_completions",
            "wired_for_provider_config_timeouts": True,
            "default_client_read_timeout": None,
            "default_client_connect_timeout": 15.0,
            "uses_request_local_client_for_abort": True,
            "circuit_breaker_shared": True,
        },
    ]

    for b in boundaries:
        cases.append({
            "case_name": f"boundary_{b['provider']}",
            **b,
            "provenance": "source_inspection",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Oracle Corpus Builder
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, List[Dict[str, Any]]]:
    return {
        "timeout_coercion_and_normalization": section_timeout_coercion_and_normalization(),
        "request_timeout_precedence": section_request_timeout_precedence(),
        "stale_timeout_precedence": section_stale_timeout_precedence(),
        "reasoning_model_floors": section_reasoning_model_floors(),
        "context_token_estimation": section_context_token_estimation(),
        "context_size_scaling": section_context_size_scaling(),
        "local_endpoint_scaling": section_local_endpoint_scaling(),
        "socket_read_and_connect_bounds": section_socket_read_and_connect_bounds(),
        "stale_detection_and_operator_visibility": section_stale_detection_and_operator_visibility(),
        "pre_vs_post_visible_effects": section_pre_vs_post_visible_effects(),
        "stale_streak_circuit_breaker": section_stale_streak_circuit_breaker(),
        "buffered_timeout_behavior": section_buffered_timeout_behavior(),
        "provider_exclusions_and_boundaries": section_provider_exclusions_and_boundaries(),
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
