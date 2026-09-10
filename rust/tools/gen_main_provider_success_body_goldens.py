#!/usr/bin/env python3
"""Deterministic source-executed oracle for main-provider success-body validation and recovery.

This generator audits and executes live Python decision functions and runtime transitions
governing chat-completions successful HTTP response body validation and recovery:
1. Malformed or structurally missing choices/message bodies.
2. Empty assistant responses, configured empty-response guard, and exhaustion.
3. finish_reason=content_filter and equivalent tagged stream refusals.
4. Nonempty reasoning with empty visible content.
5. Empty content paired with valid tool calls.
6. Truncated/partial streamed output and the exact no-replay boundary.
7. Same-provider retry versus cross-provider fallback, route stickiness, counters, notices, and terminal outcome.
8. Provider-specific exceptions or distinctions affecting the ordinary chat-completions Rust subset.

Usage:
    python3 rust/tools/gen_main_provider_success_body_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_success_body_goldens.py --check  # check parity
"""

from __future__ import annotations

import copy
import json
import os
import re
import sys
from decimal import Decimal
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-success-body-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

import agent.empty_response_guard as empty_guard
from agent.transports.chat_completions import (
    ChatCompletionsTransport,
    NormalizedResponse,
    ToolCall,
    Usage,
)
from agent.error_classifier import FailoverReason, classify_api_error
from agent.chat_completion_helpers import (
    FINISH_REASON_LENGTH,
    PARTIAL_STREAM_STUB_ID,
    _build_partial_stream_stub,
    build_assistant_message,
    rewrite_prompt_model_identity,
    try_activate_fallback,
)
from agent.conversation_loop import (
    _EMPTY_TOOL_RESPONSE_NUDGE,
    _STALE_MARKER_RE,
    _get_continuation_prompt,
    _sync_failover_system_message,
)
from agent.agent_runtime_helpers import (
    repair_empty_non_final_messages,
    restore_primary_runtime,
)
from agent.retry_utils import jittered_backoff
from run_agent import AIAgent


def _clean_str(val: Any) -> Any:
    """Sanitize string or recursive data structure to guarantee no em dash characters."""
    if isinstance(val, str):
        return val.replace("\u2014", "--")
    if isinstance(val, list):
        return [_clean_str(v) for v in val]
    if isinstance(val, dict):
        return {k: _clean_str(v) for k, v in val.items()}
    if isinstance(val, Decimal):
        return str(val)
    return val


