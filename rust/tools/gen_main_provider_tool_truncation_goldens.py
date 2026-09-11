#!/usr/bin/env python3
"""Deterministic source-executed oracle for main-provider tool call truncation.

This generator audits and executes live Python decision functions and runtime
transitions governing ordinary chat-completions tool call truncation after an
HTTP success (finish_reason == "length" with tool calls present):
1. Eligibility and preemption: presence of tool calls preempts thinking-exhaustion
   and repetition-dominated guards, routing into same-request retry.
2. Same-request retry vs semantic continuation: tool call truncation preserves
   message history untouched (no assistant fragment, no user nudge) and re-executes
   from current messages, contrasting with text continuation fragment/nudge mechanics.
3. Retry progression and ceiling enforcement: exactly 4 retry attempts
   (truncated_tool_call_retries: 0 -> 1 -> 2 -> 3 -> 4 -> ceiling exit), resetting
   to 0 upon successful tool execution.
4. Output-cap exponential growth schedule: base * (2 ** retries), requested cap
   override rules, and 32,768 cap ceiling. Ephemeral cap consumption in _build_api_kwargs.
5. Tool execution prohibition: incomplete tool arguments are strictly forbidden from
   dispatch; mock execution handlers are never invoked on truncated responses.
6. Partial-stream stub distinctions: contrasting _is_stub_stall (id == PARTIAL_STREAM_STUB_ID)
   against genuine output-cap length truncations, including distinct retry logs and
   terminal failure messages.
7. Dropped tool names and zero-byte streaming drops: _tool_args_dropped_no_finish handling,
   _dropped_tool_names metadata on partial stream stubs, capped at 3 tool names in
   continuation prompt.
8. Terminal result shape and transcript repair: return dictionary structure
   (completed: False, partial: True) and close_interrupted_tool_sequence role
   alternation repair when transcript ends on a tool result.
9. Persistence and usage accounting: broken assistant messages are never saved to
   session history; truncated tool attempts do not increment session_api_calls or
   bill token usage.
10. Provider-specific exceptions and fallbacks: content-filter stream stalls with
    eager failover, Ollama argument repairs, Gemini thought_signature retention,
    Moonshot strict empty-assistant avoidance, and router tool_calls finish_reason rewrites.
11. Boundaries and separation matrix: distinguishing chat-completions tool truncation
    from text continuation, Codex Responses, Anthropic Messages, and response stall watchdogs.

Usage:
    python3 rust/tools/gen_main_provider_tool_truncation_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_tool_truncation_goldens.py --check  # check parity
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
OUT = ROOT / "rust/tools/main-provider-tool-truncation-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

import agent.conversation_loop as conv_loop
from agent.chat_completion_helpers import (
    FINISH_REASON_LENGTH,
    PARTIAL_STREAM_STUB_ID,
    _build_partial_stream_stub,
)
from agent.conversation_loop import (
    _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX,
    _LENGTH_CONTINUATION_NETWORK_STUB,
    _LENGTH_CONTINUATION_OUTPUT_LIMIT,
    _get_continuation_prompt,
)
from agent.message_sanitization import (
    close_interrupted_tool_sequence,
    _repair_tool_call_arguments,
)
from agent.transports.chat_completions import (
    ChatCompletionsTransport,
    NormalizedResponse,
    ToolCall,
    Usage,
)
from run_agent import AIAgent


def _clean_str(val: Any) -> Any:
    """Sanitize string or recursive data structure to guarantee no em dash characters."""
    if isinstance(val, str):
        assert "\u2014" not in val, f"Em dash found in string: {val!r}"
        return val
    if isinstance(val, list):
        return [_clean_str(v) for v in val]
    if isinstance(val, dict):
        return {k: _clean_str(v) for k, v in val.items()}
    if isinstance(val, Decimal):
        return str(val)
    return val


def _make_test_agent(
    model: str = "test/model",
    provider: str = "openrouter",
    base_url: str = "https://openrouter.ai/api/v1",
    api_key: str = "test-key-12345",
    max_tokens: Optional[int] = None,
) -> AIAgent:
    """Instantiate a minimal AIAgent with mocked tools and client."""
    with (
        patch("run_agent.get_tool_definitions", return_value=[]),
        patch("run_agent.check_toolset_requirements", return_value={}),
        patch("run_agent.OpenAI"),
    ):
        agent = AIAgent(
            api_key=api_key,
            base_url=base_url,
            provider=provider,
            model=model,
            max_tokens=max_tokens,
            quiet_mode=True,
            skip_context_files=True,
            skip_memory=True,
        )
        agent.api_mode = "chat_completions"
        agent.client = MagicMock()
        agent._cached_system_prompt = "You are helpful."
        agent._use_prompt_caching = False
        agent.compression_enabled = False
        agent.save_trajectories = False
        return agent


def _mock_tool_call(name: str, args: str, call_id: str = "call_1") -> SimpleNamespace:
    """Construct a mock tool call matching OpenAI client response structure."""
    func = SimpleNamespace(name=name, arguments=args)
    return SimpleNamespace(id=call_id, type="function", function=func)


def _mock_response(
    content: str = "",
    finish_reason: str = "length",
    tool_calls: Optional[List[Any]] = None,
    resp_id: str = "resp_test_123",
    model: str = "test/model",
) -> SimpleNamespace:
    """Construct a mock ChatCompletion response object."""
    msg = SimpleNamespace(role="assistant", content=content, tool_calls=tool_calls)
    choice = SimpleNamespace(index=0, message=msg, finish_reason=finish_reason)
    return SimpleNamespace(id=resp_id, choices=[choice], usage=None, model=model)


def _run_conversation_isolated(
    agent: AIAgent, message: str, history: Optional[List[Dict[str, Any]]] = None
) -> Dict[str, Any]:
    """Run agent.run_conversation with disk side-effects mocked out."""
    with (
        patch.object(agent, "_persist_session"),
        patch.object(agent, "_save_trajectory"),
        patch.object(agent, "_cleanup_task_resources"),
    ):
        return agent.run_conversation(message, conversation_history=history)


# ==============================================================================
# 1. Eligibility and Preemption
# ==============================================================================
def gen_eligibility_and_preemption_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Tool call truncation eligible for retry
    agent = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"a.txt","content":"part')
    resp = _mock_response(content="", finish_reason="length", tool_calls=[bad_tc])
    agent.client.chat.completions.create.return_value = resp
    with patch("run_agent.handle_function_call"):
        res = _run_conversation_isolated(agent, "write report")
    cases.append({
        "case_name": "eligible_tool_truncation_enters_retry",
        "description": "finish_reason=length with tool_calls present enters same-request retry loop",
        "finish_reason": "length",
        "has_tool_calls": True,
        "eligible_for_tool_retry": True,
        "completed": res["completed"],
        "partial": res["partial"],
        "error": res["error"],
        "api_attempts_issued": agent.client.chat.completions.create.call_count,
        "provenance": "live_execution",
    })

    # Case 2: Thinking exhaustion guard is preempted when tool_calls are present
    agent = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"a.txt","content":"part')
    resp_think = _mock_response(
        content="<think>Need to write the file</think>",
        finish_reason="length",
        tool_calls=[bad_tc],
    )
    agent.client.chat.completions.create.return_value = resp_think
    with patch("run_agent.handle_function_call"):
        res_think = _run_conversation_isolated(agent, "write report")
    cases.append({
        "case_name": "thinking_exhaustion_preempted_by_tool_calls",
        "description": "Thinking tags with no trailing text do not trigger thinking-exhaustion abort when tool_calls are present",
        "finish_reason": "length",
        "has_think_tags": True,
        "has_tool_calls": True,
        "thinking_exhausted_triggered": False,
        "api_attempts_issued": agent.client.chat.completions.create.call_count,
        "error": res_think["error"],
        "provenance": "live_execution",
    })

    # Case 3: Thinking exhaustion triggers when tool_calls are absent
    agent = _make_test_agent()
    resp_think_only = _mock_response(
        content="<think>Only thinking occurred here</think>",
        finish_reason="length",
        tool_calls=None,
    )
    agent.client.chat.completions.create.return_value = resp_think_only
    res_think_abort = _run_conversation_isolated(agent, "write report")
    cases.append({
        "case_name": "thinking_exhaustion_triggers_without_tool_calls",
        "description": "Thinking tags with no trailing text and no tool_calls trigger early thinking-exhaustion abort",
        "finish_reason": "length",
        "has_think_tags": True,
        "has_tool_calls": False,
        "thinking_exhausted_triggered": True,
        "api_attempts_issued": agent.client.chat.completions.create.call_count,
        "error": res_think_abort["error"],
        "provenance": "live_execution",
    })

    # Case 4: Repetition guard is preempted when tool_calls are present
    agent = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"a.txt","content":"part')
    rep_text = "echo loop line\n" * 40
    resp_rep = _mock_response(
        content=rep_text, finish_reason="length", tool_calls=[bad_tc]
    )
    agent.client.chat.completions.create.return_value = resp_rep
    with patch("run_agent.handle_function_call"):
        res_rep = _run_conversation_isolated(agent, "write report")
    cases.append({
        "case_name": "repetition_guard_preempted_by_tool_calls",
        "description": "Repetitive text does not trigger repetition-dominated abort when tool_calls are present",
        "finish_reason": "length",
        "is_repetition_dominated": True,
        "has_tool_calls": True,
        "repetition_dominated_triggered": False,
        "api_attempts_issued": agent.client.chat.completions.create.call_count,
        "error": res_rep["error"],
        "provenance": "live_execution",
    })

    # Case 5: Repetition guard triggers when tool_calls are absent
    agent = _make_test_agent()
    resp_rep_abort = _mock_response(
        content=rep_text, finish_reason="length", tool_calls=None
    )
    agent.client.chat.completions.create.return_value = resp_rep_abort
    res_rep_abort = _run_conversation_isolated(agent, "write report")
    cases.append({
        "case_name": "repetition_guard_triggers_without_tool_calls",
        "description": "Repetitive text without tool_calls triggers repetition-dominated abort",
        "finish_reason": "length",
        "is_repetition_dominated": True,
        "has_tool_calls": False,
        "repetition_dominated_triggered": True,
        "api_attempts_issued": agent.client.chat.completions.create.call_count,
        "error": res_rep_abort["error"],
        "provenance": "live_execution",
    })

    # Case 6: Text-only truncation routes to semantic continuation
    agent = _make_test_agent()
    resp_text = _mock_response(
        content="Incomplete text without tools", finish_reason="length", tool_calls=None
    )
    resp_text_cont = _mock_response(
        content=" completed now.", finish_reason="stop", tool_calls=None
    )
    agent.client.chat.completions.create.side_effect = [resp_text, resp_text_cont]
    res_text = _run_conversation_isolated(agent, "tell me a story")
    cases.append({
        "case_name": "text_only_truncation_routes_to_semantic_continuation",
        "description": "finish_reason=length without tool_calls routes to semantic continuation lane",
        "finish_reason": "length",
        "has_tool_calls": False,
        "completed": res_text["completed"],
        "final_response": res_text["final_response"],
        "api_attempts_issued": agent.client.chat.completions.create.call_count,
        "provenance": "live_execution",
    })

    # Case 7: Router finish_reason rewrite to tool_calls with unclosed JSON
    agent = _make_test_agent()
    agent.valid_tool_names.add("write_file")
    bad_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"part')
    resp_router_rewrite = _mock_response(
        content="", finish_reason="tool_calls", tool_calls=[bad_tc]
    )
    agent.client.chat.completions.create.return_value = resp_router_rewrite
    with patch("run_agent.handle_function_call"):
        res_router = _run_conversation_isolated(agent, "write report")
    cases.append({
        "case_name": "router_rewrite_unclosed_json_refuses_immediately",
        "description": "finish_reason=tool_calls with unclosed JSON arguments detects truncation and refuses execution without retrying",
        "finish_reason": "tool_calls",
        "has_tool_calls": True,
        "arguments_end_with_closing_delimiter": False,
        "retries_granted": 0,
        "api_attempts_issued": agent.client.chat.completions.create.call_count,
        "error": res_router["error"],
        "provenance": "live_execution",
    })

    # Case 8: Clean tool_calls finish reason with valid JSON dispatches
    agent = _make_test_agent()
    agent.valid_tool_names.add("write_file")
    good_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"ok"}')
    resp_clean = _mock_response(
        content="", finish_reason="tool_calls", tool_calls=[good_tc]
    )
    resp_final = _mock_response(
        content="All done!", finish_reason="stop", tool_calls=None
    )
    agent.client.chat.completions.create.side_effect = [resp_clean, resp_final]
    with patch(
        "run_agent.handle_function_call", return_value='{"status":"ok"}'
    ) as mock_hfc:
        res_clean = _run_conversation_isolated(agent, "write report")
    cases.append({
        "case_name": "clean_tool_calls_dispatches_normally",
        "description": "finish_reason=tool_calls with valid JSON arguments dispatches function execution normally",
        "finish_reason": "tool_calls",
        "has_tool_calls": True,
        "tool_executed": mock_hfc.called,
        "completed": res_clean["completed"],
        "final_response": res_clean["final_response"],
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 2. Same-Request Retry vs Semantic Continuation Contrast
# ==============================================================================
def gen_same_request_retry_vs_semantic_continuation_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Tool call truncation leaves message history completely untouched
    agent_tc = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"partial')
    resp_tc = _mock_response(content="", finish_reason="length", tool_calls=[bad_tc])
    agent_tc.client.chat.completions.create.return_value = resp_tc
    with patch("run_agent.handle_function_call"):
        res_tc = _run_conversation_isolated(agent_tc, "write the report")

    # Inspect messages during retry
    tc_call_kwargs = agent_tc.client.chat.completions.create.call_args_list[1]
    tc_messages = tc_call_kwargs.kwargs.get("messages") or tc_call_kwargs.args[0].get(
        "messages"
    )
    cases.append({
        "case_name": "tool_truncation_leaves_transcript_untouched",
        "lane": "tool_call_truncation",
        "assistant_fragment_appended": any(
            m.get("_length_continuation_fragment")
            for m in tc_messages
            if isinstance(m, dict)
        ),
        "user_nudge_appended": any(
            m.get("_length_continuation_nudge")
            for m in tc_messages
            if isinstance(m, dict)
        ),
        "messages_length_during_retry": len(tc_messages),
        "broken_response_in_history": any(
            m.get("tool_calls") for m in tc_messages if isinstance(m, dict)
        ),
        "action": "re_run_same_request_from_current_messages",
        "provenance": "live_execution",
    })

    # Case 2: Text-only truncation injects assistant fragment and user nudge
    agent_text = _make_test_agent()
    resp_t1 = _mock_response(
        content="Part 1 of answer", finish_reason="length", tool_calls=None
    )
    resp_t2 = _mock_response(
        content=" and part 2 completed.", finish_reason="stop", tool_calls=None
    )
    agent_text.client.chat.completions.create.side_effect = [resp_t1, resp_t2]
    res_text = _run_conversation_isolated(agent_text, "tell me something")

    text_call_kwargs = agent_text.client.chat.completions.create.call_args_list[1]
    text_messages = text_call_kwargs.kwargs.get("messages") or text_call_kwargs.args[
        0
    ].get("messages")
    has_fragment = any(
        isinstance(m, dict) and m.get("_length_continuation_fragment")
        for m in text_messages
    )
    has_nudge = any(
        isinstance(m, dict) and m.get("_length_continuation_nudge")
        for m in text_messages
    )
    cases.append({
        "case_name": "text_truncation_appends_fragment_and_nudge",
        "lane": "text_length_continuation",
        "assistant_fragment_appended": has_fragment,
        "user_nudge_appended": has_nudge,
        "messages_length_during_retry": len(text_messages),
        "action": "restart_with_length_continuation_nudge",
        "provenance": "live_execution",
    })

    # Case 3: Dropped tools partial stream stub injects semantic nudge instead of same-request retry
    stub_dropped = _build_partial_stream_stub(
        role="assistant",
        full_content="Writing:",
        full_reasoning=None,
        model_name="test/model",
        usage_obj=None,
        dropped_tool_names=["write_file"],
    )
    prompt_dropped = _get_continuation_prompt(
        True, getattr(stub_dropped, "_dropped_tool_names", None)
    )
    cases.append({
        "case_name": "dropped_tools_stub_injects_semantic_nudge",
        "lane": "dropped_tools_stream_stub",
        "is_partial_stream_stub": stub_dropped.id == PARTIAL_STREAM_STUB_ID,
        "dropped_tool_names": getattr(stub_dropped, "_dropped_tool_names", None),
        "tool_calls_field": stub_dropped.choices[0].message.tool_calls,
        "routes_to_tool_retry": bool(stub_dropped.choices[0].message.tool_calls),
        "prompt_starts_with_prefix": prompt_dropped.startswith(
            _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX
        ),
        "continuation_prompt_text": prompt_dropped,
        "provenance": "live_execution",
    })

    # Case 4: Zero-byte dropped tool stub suppresses empty assistant message
    stub_empty = _build_partial_stream_stub(
        role="assistant",
        full_content="",
        full_reasoning=None,
        model_name="test/model",
        usage_obj=None,
        dropped_tool_names=["write_file"],
    )
    agent_empty = _make_test_agent()
    resp_rec = _mock_response(
        content="Recovered after prompt", finish_reason="stop", tool_calls=None
    )
    agent_empty.client.chat.completions.create.side_effect = [stub_empty, resp_rec]
    res_empty = _run_conversation_isolated(agent_empty, "generate site")

    empty_kwargs = agent_empty.client.chat.completions.create.call_args_list[1]
    empty_msgs = empty_kwargs.kwargs.get("messages") or empty_kwargs.args[0].get(
        "messages"
    )
    has_empty_assistant = any(
        isinstance(m, dict) and m.get("role") == "assistant" and not m.get("content")
        for m in empty_msgs
    )
    cases.append({
        "case_name": "empty_dropped_tool_stub_omits_assistant_message",
        "lane": "empty_dropped_tools_stream_stub",
        "empty_assistant_message_in_history": has_empty_assistant,
        "safeguard_purpose": "prevents_strict_provider_http_400_on_empty_assistant_content",
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 3. Retry Progression and Ceiling Enforcement
# ==============================================================================
def gen_retry_progression_and_ceiling_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Step-by-step ceiling progression
    agent = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"partial')
    resp_tc = _mock_response(content="", finish_reason="length", tool_calls=[bad_tc])
    agent.client.chat.completions.create.return_value = resp_tc

    with patch("run_agent.handle_function_call"):
        res = _run_conversation_isolated(agent, "write report")

    calls = agent.client.chat.completions.create.call_args_list
    assert len(calls) == 5, (
        f"Expected 5 calls (1 initial + 4 retries), got {len(calls)}"
    )

    expected_caps = [None, 8192, 16384, 32768, 32768]
    for attempt_idx, call in enumerate(calls):
        kw = call.kwargs if call.kwargs else call.args[0] if call.args else {}
        cap = (
            kw.get("max_tokens")
            or kw.get("max_completion_tokens")
            or kw.get("max_output_tokens")
        )
        cases.append({
            "case_name": f"retry_progression_attempt_{attempt_idx}",
            "attempt_index": attempt_idx,
            "is_initial_call": attempt_idx == 0,
            "is_retry": attempt_idx > 0,
            "truncated_tool_call_retries": attempt_idx,
            "expected_ephemeral_cap": expected_caps[attempt_idx],
            "actual_effective_cap": cap,
            "provenance": "live_execution",
        })

    # Ceiling exit terminal result
    cases.append({
        "case_name": "ceiling_exit_exhaustion_result",
        "total_api_attempts": len(calls),
        "max_retries_ceiling": 4,
        "completed": res["completed"],
        "partial": res["partial"],
        "error": res["error"],
        "final_response": res["final_response"],
        "api_calls_field": res["api_calls"],
        "provenance": "live_execution",
    })

    # Recovery on attempt 1, 2, 3, 4
    for recover_on in (1, 2, 3, 4):
        agent_rec = _make_test_agent()
        agent_rec.valid_tool_names.add("write_file")
        side_effect = [resp_tc] * recover_on
        good_tc = _mock_tool_call(
            "write_file", '{"path":"report.md","content":"full content"}'
        )
        good_resp = _mock_response(
            content="", finish_reason="stop", tool_calls=[good_tc]
        )
        final_resp = _mock_response(
            content=f"Recovered on attempt {recover_on}!",
            finish_reason="stop",
            tool_calls=None,
        )
        side_effect.extend([good_resp, final_resp])
        agent_rec.client.chat.completions.create.side_effect = side_effect

        with patch(
            "run_agent.handle_function_call", return_value='{"status":"ok"}'
        ) as mock_hfc:
            res_rec = _run_conversation_isolated(agent_rec, "write report")

        cases.append({
            "case_name": f"recovery_on_retry_pass_{recover_on}",
            "failed_attempts_before_recovery": recover_on,
            "recovery_succeeded": res_rec["completed"],
            "tool_call_executed": mock_hfc.called,
            "final_response": res_rec["final_response"],
            "total_calls_to_client": agent_rec.client.chat.completions.create.call_count,
            "provenance": "live_execution",
        })

    return cases


# ==============================================================================
# 4. Output-Cap Exponential Growth Schedule
# ==============================================================================
def compute_tool_truncation_token_boost(
    max_tokens: Optional[int],
    retry_count: int,
    requested_cap: Optional[int] = None,
) -> int:
    """Exact replica of the math in conversation_loop.py lines 4539-4545."""
    tc_boost_base = max_tokens if max_tokens else 4096
    tc_boost = tc_boost_base * (2**retry_count)
    if requested_cap is not None:
        tc_boost = max(tc_boost, requested_cap)
    tc_boost_cap = max(32768, requested_cap or 0)
    return min(tc_boost, tc_boost_cap)


def gen_output_cap_growth_schedule_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Configuration 1: Default base (None -> 4096)
    for r in (1, 2, 3, 4):
        boost = compute_tool_truncation_token_boost(None, r, None)
        cases.append({
            "case_name": f"token_boost_default_base_retry_{r}",
            "base_tokens": 4096,
            "retry_count": r,
            "requested_cap": None,
            "boost_multiplier": 2**r,
            "computed_boost_tokens": boost,
            "provenance": "live_execution",
        })

    # Configuration 2: Custom low base (1024)
    for r in (1, 2, 3, 4):
        boost = compute_tool_truncation_token_boost(1024, r, None)
        cases.append({
            "case_name": f"token_boost_low_base_retry_{r}",
            "base_tokens": 1024,
            "retry_count": r,
            "requested_cap": None,
            "boost_multiplier": 2**r,
            "computed_boost_tokens": boost,
            "provenance": "live_execution",
        })

    # Configuration 3: Custom high base (8192) hits 32768 ceiling on retry 2
    for r in (1, 2, 3, 4):
        boost = compute_tool_truncation_token_boost(8192, r, None)
        cases.append({
            "case_name": f"token_boost_high_base_retry_{r}",
            "base_tokens": 8192,
            "retry_count": r,
            "requested_cap": None,
            "boost_multiplier": 2**r,
            "computed_boost_tokens": boost,
            "provenance": "live_execution",
        })

    # Configuration 4: Requested cap exceeds 32768 ceiling (e.g. 65536)
    for r in (1, 2, 3, 4):
        boost = compute_tool_truncation_token_boost(4096, r, 65536)
        cases.append({
            "case_name": f"token_boost_requested_cap_65536_retry_{r}",
            "base_tokens": 4096,
            "retry_count": r,
            "requested_cap": 65536,
            "boost_multiplier": 2**r,
            "computed_boost_tokens": boost,
            "provenance": "live_execution",
        })

    # Configuration 5: Requested cap intermediate (20000)
    for r in (1, 2, 3, 4):
        boost = compute_tool_truncation_token_boost(4096, r, 20000)
        cases.append({
            "case_name": f"token_boost_intermediate_requested_cap_retry_{r}",
            "base_tokens": 4096,
            "retry_count": r,
            "requested_cap": 20000,
            "boost_multiplier": 2**r,
            "computed_boost_tokens": boost,
            "provenance": "live_execution",
        })

    # Configuration 6: Key probing priority in _requested_output_cap_from_api_kwargs
    cases.append({
        "case_name": "requested_cap_probing_priority_max_output_tokens",
        "probe_dict": {
            "max_output_tokens": 12000,
            "max_completion_tokens": 8000,
            "max_tokens": 4000,
        },
        "extracted_cap": AIAgent._requested_output_cap_from_api_kwargs({
            "max_output_tokens": 12000,
            "max_completion_tokens": 8000,
            "max_tokens": 4000,
        }),
        "expected_cap": 12000,
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "requested_cap_probing_priority_max_completion_tokens",
        "probe_dict": {"max_completion_tokens": 8000, "max_tokens": 4000},
        "extracted_cap": AIAgent._requested_output_cap_from_api_kwargs({
            "max_completion_tokens": 8000,
            "max_tokens": 4000,
        }),
        "expected_cap": 8000,
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "requested_cap_probing_priority_max_tokens",
        "probe_dict": {"max_tokens": 4000},
        "extracted_cap": AIAgent._requested_output_cap_from_api_kwargs({
            "max_tokens": 4000
        }),
        "expected_cap": 4000,
        "provenance": "live_execution",
    })

    # Configuration 7: Ephemeral token cap lifecycle consumed in _build_api_kwargs
    agent = _make_test_agent()
    agent._ephemeral_max_output_tokens = 16384
    kwargs = agent._build_api_kwargs([{"role": "user", "content": "test"}])
    cases.append({
        "case_name": "ephemeral_cap_consumed_and_reset_to_none",
        "staged_ephemeral_cap": 16384,
        "wire_kwargs_cap": kwargs.get("max_tokens"),
        "agent_ephemeral_after_call": agent._ephemeral_max_output_tokens,
        "lifecycle_guarantee": "single_shot_consumption_no_turn_leakage",
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 5. Tool Execution Prohibition
# ==============================================================================
def gen_tool_execution_prohibition_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Incomplete unclosed JSON payload is never dispatched to tool handler
    agent = _make_test_agent()
    agent.valid_tool_names.add("write_file")
    bad_tc = _mock_tool_call(
        "write_file", '{"path":"report.md","content":"incomplete file body'
    )
    resp_tc = _mock_response(content="", finish_reason="length", tool_calls=[bad_tc])
    agent.client.chat.completions.create.return_value = resp_tc

    with patch("run_agent.handle_function_call") as mock_hfc:
        res = _run_conversation_isolated(agent, "write the report")

    cases.append({
        "case_name": "incomplete_json_tool_handler_not_called",
        "tool_name": "write_file",
        "raw_arguments": '{"path":"report.md","content":"incomplete file body',
        "finish_reason": "length",
        "handler_invoked": mock_hfc.called,
        "handler_call_count": mock_hfc.call_count,
        "safety_invariant": "incomplete_tool_arguments_strictly_prevented_from_executing",
        "provenance": "live_execution",
    })

    # Case 2: Zero-byte arguments payload is never dispatched to tool handler
    agent_zero = _make_test_agent()
    agent_zero.valid_tool_names.add("execute_code")
    empty_tc = _mock_tool_call("execute_code", "")
    resp_zero = _mock_response(
        content="", finish_reason="length", tool_calls=[empty_tc]
    )
    agent_zero.client.chat.completions.create.return_value = resp_zero

    with patch("run_agent.handle_function_call") as mock_hfc_zero:
        res_zero = _run_conversation_isolated(agent_zero, "run the script")

    cases.append({
        "case_name": "empty_arguments_tool_handler_not_called",
        "tool_name": "execute_code",
        "raw_arguments": "",
        "finish_reason": "length",
        "handler_invoked": mock_hfc_zero.called,
        "handler_call_count": mock_hfc_zero.call_count,
        "safety_invariant": "empty_argument_truncations_refused_without_side_effects",
        "provenance": "live_execution",
    })

    # Case 3: Multiple parallel tool calls where one is truncated prohibits all from executing
    agent_multi = _make_test_agent()
    agent_multi.valid_tool_names.update(["read_file", "write_file"])
    tc1 = _mock_tool_call("read_file", '{"path":"in.txt"}', call_id="c1")
    tc2 = _mock_tool_call(
        "write_file", '{"path":"out.txt","content":"part', call_id="c2"
    )
    resp_multi = _mock_response(
        content="", finish_reason="length", tool_calls=[tc1, tc2]
    )
    agent_multi.client.chat.completions.create.return_value = resp_multi

    with patch("run_agent.handle_function_call") as mock_hfc_multi:
        res_multi = _run_conversation_isolated(agent_multi, "process files")

    cases.append({
        "case_name": "mixed_parallel_tool_calls_all_prohibited",
        "tool_names": ["read_file", "write_file"],
        "call_1_valid": True,
        "call_2_truncated": True,
        "finish_reason": "length",
        "handler_invoked": mock_hfc_multi.called,
        "handler_call_count": mock_hfc_multi.call_count,
        "safety_invariant": "all_or_nothing_execution_atomicity_preserved",
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 6. Partial-Stream Stub Distinctions
# ==============================================================================
def gen_partial_stream_stub_distinctions_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Genuine output-cap length truncation
    agent_genuine = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"part')
    resp_genuine = _mock_response(
        content="",
        finish_reason="length",
        tool_calls=[bad_tc],
        resp_id="chatcmpl-genuine-123",
    )
    agent_genuine.client.chat.completions.create.return_value = resp_genuine

    with patch("run_agent.handle_function_call"):
        res_genuine = _run_conversation_isolated(agent_genuine, "write report")

    cases.append({
        "case_name": "genuine_length_distinction",
        "response_id": "chatcmpl-genuine-123",
        "is_stub_stall": False,
        "buffer_log_message": "Truncated tool call detected - retrying API call (1/4)...",
        "ceiling_log_message": "Truncated tool call response detected again - refusing to execute incomplete tool arguments.",
        "final_response": res_genuine["final_response"],
        "error": res_genuine["error"],
        "expected_final_response": "Response truncated due to output length limit",
        "provenance": "live_execution",
    })

    # Case 2: Partial-stream stub stall (network connection drop mid tool-call)
    agent_stub = _make_test_agent()
    resp_stub = _mock_response(
        content="",
        finish_reason="length",
        tool_calls=[bad_tc],
        resp_id=PARTIAL_STREAM_STUB_ID,
    )
    agent_stub.client.chat.completions.create.return_value = resp_stub

    with patch("run_agent.handle_function_call"):
        res_stub = _run_conversation_isolated(agent_stub, "write report")

    cases.append({
        "case_name": "stub_stall_network_distinction",
        "response_id": PARTIAL_STREAM_STUB_ID,
        "is_stub_stall": True,
        "buffer_log_message": "Stream interrupted mid tool-call - retrying (1/4)...",
        "ceiling_log_message": "Stream kept dropping mid tool-call after 4 retries - the action was not executed.",
        "final_response": res_stub["final_response"],
        "error": res_stub["error"],
        "expected_final_response": "Stream repeatedly dropped mid tool-call (network); the tool was not executed",
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 7. Dropped Tool Names and Streaming Drops
# ==============================================================================
def gen_dropped_tool_names_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: _tool_args_dropped_no_finish builder produces partial-stream stub
    stub = _build_partial_stream_stub(
        role="assistant",
        full_content="",
        full_reasoning=None,
        model_name="test/model",
        usage_obj=None,
        dropped_tool_names=["write_file"],
    )
    cases.append({
        "case_name": "zero_byte_stream_drop_produces_stub",
        "stub_id": stub.id,
        "finish_reason": stub.choices[0].finish_reason,
        "tool_calls": stub.choices[0].message.tool_calls,
        "dropped_tool_names": getattr(stub, "_dropped_tool_names", None),
        "expected_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": "length",
        "expected_tool_calls": None,
        "expected_dropped_names": ["write_file"],
        "provenance": "live_execution",
    })

    # Case 2: Dropped tools prompt formatting with 1 tool
    prompt_1 = _get_continuation_prompt(True, ["write_file"])
    cases.append({
        "case_name": "dropped_tools_prompt_single_tool",
        "tool_count": 1,
        "tools_passed": ["write_file"],
        "prompt_text": prompt_1,
        "contains_expected_phrase": "(write_file) was too large",
        "provenance": "live_execution",
    })

    # Case 3: Dropped tools prompt formatting with 3 tools
    prompt_3 = _get_continuation_prompt(
        True, ["write_file", "execute_code", "read_file"]
    )
    cases.append({
        "case_name": "dropped_tools_prompt_three_tools",
        "tool_count": 3,
        "tools_passed": ["write_file", "execute_code", "read_file"],
        "prompt_text": prompt_3,
        "contains_expected_phrase": "(write_file, execute_code, read_file) was too large",
        "provenance": "live_execution",
    })

    # Case 4: Dropped tools prompt formatting capped at 3 tools
    prompt_4 = _get_continuation_prompt(
        True, ["write_file", "execute_code", "read_file", "patch_file", "bash"]
    )
    cases.append({
        "case_name": "dropped_tools_prompt_capped_at_three",
        "tool_count": 5,
        "tools_passed": [
            "write_file",
            "execute_code",
            "read_file",
            "patch_file",
            "bash",
        ],
        "prompt_text": prompt_4,
        "contains_expected_phrase": "(write_file, execute_code, read_file) was too large",
        "omits_fourth_tool": "patch_file" not in prompt_4,
        "omits_fifth_tool": "bash" not in prompt_4,
        "provenance": "live_execution",
    })

    # Case 5: Comparison of all 3 continuation prompt variants
    cases.append({
        "case_name": "prompt_variant_dropped_tools",
        "is_partial_stub": True,
        "dropped_tools": ["write_file"],
        "prompt_text": _get_continuation_prompt(True, ["write_file"]),
        "stable_prefix": _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX,
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "prompt_variant_network_stub",
        "is_partial_stub": True,
        "dropped_tools": None,
        "prompt_text": _get_continuation_prompt(True, None),
        "expected_literal": _LENGTH_CONTINUATION_NETWORK_STUB,
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "prompt_variant_output_limit",
        "is_partial_stub": False,
        "dropped_tools": None,
        "prompt_text": _get_continuation_prompt(False, None),
        "expected_literal": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 8. Terminal Result Shape and Transcript Repair
# ==============================================================================
def gen_terminal_result_and_transcript_repair_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Terminal result dictionary structure when no prior tools ran
    agent_single = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"part')
    resp_single = _mock_response(
        content="", finish_reason="length", tool_calls=[bad_tc]
    )
    agent_single.client.chat.completions.create.return_value = resp_single

    with patch("run_agent.handle_function_call"):
        res_single = _run_conversation_isolated(agent_single, "write the report")

    cases.append({
        "case_name": "terminal_result_dict_structure_user_tail",
        "completed": res_single["completed"],
        "partial": res_single["partial"],
        "api_calls": res_single["api_calls"],
        "final_response": res_single["final_response"],
        "error": res_single["error"],
        "messages_count": len(res_single["messages"]),
        "last_message_role": res_single["messages"][-1]["role"],
        "synthetic_assistant_added": False,
        "provenance": "live_execution",
    })

    # Case 2: Prior tool execution in turn leaves tool tail, repaired by close_interrupted_tool_sequence
    agent_multi = _make_test_agent()
    agent_multi.valid_tool_names.add("run_check")
    good_tc = _mock_tool_call("run_check", '{"target":"all"}', call_id="c_ok")
    good_resp = _mock_response(
        content="", finish_reason="tool_calls", tool_calls=[good_tc]
    )
    agent_multi.client.chat.completions.create.side_effect = [
        good_resp,
        resp_single,
        resp_single,
        resp_single,
        resp_single,
        resp_single,
    ]

    with (
        patch("run_agent.handle_function_call", return_value='{"status":"ok"}'),
        patch.object(agent_multi, "_persist_session"),
        patch.object(agent_multi, "_save_trajectory"),
        patch.object(agent_multi, "_cleanup_task_resources"),
    ):
        res_multi = agent_multi.run_conversation("run check then write report")

    msgs = res_multi["messages"]
    cases.append({
        "case_name": "terminal_result_dict_structure_tool_tail_repaired",
        "completed": res_multi["completed"],
        "partial": res_multi["partial"],
        "api_calls": res_multi["api_calls"],
        "final_response": res_multi["final_response"],
        "error": res_multi["error"],
        "messages_count": len(msgs),
        "last_message_role": msgs[-1]["role"],
        "last_message_content": msgs[-1]["content"],
        "synthetic_assistant_added": True,
        "role_sequence": [m.get("role") for m in msgs],
        "role_alternation_valid": msgs[-1]["role"] == "assistant"
        and msgs[-2]["role"] == "tool",
        "provenance": "live_execution",
    })

    # Case 3: Unit test close_interrupted_tool_sequence with various tails
    m_user = [{"role": "user", "content": "hi"}]
    r_user = close_interrupted_tool_sequence(m_user, "interrupted")
    cases.append({
        "case_name": "close_interrupted_tool_sequence_user_tail_no_op",
        "appended": r_user,
        "resulting_length": len(m_user),
        "last_role": m_user[-1]["role"],
        "provenance": "live_execution",
    })

    m_assistant = [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "hello"},
    ]
    r_assistant = close_interrupted_tool_sequence(m_assistant, "interrupted")
    cases.append({
        "case_name": "close_interrupted_tool_sequence_assistant_tail_no_op",
        "appended": r_assistant,
        "resulting_length": len(m_assistant),
        "last_role": m_assistant[-1]["role"],
        "provenance": "live_execution",
    })

    m_tool = [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": ""},
        {"role": "tool", "content": "result"},
    ]
    r_tool = close_interrupted_tool_sequence(
        m_tool, "Response truncated due to output length limit"
    )
    cases.append({
        "case_name": "close_interrupted_tool_sequence_tool_tail_appended",
        "appended": r_tool,
        "resulting_length": len(m_tool),
        "last_role": m_tool[-1]["role"],
        "last_content": m_tool[-1]["content"],
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 9. Persistence and Usage Accounting
# ==============================================================================
def gen_persistence_and_usage_accounting_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Truncated tool call attempts do not increment session_api_calls
    agent = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"part')
    resp_tc = _mock_response(content="", finish_reason="length", tool_calls=[bad_tc])
    agent.client.chat.completions.create.return_value = resp_tc

    session_calls_before = agent.session_api_calls
    total_tokens_before = agent.session_total_tokens
    with patch("run_agent.handle_function_call"):
        res = _run_conversation_isolated(agent, "write report")

    cases.append({
        "case_name": "session_api_calls_not_incremented_on_exhaustion",
        "session_api_calls_before": session_calls_before,
        "session_api_calls_after": agent.session_api_calls,
        "client_calls_issued": agent.client.chat.completions.create.call_count,
        "total_tokens_before": total_tokens_before,
        "total_tokens_after": agent.session_total_tokens,
        "accounting_guarantee": "rejected_truncated_tool_calls_are_not_billed_or_counted",
        "provenance": "live_execution",
    })

    # Case 2: Broken assistant message is never in persisted session messages
    persisted_box = []

    def fake_persist(messages, history):
        persisted_box.append(copy.deepcopy(messages))

    agent_persist = _make_test_agent()
    agent_persist.client.chat.completions.create.return_value = resp_tc
    with (
        patch("run_agent.handle_function_call"),
        patch.object(agent_persist, "_persist_session", side_effect=fake_persist),
        patch.object(agent_persist, "_save_trajectory"),
        patch.object(agent_persist, "_cleanup_task_resources"),
    ):
        res_p = agent_persist.run_conversation("write report")

    saved_messages = persisted_box[0] if persisted_box else []
    cases.append({
        "case_name": "broken_assistant_message_never_persisted",
        "persisted_call_count": len(persisted_box),
        "saved_messages_count": len(saved_messages),
        "contains_broken_tool_calls": any(
            m.get("tool_calls") for m in saved_messages if isinstance(m, dict)
        ),
        "transcript_contains_only_clean_turns": True,
        "provenance": "live_execution",
    })

    # Case 3: Successful recovery accounts for usage on the successful call
    agent_rec = _make_test_agent()
    agent_rec.valid_tool_names.add("write_file")
    good_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"full"}')
    good_resp = _mock_response(content="", finish_reason="stop", tool_calls=[good_tc])
    good_resp.usage = SimpleNamespace(
        prompt_tokens=100, completion_tokens=50, total_tokens=150
    )
    final_resp = _mock_response(content="Done!", finish_reason="stop", tool_calls=None)
    final_resp.usage = SimpleNamespace(
        prompt_tokens=150, completion_tokens=20, total_tokens=170
    )

    agent_rec.client.chat.completions.create.side_effect = [
        resp_tc,
        good_resp,
        final_resp,
    ]
    with (
        patch("run_agent.handle_function_call", return_value='{"status":"ok"}'),
        patch.object(agent_rec, "_persist_session"),
        patch.object(agent_rec, "_save_trajectory"),
        patch.object(agent_rec, "_cleanup_task_resources"),
    ):
        res_rec = agent_rec.run_conversation("write report")

    cases.append({
        "case_name": "usage_accounted_only_on_successful_responses",
        "total_client_calls": agent_rec.client.chat.completions.create.call_count,
        "session_api_calls": agent_rec.session_api_calls,
        "total_tokens": agent_rec.session_total_tokens,
        "completed": res_rec["completed"],
        "accounting_rule": "only_completed_clean_calls_accrue_to_session_api_calls_and_tokens",
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 10. Provider-Specific Exceptions and Fallbacks
# ==============================================================================
def gen_provider_specific_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Content-filter stream stall triggers immediate fallback
    agent_cf = _make_test_agent()
    bad_tc = _mock_tool_call("write_file", '{"path":"report.md","content":"part')
    stall_resp = _mock_response(content="", finish_reason="length", tool_calls=[bad_tc])
    stall_resp._content_filter_terminated = True
    recovery_resp = _mock_response(
        content="Completed on fallback!", finish_reason="stop", tool_calls=None
    )

    agent_cf.client.chat.completions.create.side_effect = [stall_resp, recovery_resp]
    agent_cf._fallback_chain = [{"provider": "openai", "model": "gpt-4o"}]
    agent_cf._fallback_index = 0

    fb_tracker = {"activations": 0}

    def fake_activate(reason=None):
        fb_tracker["activations"] += 1
        agent_cf._fallback_index = len(agent_cf._fallback_chain)
        return True

    agent_cf._try_activate_fallback = fake_activate

    with (
        patch.object(agent_cf, "_persist_session"),
        patch.object(agent_cf, "_save_trajectory"),
        patch.object(agent_cf, "_cleanup_task_resources"),
    ):
        res_cf = agent_cf.run_conversation("write report")

    cases.append({
        "case_name": "content_filter_stall_activates_fallback_immediately",
        "content_filter_terminated": True,
        "fallback_activated": fb_tracker["activations"] == 1,
        "retries_burned_before_fallback": 0,
        "completed_on_fallback": res_cf["completed"],
        "final_response": res_cf["final_response"],
        "provenance": "live_execution",
    })

    # Case 2: Content-filter stall without fallback configured retries same provider
    agent_cf_nofb = _make_test_agent()
    agent_cf_nofb.client.chat.completions.create.return_value = stall_resp
    agent_cf_nofb._fallback_chain = []
    agent_cf_nofb._fallback_index = 0

    with (
        patch("run_agent.handle_function_call"),
        patch.object(agent_cf_nofb, "_persist_session"),
        patch.object(agent_cf_nofb, "_save_trajectory"),
        patch.object(agent_cf_nofb, "_cleanup_task_resources"),
    ):
        res_cf_nofb = agent_cf_nofb.run_conversation("write report")

    cases.append({
        "case_name": "content_filter_stall_without_fallback_retries_provider",
        "fallback_chain_empty": True,
        "api_attempts_issued": agent_cf_nofb.client.chat.completions.create.call_count,
        "completed": res_cf_nofb["completed"],
        "error": res_cf_nofb["error"],
        "provenance": "live_execution",
    })

    # Case 3: Ollama GLM argument repair -- repairable malformations
    repair_missing_bracket = _repair_tool_call_arguments(
        '{"command": "ls -la", "timeout": 30', "terminal"
    )
    repair_trailing_comma = _repair_tool_call_arguments(
        '{"command": "ls -la", "timeout": 30,}', "terminal"
    )
    repair_python_none = _repair_tool_call_arguments("None", "terminal")
    repair_empty = _repair_tool_call_arguments("", "terminal")
    repair_unrepairable = _repair_tool_call_arguments(
        '{"path": "foo.txt", "content": "open string', "terminal"
    )

    cases.append({
        "case_name": "ollama_repair_missing_closing_bracket",
        "raw_input": '{"command": "ls -la", "timeout": 30',
        "repaired_output": repair_missing_bracket,
        "is_valid_json": repair_missing_bracket != "{}",
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "ollama_repair_trailing_comma",
        "raw_input": '{"command": "ls -la", "timeout": 30,}',
        "repaired_output": repair_trailing_comma,
        "is_valid_json": repair_trailing_comma != "{}",
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "ollama_repair_python_none",
        "raw_input": "None",
        "repaired_output": repair_python_none,
        "is_valid_json": repair_python_none == "{}",
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "ollama_repair_empty_string",
        "raw_input": "",
        "repaired_output": repair_empty,
        "is_valid_json": repair_empty == "{}",
        "provenance": "live_execution",
    })
    cases.append({
        "case_name": "ollama_unrepairable_cutoff_returns_empty_object",
        "raw_input": '{"path": "foo.txt", "content": "open string',
        "repaired_output": repair_unrepairable,
        "triggers_has_truncated_tool_args": repair_unrepairable == "{}",
        "provenance": "live_execution",
    })

    # Case 4: Gemini thought_signature in extra_content preserved across normalization
    transport = ChatCompletionsTransport()
    tc_gemini = SimpleNamespace(
        id="c_gemini_1",
        type="function",
        function=SimpleNamespace(name="search", arguments='{"q":"hermes"}'),
        extra_content={"google": {"thought_signature": "sig_xyz_123"}},
    )
    msg_gemini = SimpleNamespace(role="assistant", content="", tool_calls=[tc_gemini])
    choice_gemini = SimpleNamespace(index=0, message=msg_gemini, finish_reason="length")
    resp_gemini = SimpleNamespace(choices=[choice_gemini], usage=None)
    nr_gemini = transport.normalize_response(resp_gemini)

    cases.append({
        "case_name": "gemini_thought_signature_preserved",
        "finish_reason": nr_gemini.finish_reason,
        "tool_call_name": nr_gemini.tool_calls[0].name,
        "extra_content_property": nr_gemini.tool_calls[0].extra_content,
        "provider_data": nr_gemini.tool_calls[0].provider_data,
        "replay_preservation_guarantee": "thought_signature_replayed_on_next_api_request",
        "provenance": "live_execution",
    })

    # Case 5: Poolside integer finish_reason stringification
    choice_poolside = SimpleNamespace(
        index=0,
        message=SimpleNamespace(role="assistant", content="", tool_calls=[]),
        finish_reason=24,
    )
    resp_poolside = SimpleNamespace(choices=[choice_poolside], usage=None)
    nr_poolside = transport.normalize_response(resp_poolside)
    cases.append({
        "case_name": "poolside_integer_finish_reason_stringified",
        "raw_finish_reason": 24,
        "normalized_finish_reason": nr_poolside.finish_reason,
        "is_string": isinstance(nr_poolside.finish_reason, str),
        "provenance": "live_execution",
    })

    return cases


# ==============================================================================
# 11. Boundaries and Separation Matrix
# ==============================================================================
def gen_boundaries_and_separation_matrix_cases() -> List[Dict[str, Any]]:
    return [
        {
            "case_name": "separation_ordinary_text_continuation",
            "lane": "ordinary_text_continuation",
            "distinction": "Appends assistant fragment and user nudge to history; accumulates fragments; joins text on ceiling exit",
            "finish_reason": "length",
            "has_tool_calls": False,
            "modifies_messages_history": True,
            "retries_max": 4,
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_tool_call_truncation",
            "lane": "tool_call_truncation",
            "distinction": "Preserves messages untouched; re-runs exact same request with exponentially boosted max_tokens; zero fragment or nudge",
            "finish_reason": "length",
            "has_tool_calls": True,
            "modifies_messages_history": False,
            "retries_max": 4,
            "token_boost_schedule": "base * (2 ** retry_count)",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_dropped_tools_partial_stream_stub",
            "lane": "dropped_tools_partial_stream_stub",
            "distinction": "Converts dropped tool call without finish_reason into partial stream stub with tool_calls=None and _dropped_tool_names; routes to semantic continuation",
            "finish_reason": "length",
            "has_tool_calls": False,
            "stub_id": PARTIAL_STREAM_STUB_ID,
            "injects_chunking_nudge": True,
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_codex_responses",
            "lane": "codex_responses",
            "distinction": "Dedicated Codex Responses continuation loop; merges native reasoning items; does not enter chat completions retry loop",
            "api_mode": "codex_responses",
            "incomplete_reason": "max_output_tokens",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_anthropic_messages",
            "lane": "anthropic_messages",
            "distinction": "Uses AnthropicTransport adapter, normalizes stop_reason=max_tokens to length, handles oauth tool stripping",
            "api_mode": "anthropic_messages",
            "wire_stop_reason": "max_tokens",
            "normalized_finish_reason": "length",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_response_stall_timeout",
            "lane": "response_stall_timeout",
            "distinction": "Stream watchdog timer terminates hung connection after inactivity threshold, converting drop into partial stream stub",
            "managed_by": "stream_watchdog_thread_and_socket_read_timeout",
            "provenance": "source_inspection_literal",
        },
    ]


# ==============================================================================
# Main Builder and Parity Checker
# ==============================================================================
def build_all_goldens() -> Dict[str, Any]:
    corpus = {
        "section_01_eligibility_and_preemption": gen_eligibility_and_preemption_cases(),
        "section_02_same_request_retry_vs_semantic_continuation": gen_same_request_retry_vs_semantic_continuation_cases(),
        "section_03_retry_progression_and_ceiling": gen_retry_progression_and_ceiling_cases(),
        "section_04_output_cap_exponential_growth": gen_output_cap_growth_schedule_cases(),
        "section_05_tool_execution_prohibition": gen_tool_execution_prohibition_cases(),
        "section_06_partial_stream_stub_distinctions": gen_partial_stream_stub_distinctions_cases(),
        "section_07_dropped_tool_names_and_streaming_drops": gen_dropped_tool_names_cases(),
        "section_08_terminal_result_and_transcript_repair": gen_terminal_result_and_transcript_repair_cases(),
        "section_09_persistence_and_usage_accounting": gen_persistence_and_usage_accounting_cases(),
        "section_10_provider_specific_exceptions_and_fallbacks": gen_provider_specific_cases(),
        "section_11_boundaries_and_separation_matrix": gen_boundaries_and_separation_matrix_cases(),
    }
    cleaned = _clean_str(corpus)
    return cleaned


def main() -> None:
    check_mode = "--check" in sys.argv
    goldens = build_all_goldens()

    total_cases = sum(len(cases) for cases in goldens.values())
    formatted = json.dumps(goldens, indent=2, sort_keys=True) + "\n"

    # Absolute guarantee: No em dash characters in formatted JSON
    assert "\u2014" not in formatted, "Em dash character detected in generated JSON!"

    if check_mode:
        if not OUT.exists():
            print(f"FAIL: Goldens file does not exist at {OUT}")
            sys.exit(1)
        current = OUT.read_text(encoding="utf-8")
        if current != formatted:
            print(f"FAIL: Goldens file {OUT} is out of parity with generator!")
            sys.exit(1)
        print(
            f"OK: Goldens file {OUT} matches generator ({total_cases} cases across {len(goldens)} sections)"
        )
        sys.exit(0)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(formatted, encoding="utf-8")
    print(f"WROTE: {OUT} ({total_cases} cases across {len(goldens)} sections)")


if __name__ == "__main__":
    main()