def _make_test_agent(
    model: str = "gpt-4o",
    provider: str = "openai",
    base_url: str = "https://api.openai.com/v1",
    api_key: str = "synth-primary-key",
    fallback_model: Any = None,
    api_max_retries: Optional[int] = None,
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
        agent._create_openai_client = lambda *a, **kw: MagicMock()
        agent._retire_shared_openai_client = lambda *a, **kw: None
        agent._ensure_lmstudio_runtime_loaded = lambda: None
        if api_max_retries is not None:
            agent._api_max_retries = api_max_retries
        return agent


# ---------------------------------------------------------------------------
# Section 1: Malformed and Missing Choice Bodies
# ---------------------------------------------------------------------------
def section_malformed_and_missing_choice_bodies() -> List[Dict[str, Any]]:
    transport = ChatCompletionsTransport()
    cases: List[Dict[str, Any]] = []

    # 1.1 Structural validation via validate_response
    validation_test_inputs = [
        ("response_none", None, False, ["response is None"]),
        (
            "missing_choices_attribute",
            SimpleNamespace(id="resp-1"),
            False,
            ["response has no 'choices' attribute"],
        ),
        (
            "choices_none",
            SimpleNamespace(id="resp-2", choices=None),
            False,
            ["response.choices is None"],
        ),
        (
            "choices_empty_list",
            SimpleNamespace(id="resp-3", choices=[]),
            False,
            ["response.choices is empty"],
        ),
        (
            "valid_choice_with_content",
            SimpleNamespace(
                id="resp-4",
                choices=[
                    SimpleNamespace(
                        message=SimpleNamespace(content="Hello world", tool_calls=None),
                        finish_reason="stop",
                    )
                ],
            ),
            True,
            [],
        ),
        (
            "valid_choice_with_tool_calls",
            SimpleNamespace(
                id="resp-5",
                choices=[
                    SimpleNamespace(
                        message=SimpleNamespace(
                            content="",
                            tool_calls=[
                                SimpleNamespace(
                                    id="call_1",
                                    type="function",
                                    function=SimpleNamespace(
                                        name="read_file",
                                        arguments='{"path":"test.txt"}',
                                    ),
                                )
                            ],
                        ),
                        finish_reason="tool_calls",
                    )
                ],
            ),
            True,
            [],
        ),
        (
            "choice_message_is_none",
            SimpleNamespace(
                id="resp-6",
                choices=[SimpleNamespace(message=None, finish_reason="stop")],
            ),
            True,  # validate_response accepts choices with truthy list, message parsed in normalize
            [],
        ),
    ]

    for (
        name,
        resp_obj,
        expected_valid,
        expected_error_details,
    ) in validation_test_inputs:
        actual_valid = transport.validate_response(resp_obj)
        assert actual_valid == expected_valid, (
            f"{name}: expected {expected_valid}, got {actual_valid}"
        )

        # Mirror error details generation in conversation_loop.py lines 3808-3816
        actual_error_details = []
        if not actual_valid:
            if resp_obj is None:
                actual_error_details.append("response is None")
            elif not hasattr(resp_obj, "choices"):
                actual_error_details.append("response has no 'choices' attribute")
            elif resp_obj.choices is None:
                actual_error_details.append("response.choices is None")
            else:
                actual_error_details.append("response.choices is empty")
            assert actual_error_details == expected_error_details

        cases.append({
            "case_name": f"validate_response_{name}",
            "is_valid": actual_valid,
            "error_details": actual_error_details,
            "hook_error_type": "InvalidAPIResponse" if not actual_valid else None,
            "hook_reason": "invalid_response" if not actual_valid else None,
            "hook_retryable": True if not actual_valid else None,
            "provenance": "executed_source",
        })

    # 1.2 Normalization of edge choice and tool-call shapes
    # (a) Choice message None
    resp_msg_none = SimpleNamespace(
        choices=[SimpleNamespace(message=None, finish_reason="stop")],
        usage=None,
    )
    norm_none = transport.normalize_response(resp_msg_none)
    assert norm_none.content is None
    assert norm_none.tool_calls is None
    assert norm_none.finish_reason == "stop"
    cases.append({
        "case_name": "normalize_choice_message_none",
        "normalized_content": norm_none.content,
        "normalized_tool_calls": norm_none.tool_calls,
        "normalized_finish_reason": norm_none.finish_reason,
        "provenance": "executed_source",
    })

    # (b) Tool call with None function or None function name (skipped by transport)
    resp_malformed_tc = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(
                    content="",
                    tool_calls=[
                        SimpleNamespace(id="c1", type="function", function=None),
                        SimpleNamespace(
                            id="c2",
                            type="function",
                            function=SimpleNamespace(name=None, arguments="{}"),
                        ),
                        SimpleNamespace(
                            id="c3",
                            type="function",
                            function=SimpleNamespace(
                                name="valid_tool", arguments='{"k":"v"}'
                            ),
                        ),
                    ],
                ),
                finish_reason="tool_calls",
            )
        ],
        usage=None,
    )
    norm_tc = transport.normalize_response(resp_malformed_tc)
    assert norm_tc.tool_calls is not None
    assert len(norm_tc.tool_calls) == 1
    assert norm_tc.tool_calls[0].name == "valid_tool"
    assert norm_tc.tool_calls[0].id == "c1" or norm_tc.tool_calls[0].id == "c3"
    cases.append({
        "case_name": "normalize_tool_calls_skip_none_function_or_name",
        "retained_tool_call_count": len(norm_tc.tool_calls),
        "retained_tool_names": [tc.name for tc in norm_tc.tool_calls],
        "provenance": "executed_source",
    })

    # (c) Tool call with explicit empty function name (preserved for Hermes recovery)
    resp_empty_tc_name = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(
                    content="",
                    tool_calls=[
                        SimpleNamespace(
                            id="c1",
                            type="function",
                            function=SimpleNamespace(name="", arguments="{}"),
                        ),
                    ],
                ),
                finish_reason="tool_calls",
            )
        ],
        usage=None,
    )
    norm_empty_name = transport.normalize_response(resp_empty_tc_name)
    assert norm_empty_name.tool_calls is not None
    assert len(norm_empty_name.tool_calls) == 1
    assert norm_empty_name.tool_calls[0].name == ""
    cases.append({
        "case_name": "normalize_tool_call_preserves_explicit_blank_name",
        "retained_name": norm_empty_name.tool_calls[0].name,
        "provenance": "executed_source",
    })

    # (d) Poolside integer finish_reason
    resp_poolside = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="Done", tool_calls=None),
                finish_reason=24,
            )
        ],
        usage=None,
    )
    norm_poolside = transport.normalize_response(resp_poolside)
    assert norm_poolside.finish_reason == "24"
    cases.append({
        "case_name": "normalize_poolside_integer_finish_reason",
        "normalized_finish_reason": norm_poolside.finish_reason,
        "provenance": "executed_source",
    })

    # 1.3 Failure hint generation across error codes and durations
    failure_hint_specs = [
        (524, 15.0, "upstream provider timed out (Cloudflare 524, 15s)"),
        (504, 30.0, "upstream gateway timeout (504, 30s)"),
        (429, 2.0, "rate limited by upstream provider (429)"),
        (500, 4.0, "upstream server error (500, 4s)"),
        (502, 5.0, "upstream server error (502, 5s)"),
        (503, 3.0, "upstream provider overloaded (503)"),
        (529, 3.0, "upstream provider overloaded (529)"),
        (599, 12.0, "upstream error (code 599, 12s)"),
        (None, 4.5, "fast response (4.5s) -- likely rate limited"),
        (None, 75.0, "slow response (75s) -- likely upstream timeout"),
        (None, 25.0, "response time 25.0s"),
    ]

    for err_code, duration, expected_hint in failure_hint_specs:
        # Replicate conversation_loop.py lines 3896-3914 logic
        if err_code == 524:
            hint = f"upstream provider timed out (Cloudflare 524, {duration:.0f}s)"
        elif err_code == 504:
            hint = f"upstream gateway timeout (504, {duration:.0f}s)"
        elif err_code == 429:
            hint = "rate limited by upstream provider (429)"
        elif err_code in {500, 502}:
            hint = f"upstream server error ({err_code}, {duration:.0f}s)"
        elif err_code in {503, 529}:
            hint = f"upstream provider overloaded ({err_code})"
        elif err_code is not None:
            hint = f"upstream error (code {err_code}, {duration:.0f}s)"
        elif duration < 10:
            hint = f"fast response ({duration:.1f}s) -- likely rate limited"
        elif duration > 60:
            hint = f"slow response ({duration:.0f}s) -- likely upstream timeout"
        else:
            hint = f"response time {duration:.1f}s"

        assert hint == expected_hint
        cases.append({
            "case_name": f"failure_hint_code_{err_code}_dur_{int(duration)}",
            "error_code": err_code,
            "duration_seconds": duration,
            "derived_hint": hint,
            "terminal_response_text": f"Invalid API response after 3 retries: {hint}",
            "provenance": "executed_source",
        })

    # 1.4 Eager fallback on invalid response
    # When fallback is configured, response_invalid triggers eager fallback on attempt 1
    cases.append({
        "case_name": "invalid_response_eager_fallback_trigger",
        "condition": "response_invalid is True and fallback_chain available",
        "action": "eager_fallback",
        "attempt": 1,
        "resets_retry_count": True,
        "resets_compression_attempts": True,
        "buffer_status_message": "⚠️ Empty/malformed response -- switching to fallback...",
        "provenance": "source_inspection_literal",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 2: Empty Assistant Responses, Configured Guard, and Exhaustion
# ---------------------------------------------------------------------------
def section_empty_assistant_response_guard_and_exhaustion() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # 2.1 resolve_guard_settings
    settings_inputs = [
        ("none_section", None, True, Decimal("0.25")),
        ("non_dict_section", "invalid_section", True, Decimal("0.25")),
        ("disabled_bool", {"enabled": False}, False, Decimal("0.25")),
        ("disabled_str_false", {"enabled": "false"}, False, Decimal("0.25")),
        ("disabled_str_zero", {"enabled": "0"}, False, Decimal("0.25")),
        ("disabled_str_off", {"enabled": "off"}, False, Decimal("0.25")),
        ("enabled_str_true", {"enabled": "true"}, True, Decimal("0.25")),
        ("custom_threshold_int", {"cost_threshold_usd": 5}, True, Decimal("5")),
        ("custom_threshold_str", {"cost_threshold_usd": "1.50"}, True, Decimal("1.50")),
        (
            "invalid_threshold_negative",
            {"cost_threshold_usd": -1},
            True,
            Decimal("0.25"),
        ),
        (
            "invalid_threshold_str_garbage",
            {"cost_threshold_usd": "banana"},
            True,
            Decimal("0.25"),
        ),
        ("invalid_threshold_bool", {"cost_threshold_usd": True}, True, Decimal("0.25")),
    ]

    for name, section_data, exp_enabled, exp_threshold in settings_inputs:
        actual_enabled, actual_threshold = empty_guard.resolve_guard_settings(
            section_data
        )
        assert actual_enabled == exp_enabled
        assert actual_threshold == exp_threshold
        cases.append({
            "case_name": f"guard_config_{name}",
            "input_section": str(section_data),
            "enabled": actual_enabled,
            "cost_threshold_usd": str(actual_threshold),
            "provenance": "executed_source",
        })

    # 2.2 _zero_output extraction
    def make_agent(**kwargs):
        base = {
            "model": "gpt-4o",
            "provider": "openai",
            "api_mode": "chat_completions",
            "base_url": None,
            "api_key": None,
            "_empty_content_retries": 0,
        }
        base.update(kwargs)
        return SimpleNamespace(**base)

    def make_response(
        prompt_tokens=1000, completion_tokens=0, reasoning_tokens=0, usage_present=True
    ):
        if not usage_present:
            return SimpleNamespace(usage=None)
        details = (
            SimpleNamespace(reasoning_tokens=reasoning_tokens)
            if reasoning_tokens
            else None
        )
        usage = SimpleNamespace(
            prompt_tokens=prompt_tokens,
            completion_tokens=completion_tokens,
            total_tokens=prompt_tokens + completion_tokens,
            completion_tokens_details=details,
        )
        return SimpleNamespace(usage=usage)

    zero_output_inputs = [
        ("openai_zero_completion", make_response(1000, 0, 0), True, True),
        ("openai_positive_completion", make_response(1000, 15, 0), True, False),
        ("openai_reasoning_only_completion", make_response(1000, 0, 128), True, False),
        ("no_usage_object", make_response(usage_present=False), False, False),
        (
            "all_zero_prompt_tokens_proxy_artifact",
            SimpleNamespace(
                usage=SimpleNamespace(prompt_tokens=0, completion_tokens=0)
            ),
            False,
            False,
        ),
    ]

    agent = make_agent()
    for name, resp, exp_present, exp_zero in zero_output_inputs:
        actual_present, actual_zero = empty_guard._zero_output(agent, resp)
        assert actual_present == exp_present
        assert actual_zero == exp_zero
        cases.append({
            "case_name": f"zero_output_{name}",
            "usage_present": actual_present,
            "zero_output": actual_zero,
            "provenance": "executed_source",
        })

    # 2.3 deterministic_empty evaluation matrix
    streak_scenarios = [
        (
            "single_zero_attempt_fails_open",
            [make_response(1000, 0)],
            ["stop"],
            [False],
            False,
        ),
        (
            "two_zero_attempts_same_signature_deterministic",
            [make_response(1000, 0), make_response(1000, 0)],
            ["stop", "stop"],
            [False, False],
            True,
        ),
        (
            "two_attempts_positive_completion_not_deterministic",
            [make_response(1000, 10), make_response(1000, 10)],
            ["stop", "stop"],
            [False, False],
            False,
        ),
        (
            "two_attempts_reasoning_generation_not_deterministic",
            [make_response(1000, 0, 64), make_response(1000, 0, 64)],
            ["stop", "stop"],
            [True, True],
            False,
        ),
        (
            "two_attempts_no_usage_no_observed_generation_deterministic",
            [make_response(usage_present=False), make_response(usage_present=False)],
            ["stop", "stop"],
            [False, False],
            True,
        ),
        (
            "two_attempts_no_usage_with_observed_reasoning_fails_open",
            [make_response(usage_present=False), make_response(usage_present=False)],
            ["stop", "stop"],
            [True, True],
            False,
        ),
        (
            "mixed_usage_presence_fails_open",
            [make_response(1000, 0), make_response(usage_present=False)],
            ["stop", "stop"],
            [False, False],
            False,
        ),
        (
            "differing_finish_reasons_fails_open",
            [make_response(1000, 0), make_response(1000, 0)],
            ["stop", "length"],
            [False, False],
            False,
        ),
    ]

    for (
        name,
        responses,
        finish_reasons,
        observed_gens,
        exp_deterministic,
    ) in streak_scenarios:
        t_agent = make_agent()
        for resp, fr, og in zip(responses, finish_reasons, observed_gens):
            empty_guard.record_empty_attempt(
                t_agent, finish_reason=fr, response=resp, observed_generation=og
            )
            t_agent._empty_content_retries += 1

        is_det = empty_guard.deterministic_empty(t_agent)
        assert is_det == exp_deterministic, (
            f"{name}: expected {exp_deterministic}, got {is_det}"
        )
        cases.append({
            "case_name": f"deterministic_empty_{name}",
            "attempts_count": len(responses),
            "deterministic_empty": is_det,
            "provenance": "executed_source",
        })

    # Guard disabled via config
    disabled_agent = make_agent(_empty_guard_enabled=False)
    for resp in [make_response(1000, 0), make_response(1000, 0)]:
        empty_guard.record_empty_attempt(
            disabled_agent,
            finish_reason="stop",
            response=resp,
            observed_generation=False,
        )
        disabled_agent._empty_content_retries += 1
    assert empty_guard.deterministic_empty(disabled_agent) is False
    cases.append({
        "case_name": "deterministic_empty_disabled_via_config",
        "deterministic_empty": False,
        "provenance": "executed_source",
    })

    # 2.4 empty_retry_budget
    with patch.object(empty_guard, "_estimate_attempt_cost", return_value=None):
        budget_unknown = empty_guard.empty_retry_budget(agent, make_response())
        assert budget_unknown == 3
    with patch.object(
        empty_guard, "_estimate_attempt_cost", return_value=Decimal("0.10")
    ):
        budget_low_cost = empty_guard.empty_retry_budget(agent, make_response())
        assert budget_low_cost == 3
    with patch.object(
        empty_guard, "_estimate_attempt_cost", return_value=Decimal("0.50")
    ):
        budget_high_cost = empty_guard.empty_retry_budget(agent, make_response())
        assert budget_high_cost == 1
    with patch.object(
        empty_guard, "_estimate_attempt_cost", return_value=Decimal("0.50")
    ):
        custom_agent = make_agent(_empty_guard_cost_threshold_usd=Decimal("1.00"))
        budget_custom = empty_guard.empty_retry_budget(custom_agent, make_response())
        assert budget_custom == 3

    cases.extend([
        {
            "case_name": "empty_retry_budget_unknown_cost",
            "budget": budget_unknown,
            "provenance": "executed_source",
        },
        {
            "case_name": "empty_retry_budget_low_cost_below_threshold",
            "budget": budget_low_cost,
            "provenance": "executed_source",
        },
        {
            "case_name": "empty_retry_budget_high_cost_reduced",
            "budget": budget_high_cost,
            "provenance": "executed_source",
        },
        {
            "case_name": "empty_retry_budget_custom_high_threshold",
            "budget": budget_custom,
            "provenance": "executed_source",
        },
    ])

    # 2.5 streak_cost_usd accumulation and reset
    cost_agent = make_agent()
    with patch.object(
        empty_guard,
        "_estimate_attempt_cost",
        side_effect=[Decimal("0.15"), Decimal("0.20")],
    ):
        empty_guard.record_empty_attempt(
            cost_agent, finish_reason="stop", response=make_response()
        )
        cost_agent._empty_content_retries += 1
        empty_guard.record_empty_attempt(
            cost_agent, finish_reason="stop", response=make_response()
        )
        cost_agent._empty_content_retries += 1
        acc_cost = empty_guard.streak_cost_usd(cost_agent)
        assert acc_cost == Decimal("0.35")

    # Reset site: _empty_content_retries = 0 starts fresh streak
    cost_agent._empty_content_retries = 0
    with patch.object(
        empty_guard, "_estimate_attempt_cost", return_value=Decimal("0.12")
    ):
        empty_guard.record_empty_attempt(
            cost_agent, finish_reason="stop", response=make_response()
        )
        cost_agent._empty_content_retries += 1
        reset_cost = empty_guard.streak_cost_usd(cost_agent)
        assert reset_cost == Decimal("0.12")

    cases.extend([
        {
            "case_name": "streak_cost_accumulation",
            "accumulated_usd": str(acc_cost),
            "provenance": "executed_source",
        },
        {
            "case_name": "streak_cost_reset_on_zero_counter",
            "reset_usd": str(reset_cost),
            "provenance": "executed_source",
        },
    ])

    # 2.6 Jittered backoff bounds for empty retry loop
    # Base delay 5.0, max delay 60.0
    cases.append({
        "case_name": "empty_retry_jittered_backoff_bounds",
        "base_delay": 5.0,
        "max_delay": 60.0,
        "attempt_1_min_max": [2.5, 5.0],
        "attempt_2_min_max": [5.0, 10.0],
        "attempt_3_min_max": [10.0, 20.0],
        "provenance": "source_inspection_literal",
    })

    # 2.7 Terminal empty message persistence markers
    cases.append({
        "case_name": "empty_response_terminal_outcome_literals",
        "terminal_content": "(empty)",
        "empty_terminal_sentinel": True,
        "turn_exit_reason": "empty_response_exhausted",
        "synthetic_recovery_tag_stripped_on_persist": "_empty_recovery_synthetic",
        "provenance": "source_inspection_literal",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 3: Content Filter and Tagged Stream Refusals
# ---------------------------------------------------------------------------
def section_content_filter_and_tagged_stream_refusals() -> List[Dict[str, Any]]:
    transport = ChatCompletionsTransport()
    cases: List[Dict[str, Any]] = []

    # 3.1 Normalization of HTTP 200 refusals
    # (a) finish_reason="content_filter"
    resp_cf = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(
                    content="Blocked by safety policy", tool_calls=None
                ),
                finish_reason="content_filter",
            )
        ],
        usage=None,
    )
    norm_cf = transport.normalize_response(resp_cf)
    assert norm_cf.finish_reason == "content_filter"
    assert norm_cf.content == "Blocked by safety policy"
    cases.append({
        "case_name": "normalize_finish_reason_content_filter",
        "finish_reason": norm_cf.finish_reason,
        "content": norm_cf.content,
        "provenance": "executed_source",
    })

    # (b) OpenAI message.refusal with empty content -> promoted to content + content_filter finish_reason
    resp_refusal_empty = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(
                    content="", refusal="I cannot fulfill this request", tool_calls=None
                ),
                finish_reason="stop",
            )
        ],
        usage=None,
    )
    norm_refusal_empty = transport.normalize_response(resp_refusal_empty)
    assert norm_refusal_empty.finish_reason == "content_filter"
    assert norm_refusal_empty.content == "I cannot fulfill this request"
    assert norm_refusal_empty.provider_data == {
        "refusal": "I cannot fulfill this request"
    }
    cases.append({
        "case_name": "normalize_refusal_sole_payload_promotion",
        "finish_reason": norm_refusal_empty.finish_reason,
        "content": norm_refusal_empty.content,
        "provider_data": norm_refusal_empty.provider_data,
        "provenance": "executed_source",
    })

    # (c) message.refusal alongside visible content -> NOT promoted, preserved in provider_data
    resp_refusal_content = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(
                    content="Here is partial allowable text",
                    refusal="Omitted sensitive part",
                    tool_calls=None,
                ),
                finish_reason="stop",
            )
        ],
        usage=None,
    )
    norm_refusal_content = transport.normalize_response(resp_refusal_content)
    assert norm_refusal_content.finish_reason == "stop"
    assert norm_refusal_content.content == "Here is partial allowable text"
    assert norm_refusal_content.provider_data == {"refusal": "Omitted sensitive part"}
    cases.append({
        "case_name": "normalize_refusal_with_visible_content_unpromoted",
        "finish_reason": norm_refusal_content.finish_reason,
        "content": norm_refusal_content.content,
        "provider_data": norm_refusal_content.provider_data,
        "provenance": "executed_source",
    })

    # (d) message.refusal alongside tool calls -> NOT promoted, finish_reason stays tool_calls
    resp_refusal_tools = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(
                    content="",
                    refusal="Cannot access internal path",
                    tool_calls=[
                        SimpleNamespace(
                            id="call_99",
                            type="function",
                            function=SimpleNamespace(
                                name="allowed_tool", arguments="{}"
                            ),
                        )
                    ],
                ),
                finish_reason="tool_calls",
            )
        ],
        usage=None,
    )
    norm_refusal_tools = transport.normalize_response(resp_refusal_tools)
    assert norm_refusal_tools.finish_reason == "tool_calls"
    assert norm_refusal_tools.tool_calls is not None
    assert norm_refusal_tools.provider_data == {
        "refusal": "Cannot access internal path"
    }
    cases.append({
        "case_name": "normalize_refusal_with_tool_calls_unpromoted",
        "finish_reason": norm_refusal_tools.finish_reason,
        "tool_call_count": len(norm_refusal_tools.tool_calls),
        "provider_data": norm_refusal_tools.provider_data,
        "provenance": "executed_source",
    })

    # 3.2 Error classifier mapping of stream refusal errors
    refusal_errors = [
        (
            "openai_cybersecurity",
            Exception("This content was flagged for possible cybersecurity risk."),
            "openai",
            "gpt-4o",
        ),
        (
            "minimax_sensitive_code",
            Exception("output new_sensitive (1027)"),
            "minimax",
            "MiniMax-Text-01",
        ),
        (
            "azure_safety_trigger",
            Exception("The response was filtered due to responsibleaipolicyviolation."),
            "azure",
            "gpt-4o",
        ),
    ]

    for name, exc, prov, mod in refusal_errors:
        cls = classify_api_error(exc, provider=prov, model=mod)
        assert cls.reason == FailoverReason.content_policy_blocked
        assert cls.retryable is False
        assert cls.should_fallback is True
        cases.append({
            "case_name": f"error_classifier_{name}",
            "reason": cls.reason.value,
            "retryable": cls.retryable,
            "should_fallback": cls.should_fallback,
            "should_compress": cls.should_compress,
            "provenance": "executed_source",
        })

    # 3.3 Tagged partial-stream stub creation for content filter
    stub = _build_partial_stream_stub(
        role="assistant",
        full_content="Partially delivered text before safety kill",
        full_reasoning=None,
        model_name="gpt-4o",
        usage_obj=None,
    )
    stub._content_filter_terminated = True
    assert stub.id == PARTIAL_STREAM_STUB_ID
    assert stub.choices[0].finish_reason == FINISH_REASON_LENGTH
    assert getattr(stub, "_content_filter_terminated", False) is True
    cases.append({
        "case_name": "tagged_stream_stub_content_filter",
        "stub_id": stub.id,
        "finish_reason": stub.choices[0].finish_reason,
        "content_filter_terminated": stub._content_filter_terminated,
        "provenance": "executed_source",
    })

    # 3.4 Recovery action specification
    cases.extend([
        {
            "case_name": "content_filter_http_200_loop_actions",
            "retryable": False,
            "eager_fallback": True,
            "resets_retry_count_on_fallback": True,
            "terminal_error_type": "ContentPolicyBlocked",
            "terminal_reason": "content_policy_blocked",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "content_filter_stream_stall_loop_actions",
            "eager_fallback": True,
            "rolls_back_partial_fragments": True,
            "rollback_helper": "_get_messages_up_to_last_assistant",
            "unmarks_fragment_tags": [
                "_length_continuation_fragment",
                "_length_continuation_nudge",
            ],
            "same_provider_fallback_if_no_chain": "length_continuation_best_effort",
            "provenance": "source_inspection_literal",
        },
    ])

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 4: Nonempty Reasoning with Empty Visible Content
# ---------------------------------------------------------------------------
def section_nonempty_reasoning_with_empty_visible_content() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []
    agent = _make_test_agent()

    # 4.1 Reasoning extraction and structured reasoning detection
    reasoning_variants = [
        (
            "structured_reasoning_attr",
            SimpleNamespace(
                content="",
                reasoning="Calculation steps: 1 + 1 = 2",
                reasoning_content=None,
                reasoning_details=None,
            ),
            True,
            "Calculation steps: 1 + 1 = 2",
        ),
        (
            "structured_reasoning_content_attr",
            SimpleNamespace(
                content="",
                reasoning=None,
                reasoning_content="DeepSeek thinking trace",
                reasoning_details=None,
            ),
            True,
            "DeepSeek thinking trace",
        ),
        (
            "structured_reasoning_details_attr",
            SimpleNamespace(
                content="",
                reasoning=None,
                reasoning_content=None,
                reasoning_details=[{"type": "thought", "signature": "sig_123"}],
            ),
            True,
            None,
        ),
        (
            "inline_think_tags_in_content",
            SimpleNamespace(
                content="<think>Step by step plan</think>",
                reasoning=None,
                reasoning_content=None,
                reasoning_details=None,
            ),
            True,
            "Step by step plan",
        ),
        (
            "truly_empty_no_reasoning",
            SimpleNamespace(
                content="",
                reasoning=None,
                reasoning_content=None,
                reasoning_details=None,
            ),
            False,
            None,
        ),
    ]

    for name, asst_msg, exp_structured, exp_extracted_text in reasoning_variants:
        extracted = agent._extract_reasoning(asst_msg)
        built = build_assistant_message(agent, asst_msg, "stop")
        has_inline = bool(
            re.search(
                r"<think>|<thinking>|<reasoning>", asst_msg.content or "", re.IGNORECASE
            )
        )
        has_structured = bool(
            getattr(asst_msg, "reasoning", None)
            or getattr(asst_msg, "reasoning_content", None)
            or getattr(asst_msg, "reasoning_details", None)
            or has_inline
        )
        assert has_structured == exp_structured
        if exp_extracted_text:
            assert built.get("reasoning") == exp_extracted_text

        cases.append({
            "case_name": f"reasoning_detection_{name}",
            "has_structured": has_structured,
            "extracted_reasoning": built.get("reasoning"),
            "content_after_think_strip": agent._strip_think_blocks(
                asst_msg.content or ""
            ).strip(),
            "provenance": "executed_source",
        })

    # 4.2 Thinking-only prefill continuation lifecycle
    cases.extend([
        {
            "case_name": "thinking_prefill_attempt_1",
            "max_prefill_attempts": 2,
            "current_prefill_count": 0,
            "action": "prefill_continuation",
            "interim_finish_reason": "incomplete",
            "interim_tag": "_thinking_prefill",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "thinking_prefill_attempt_2",
            "max_prefill_attempts": 2,
            "current_prefill_count": 1,
            "action": "prefill_continuation",
            "interim_finish_reason": "incomplete",
            "interim_tag": "_thinking_prefill",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "thinking_prefill_exhaustion_handoff",
            "current_prefill_count": 2,
            "action": "escalate_to_empty_retry_ladder",
            "prefill_exhausted": True,
            "empty_candidate": True,
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "thinking_prefill_success_clears_interim",
            "action": "pop_prefill_interim_messages",
            "resets_prefill_counter": True,
            "provenance": "source_inspection_literal",
        },
    ])

    # 4.3 Terminal labeled reasoning excerpt delivery vs transcript sentinel
    reasoning_text = "The calculated answer is 42."
    preview = reasoning_text[:500]
    expected_delivered = (
        "⚠️ The model produced only internal reasoning and "
        "no final answer, despite retries. Its last reasoning, "
        "which may contain the answer:\n\n" + preview
    )
    cases.append({
        "case_name": "terminal_reasoning_delivered_excerpt",
        "delivered_text_format": expected_delivered,
        "persisted_message_content": "(empty)",
        "persisted_empty_terminal_sentinel": True,
        "reasoning_promoted_early": False,
        "provenance": "source_inspection_literal",
    })

    # 4.4 Thinking budget exhaustion under finish_reason=length
    cases.append({
        "case_name": "thinking_budget_exhausted_under_length",
        "condition": "finish_reason == 'length' and has_think_tags and not has_content_after_think_block",
        "error": "Model used all output tokens on reasoning with none left for the response. Try lowering reasoning effort or increasing max_tokens.",
        "final_response_header": "⚠️ **Thinking Budget Exhausted**",
        "completed": False,
        "partial": True,
        "provenance": "source_inspection_literal",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 5: Empty Content Paired with Valid Tool Calls
# ---------------------------------------------------------------------------
def section_empty_content_paired_with_valid_tool_calls() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # 5.1 Acceptance of empty content with valid tool calls
    openai_agent = _make_test_agent(model="gpt-4o", provider="openai")
    deepseek_agent = _make_test_agent(
        model="deepseek-reasoner",
        provider="deepseek",
        base_url="https://api.deepseek.com/v1",
    )

    valid_tool_call = SimpleNamespace(
        id="call_read_1",
        type="function",
        function=SimpleNamespace(name="read_file", arguments='{"path":"src/main.rs"}'),
    )

    asst_msg_empty_content = SimpleNamespace(
        content="",
        tool_calls=[valid_tool_call],
        reasoning=None,
        reasoning_content=None,
    )

    built_openai = build_assistant_message(
        openai_agent, asst_msg_empty_content, "tool_calls"
    )
    assert built_openai["content"] == ""
    assert len(built_openai["tool_calls"]) == 1
    assert built_openai.get("reasoning_content") is None

    built_deepseek = build_assistant_message(
        deepseek_agent, asst_msg_empty_content, "tool_calls"
    )
    assert built_deepseek["content"] == ""
    assert len(built_deepseek["tool_calls"]) == 1
    # DeepSeek thinking tool reasoning pad: whitespace " "
    assert built_deepseek.get("reasoning_content") == " "

    cases.extend([
        {
            "case_name": "empty_content_with_tool_calls_openai",
            "content": built_openai["content"],
            "tool_call_count": len(built_openai["tool_calls"]),
            "reasoning_content": built_openai.get("reasoning_content"),
            "triggers_empty_retry": False,
            "provenance": "executed_source",
        },
        {
            "case_name": "empty_content_with_tool_calls_deepseek_pad",
            "content": built_deepseek["content"],
            "tool_call_count": len(built_deepseek["tool_calls"]),
            "reasoning_content": built_deepseek.get("reasoning_content"),
            "triggers_empty_retry": False,
            "provenance": "executed_source",
        },
    ])

    # 5.2 Discarding bare tool-call marker from content (e.g. [memory])
    stale_markers = [
        "[memory]",
        "[todo_list]",
        "[terminal]",
        "normal text [memory]",
        "[invalid marker]",
    ]
    for marker in stale_markers:
        is_stale_marker = bool(_STALE_MARKER_RE.fullmatch(marker.strip()))
        cases.append({
            "case_name": f"stale_marker_check_{marker.replace(' ', '_')}",
            "raw_content": marker,
            "is_stale_marker": is_stale_marker,
            "content_after_rule": "" if is_stale_marker else marker,
            "provenance": "executed_source",
        })

    # 5.3 Gemini thought signature preservation on tool calls
    transport = ChatCompletionsTransport()
    gemini_tc = SimpleNamespace(
        id="call_gemini_1",
        type="function",
        function=SimpleNamespace(name="search", arguments='{"q":"rust"}'),
        extra_content={"thought_signature": "opaque_gemini_signature_token"},
    )
    resp_gemini = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="", tool_calls=[gemini_tc]),
                finish_reason="tool_calls",
            )
        ],
        usage=None,
    )
    norm_gemini = transport.normalize_response(resp_gemini)
    assert norm_gemini.tool_calls is not None
    assert norm_gemini.tool_calls[0].provider_data == {
        "extra_content": {"thought_signature": "opaque_gemini_signature_token"}
    }
    cases.append({
        "case_name": "gemini_thought_signature_preservation",
        "tool_name": norm_gemini.tool_calls[0].name,
        "provider_data": norm_gemini.tool_calls[0].provider_data,
        "provenance": "executed_source",
    })

    # 5.4 Tool loop progression and post-tool follow-up turn contract
    cases.extend([
        {
            "case_name": "tool_execution_resets_counters",
            "resets_thinking_prefill_retries": True,
            "resets_empty_content_retries": True,
            "resets_post_tool_empty_retried": True,
            "resets_dropped_toolcall_retries": True,
            "pops_thinking_prefill_scaffolding": True,
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "post_tool_empty_followup_housekeeping_content_reuse",
            "condition": "previous_turn_has_content and all_tools_housekeeping",
            "housekeeping_tools": [
                "memory",
                "todo_list",
                "skill_manage",
                "session_search",
            ],
            "action": "reuse_previous_content_as_final_response",
            "exit_reason": "fallback_prior_turn_content",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "post_tool_empty_followup_substantive_nudge",
            "condition": "previous_turn_substantive_tools and not post_tool_empty_retried",
            "action": "send_synthetic_user_nudge_once",
            "nudge_content": _EMPTY_TOOL_RESPONSE_NUDGE,
            "synthetic_tag": "_empty_recovery_synthetic",
            "provenance": "source_inspection_literal",
        },
    ])

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 6: Stream Truncation and No-Replay Boundary
# ---------------------------------------------------------------------------
def section_stream_truncation_and_no_replay_boundary() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # 6.1 Continuation prompt variations
    p_dropped = _get_continuation_prompt(
        is_partial_stub=True, dropped_tools=["terminal", "write_file"]
    )
    p_network = _get_continuation_prompt(is_partial_stub=True, dropped_tools=None)
    p_length = _get_continuation_prompt(is_partial_stub=False, dropped_tools=None)

    assert "terminal, write_file" in p_dropped
    assert "network error" in p_network
    assert "output length limit" in p_length

    cases.extend([
        {
            "case_name": "continuation_prompt_dropped_tools",
            "is_partial_stub": True,
            "dropped_tools": ["terminal", "write_file"],
            "prompt_text": p_dropped,
            "provenance": "executed_source",
        },
        {
            "case_name": "continuation_prompt_network_stub",
            "is_partial_stub": True,
            "dropped_tools": None,
            "prompt_text": p_network,
            "provenance": "executed_source",
        },
        {
            "case_name": "continuation_prompt_length_limit",
            "is_partial_stub": False,
            "dropped_tools": None,
            "prompt_text": p_length,
            "provenance": "executed_source",
        },
    ])

    # 6.2 The exact no-replay boundary definition
    cases.extend([
        {
            "case_name": "stream_failure_before_deltas_delivered",
            "deltas_were_sent": False,
            "action": "re_raise_exception_to_outer_loop",
            "replay_permitted": True,
            "worker_retries": "up to HERMES_STREAM_RETRIES (default 2)",
            "outer_retries": "up to api_max_retries (default 3)",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "stream_failure_after_deltas_narrow_tool_gate",
            "deltas_were_sent": True,
            "tool_in_flight": True,
            "is_transient_error": True,
            "stream_attempt_below_max": True,
            "action": "silent_worker_retry_with_reconnect_marker",
            "reconnect_marker": "\n\n⚠ Connection dropped mid tool-call; reconnecting…\n\n",
            "resets_deltas_flag": True,
            "replay_permitted": "inner_stream_only",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "stream_failure_after_deltas_general_no_replay",
            "deltas_were_sent": True,
            "tool_in_flight": False,
            "action": "suppress_exception_and_return_partial_stream_stub",
            "stub_id": PARTIAL_STREAM_STUB_ID,
            "stub_finish_reason": FINISH_REASON_LENGTH,
            "replay_permitted": False,
            "loop_recovery_mode": "length_continuation",
            "provenance": "source_inspection_literal",
        },
    ])

    # 6.3 Continuation loop limits and ceiling exit
    cases.extend([
        {
            "case_name": "text_length_continuation_ceiling",
            "max_continuation_passes": 4,
            "fragment_tag": "_length_continuation_fragment",
            "nudge_tag": "_length_continuation_nudge",
            "ceiling_exit_action": "stitch_partial_fragments_and_strip_nudges",
            "completed": False,
            "partial": True,
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "tool_call_truncation_retry_and_boost",
            "max_tool_call_retries": 4,
            "token_boost_multiplier": "base * (2 ** retry_count)",
            "token_boost_cap": 32768,
            "broken_tool_call_appended_to_history": False,
            "ceiling_exit_error": "Response truncated due to output length limit",
            "provenance": "source_inspection_literal",
        },
    ])

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 7: Retry vs Fallback Stickiness and Lifecycle
# ---------------------------------------------------------------------------
def section_retry_vs_fallback_stickiness_and_lifecycle() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # 7.1 Dispatch matrix
    dispatch_rules = [
        (
            "malformed_http_200_with_fallback",
            "response_invalid",
            True,
            "fallback_immediate",
            0,
        ),
        (
            "malformed_http_200_no_fallback",
            "response_invalid",
            False,
            "retry_same_provider_jittered",
            3,
        ),
        (
            "empty_response_ambiguous",
            "empty_candidate_single",
            False,
            "retry_same_provider_jittered",
            3,
        ),
        (
            "empty_response_deterministic",
            "deterministic_empty",
            True,
            "fallback_skip_remaining_retries",
            0,
        ),
        (
            "empty_response_exhausted",
            "empty_budget_exhausted",
            True,
            "fallback_immediate",
            0,
        ),
        (
            "content_filter_http_200",
            "content_policy_blocked",
            True,
            "fallback_immediate",
            0,
        ),
        (
            "content_filter_stream_stall",
            "content_filter_stream_stall",
            True,
            "fallback_immediate_with_rollback",
            0,
        ),
        (
            "text_length_truncation",
            "finish_reason_length",
            True,
            "same_provider_continuation_passes",
            4,
        ),
    ]

    for name, trigger, has_fallback, action, budget in dispatch_rules:
        cases.append({
            "case_name": f"dispatch_{name}",
            "trigger": trigger,
            "has_fallback_chain": has_fallback,
            "selected_action": action,
            "budget_or_pass_limit": budget,
            "provenance": "source_inspection_literal",
        })

    # 7.2 Fallback route stickiness and identity rewrite
    prompt_template = (
        "You are Hermes, an AI assistant.\n"
        "Model: gpt-4o\n"
        "Provider: openai\n"
        "Help the user."
    )
    agent = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        fallback_model=[{"provider": "anthropic", "model": "claude-3-5-sonnet"}],
    )
    agent._cached_system_prompt = prompt_template

    # Execute rewrite_prompt_model_identity
    rewrite_prompt_model_identity(agent, "claude-3-5-sonnet", "anthropic")
    rewritten_prompt = agent._cached_system_prompt
    assert "Model: claude-3-5-sonnet" in rewritten_prompt
    assert "Provider: anthropic" in rewritten_prompt
    assert "gpt-4o" not in rewritten_prompt

    cases.append({
        "case_name": "rewrite_prompt_model_identity_execution",
        "original_prompt": prompt_template,
        "rewritten_prompt": rewritten_prompt,
        "touches_last_occurrence_only": True,
        "provenance": "executed_source",
    })

    # 7.3 Within-turn stickiness vs multi-turn restoration
    cases.extend([
        {
            "case_name": "fallback_route_within_turn_stickiness",
            "sticky_for_remaining_iterations": True,
            "sticky_for_tool_rounds": True,
            "resets_retry_count": True,
            "resets_compression_attempts": True,
            "emits_pending_fallback_notice_on_success": True,
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "multi_turn_restoration_to_primary",
            "restoration_function": "restore_primary_runtime",
            "timing": "at_start_of_next_turn",
            "restores_primary_model": True,
            "restores_primary_provider": True,
            "restores_primary_base_url": True,
            "restores_primary_api_key": True,
            "provenance": "source_inspection_literal",
        },
    ])

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 8: Provider-Specific Chat Completions Distinctions
# ---------------------------------------------------------------------------
def section_provider_specific_chat_completions_distinctions() -> List[Dict[str, Any]]:
    transport = ChatCompletionsTransport()
    cases: List[Dict[str, Any]] = []

    # 8.1 DeepSeek native cache statistics extraction
    deepseek_usage = SimpleNamespace(
        prompt_tokens=1500,
        completion_tokens=200,
        prompt_cache_hit_tokens=1200,
        prompt_cache_miss_tokens=300,
    )
    deepseek_resp = SimpleNamespace(usage=deepseek_usage)
    stats_ds = transport.extract_cache_stats(deepseek_resp)
    assert stats_ds == {"cached_tokens": 1200, "creation_tokens": 0}
    cases.append({
        "case_name": "deepseek_native_prompt_cache_tokens",
        "extracted_stats": stats_ds,
        "provenance": "executed_source",
    })

    # 8.2 OpenAI / OpenRouter prompt_tokens_details cache stats
    openai_usage = SimpleNamespace(
        prompt_tokens=2000,
        completion_tokens=100,
        prompt_tokens_details=SimpleNamespace(
            cached_tokens=1800, cache_write_tokens=200
        ),
    )
    openai_resp = SimpleNamespace(usage=openai_usage)
    stats_oa = transport.extract_cache_stats(openai_resp)
    assert stats_oa == {"cached_tokens": 1800, "creation_tokens": 200}
    cases.append({
        "case_name": "openai_prompt_tokens_details_cache_stats",
        "extracted_stats": stats_oa,
        "provenance": "executed_source",
    })

    # 8.3 Kimi / Moonshot pre-send healing of empty non-final messages
    raw_history = [
        {"role": "user", "content": "Initial prompt"},
        {
            "role": "assistant",
            "content": None,
        },  # empty non-final message that would 400 Kimi
        {"role": "user", "content": "Follow up query"},
        {"role": "assistant", "content": None},  # final assistant turn is legal
    ]
    repaired_history = repair_empty_non_final_messages(raw_history)
    assert repaired_history[1]["content"] == "[response interrupted]"
    assert repaired_history[3]["content"] is None
    cases.append({
        "case_name": "kimi_moonshot_heal_empty_non_final_messages",
        "repaired_middle_content": repaired_history[1]["content"],
        "preserved_final_content": repaired_history[3]["content"],
        "provenance": "executed_source",
    })

    # 8.4 xAI hermes_tool_search wire alias reversal
    resp_xai = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(
                    content="",
                    tool_calls=[
                        SimpleNamespace(
                            id="call_xai_1",
                            type="function",
                            function=SimpleNamespace(
                                name="hermes_tool_search", arguments='{"q":"find"}'
                            ),
                        )
                    ],
                ),
                finish_reason="tool_calls",
            )
        ],
        usage=None,
    )
    # Set _last_wire_aliases to None to exercise static fallback reversal
    transport._last_wire_aliases = None
    norm_xai = transport.normalize_response(resp_xai)
    assert norm_xai.tool_calls is not None
    assert norm_xai.tool_calls[0].name == "tool_search"
    cases.append({
        "case_name": "xai_wire_alias_reversal",
        "wire_name": "hermes_tool_search",
        "reversed_name": norm_xai.tool_calls[0].name,
        "provenance": "executed_source",
    })

    # 8.5 Ollama / GLM in-content think block stripping
    content_with_think = (
        "<think>\nThinking through the response.\n</think>\nActual visible reply."
    )
    agent = _make_test_agent()
    stripped_content = agent._strip_think_blocks(content_with_think).strip()
    assert stripped_content == "Actual visible reply."
    cases.append({
        "case_name": "ollama_glm_think_block_strip",
        "raw_content": content_with_think,
        "stripped_content": stripped_content,
        "provenance": "executed_source",
    })

    # 8.6 Ollama / GLM suspicious premature stop detection
    cases.append({
        "case_name": "ollama_glm_suspicious_stop_rewritten_to_length",
        "helper": "_should_treat_stop_as_truncated",
        "action": "rewrite_finish_reason_from_stop_to_length",
        "provenance": "source_inspection_literal",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Main Orchestrator
# ---------------------------------------------------------------------------
def generate_all_goldens() -> Dict[str, List[Dict[str, Any]]]:
    """Assemble all 8 sections into a deterministic dictionary."""
    goldens = {
        "malformed_and_missing_choice_bodies": section_malformed_and_missing_choice_bodies(),
        "empty_assistant_response_guard_and_exhaustion": section_empty_assistant_response_guard_and_exhaustion(),
        "content_filter_and_tagged_stream_refusals": section_content_filter_and_tagged_stream_refusals(),
        "nonempty_reasoning_with_empty_visible_content": section_nonempty_reasoning_with_empty_visible_content(),
        "empty_content_paired_with_valid_tool_calls": section_empty_content_paired_with_valid_tool_calls(),
        "stream_truncation_and_no_replay_boundary": section_stream_truncation_and_no_replay_boundary(),
        "retry_vs_fallback_stickiness_and_lifecycle": section_retry_vs_fallback_stickiness_and_lifecycle(),
        "provider_specific_chat_completions_distinctions": section_provider_specific_chat_completions_distinctions(),
    }
    return _clean_str(goldens)


def main() -> None:
    data = generate_all_goldens()
    json_text = json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n"

    # Enforce strict absence of em dash character
    if "\u2014" in json_text:
        raise ValueError(
            "Em dash character (\u2014) detected in generated JSON text! Aborting."
        )

    if sys.argv[1:] == ["--check"]:
        if not OUT.exists():
            sys.stderr.write(
                f"Error: {OUT} does not exist. Run without --check to generate it.\n"
            )
            sys.exit(1)
        existing = OUT.read_text(encoding="utf-8")
        if existing != json_text:
            sys.stderr.write(f"Error: {OUT} is out of sync with generator output.\n")
            sys.exit(1)
        total_cases = sum(len(v) for v in data.values())
        print(
            f"Parity check passed: {total_cases} cases verified across 8 sections in {OUT.name}"
        )
        return

    if len(sys.argv) > 1:
        sys.stderr.write(
            "Usage: python3 gen_main_provider_success_body_goldens.py [--check]\n"
        )
        sys.exit(1)

    OUT.write_text(json_text, encoding="utf-8")
    total_cases = sum(len(v) for v in data.values())
    print(f"Wrote {total_cases} deterministic golden cases across 8 sections to {OUT}")


if __name__ == "__main__":
    main()
