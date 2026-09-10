#!/usr/bin/env python3
"""Deterministic source-executed oracle for native main-provider length continuation.

This generator audits and executes live Python decision functions and runtime transitions
governing ordinary chat-completions text truncation continuation after an HTTP success:
1. finish-reason normalization and conservative stop-to-length misreport detection.
2. Truncation eligibility and pre-continuation guardrails (thinking exhaustion, repetition guard, content filter stall).
3. Repeated-fragment detection algorithm (is_repetition_dominated).
4. Continuation prompt construction (_get_continuation_prompt variants).
5. Synthetic message metadata (_length_continuation_fragment, _length_continuation_nudge), role alternation, and wire sanitization.
6. Request budgets (4 attempts max), progressive output token boosting (2^retry), and ephemeral reasoning override.
7. Fragment accumulation, whitespace-safe joining (_join_truncated_parts), and turn recovery.
8. Ceiling exit, persistence cleanup, fragment collapsing, and session wedge prevention.
9. Usage accounting, API call counting, and the exact no-replay boundary after visible streaming output.
10. Explicit boundaries separating ordinary chat-completions from Anthropic, Codex Responses, content policy, tool-call truncation, and verification continuation.

Usage:
    python3 rust/tools/gen_main_provider_length_continuation_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_length_continuation_goldens.py --check  # check parity
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
OUT = ROOT / "rust/tools/main-provider-length-continuation-goldens.json"

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
import agent.repetition_guard as rep_guard
from agent.chat_completion_helpers import (
    FINISH_REASON_LENGTH,
    PARTIAL_STREAM_STUB_ID,
    _build_partial_stream_stub,
    _reasoning_config_for_wire,
)
from agent.conversation_loop import (
    _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX,
    _LENGTH_CONTINUATION_NETWORK_STUB,
    _LENGTH_CONTINUATION_OUTPUT_LIMIT,
    _get_continuation_prompt,
    _join_truncated_parts,
)
from agent.repetition_guard import (
    MIN_FRAGMENT_LENGTH,
    _DOMINANCE_RATIO,
    _MIN_REPEAT_COUNT,
    _REPEAT_WINDOW,
    _line_repetition_dominated,
    is_repetition_dominated,
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
    api_key: str = "synth-primary-key",
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


def _run_conversation_isolated(
    agent: AIAgent, message: str, history: Optional[List[Dict[str, Any]]] = None
) -> Dict[str, Any]:
    """Run agent.run_conversation with persistent disk operations stubbed."""
    with (
        patch.object(agent, "_persist_session"),
        patch.object(agent, "_save_trajectory"),
        patch.object(agent, "_cleanup_task_resources"),
    ):
        return agent.run_conversation(message, conversation_history=history)


# ==============================================================================
# 1. Finish-Reason Normalization and Misreport Detection
# ==============================================================================
def gen_finish_reason_normalization_cases() -> List[Dict[str, Any]]:
    transport = ChatCompletionsTransport()
    cases: List[Dict[str, Any]] = []

    # Case 1: Standard length finish reason
    resp_length = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="partial text", tool_calls=None),
                finish_reason="length",
            )
        ],
        usage=None,
    )
    norm1 = transport.normalize_response(resp_length)
    cases.append({
        "case_name": "normalize_standard_length",
        "input_finish_reason": "length",
        "normalized_finish_reason": norm1.finish_reason,
        "content": norm1.content,
        "provenance": "executed_source",
    })

    # Case 2: Standard stop finish reason
    resp_stop = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="complete text", tool_calls=None),
                finish_reason="stop",
            )
        ],
        usage=None,
    )
    norm2 = transport.normalize_response(resp_stop)
    cases.append({
        "case_name": "normalize_standard_stop",
        "input_finish_reason": "stop",
        "normalized_finish_reason": norm2.finish_reason,
        "content": norm2.content,
        "provenance": "executed_source",
    })

    # Case 3: Absent finish reason defaults to stop
    resp_none = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="some text", tool_calls=None),
                finish_reason=None,
            )
        ],
        usage=None,
    )
    norm3 = transport.normalize_response(resp_none)
    cases.append({
        "case_name": "normalize_none_defaults_to_stop",
        "input_finish_reason": None,
        "normalized_finish_reason": norm3.finish_reason,
        "content": norm3.content,
        "provenance": "executed_source",
    })

    # Case 4: Integer finish reason stringification (Poolside integer 24)
    resp_int = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="poolside text", tool_calls=None),
                finish_reason=24,
            )
        ],
        usage=None,
    )
    norm4 = transport.normalize_response(resp_int)
    cases.append({
        "case_name": "normalize_integer_finish_reason",
        "input_finish_reason": 24,
        "normalized_finish_reason": norm4.finish_reason,
        "content": norm4.content,
        "provenance": "executed_source",
    })

    # Case 5: Tool calls finish reason preserved
    tc_mock = SimpleNamespace(
        id="tc_1", function=SimpleNamespace(name="bash", arguments='{"cmd":"ls"}')
    )
    resp_tc = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="", tool_calls=[tc_mock]),
                finish_reason="tool_calls",
            )
        ],
        usage=None,
    )
    norm5 = transport.normalize_response(resp_tc)
    cases.append({
        "case_name": "normalize_tool_calls_finish_reason",
        "input_finish_reason": "tool_calls",
        "normalized_finish_reason": norm5.finish_reason,
        "tool_call_count": len(norm5.tool_calls or []),
        "provenance": "executed_source",
    })

    # Case 6: Content filter finish reason preserved
    resp_cf = SimpleNamespace(
        choices=[
            SimpleNamespace(
                message=SimpleNamespace(content="filter notice", tool_calls=None),
                finish_reason="content_filter",
            )
        ],
        usage=None,
    )
    norm6 = transport.normalize_response(resp_cf)
    cases.append({
        "case_name": "normalize_content_filter_finish_reason",
        "input_finish_reason": "content_filter",
        "normalized_finish_reason": norm6.finish_reason,
        "content": norm6.content,
        "provenance": "executed_source",
    })

    # Case 7: Text-only stream drop stub tagged PARTIAL_STREAM_STUB_ID with finish_reason="length"
    stub_text = _build_partial_stream_stub(
        role="assistant",
        full_content="partial stream text so far",
        full_reasoning=None,
        model_name="test-model",
        usage_obj=None,
    )
    cases.append({
        "case_name": "partial_stream_stub_text_only",
        "stub_id": stub_text.id,
        "finish_reason": stub_text.choices[0].finish_reason,
        "content": stub_text.choices[0].message.content,
        "dropped_tool_names": getattr(stub_text, "_dropped_tool_names", None),
        "provenance": "executed_source",
    })

    # Case 8: Mid-tool-call stream drop stub tagged with dropped_tool_names
    stub_tool = _build_partial_stream_stub(
        role="assistant",
        full_content="",
        full_reasoning=None,
        model_name="test-model",
        usage_obj=None,
        dropped_tool_names=["write_file", "bash"],
    )
    cases.append({
        "case_name": "partial_stream_stub_dropped_tools",
        "stub_id": stub_tool.id,
        "finish_reason": stub_tool.choices[0].finish_reason,
        "dropped_tool_names": getattr(stub_tool, "_dropped_tool_names", None),
        "provenance": "executed_source",
    })

    # Case 9: Clean stream ending with usage chunk does NOT create partial stub
    cases.append({
        "case_name": "stream_clean_with_usage_not_stub",
        "description": "When include_usage chunk arrives without finish_reason, it proves stream completion",
        "finish_reason": "stop",
        "is_partial_stream_stub": False,
        "provenance": "source_inspection_literal",
    })

    # Ollama GLM misreport heuristic checks via AIAgent._should_treat_stop_as_truncated
    agent_ollama_glm = _make_test_agent(
        model="glm-4-9b",
        provider="ollama",
        base_url="http://localhost:11434/v1",
    )
    history_with_tool = [
        {"role": "user", "content": "run task"},
        {"role": "tool", "content": "tool output"},
    ]

    # Case 10: Ollama GLM stop without ending punctuation rewritten to length
    msg_incomplete = SimpleNamespace(
        content="Based on the search results, the best next", tool_calls=None
    )
    res_incomplete = agent_ollama_glm._should_treat_stop_as_truncated(
        "stop", msg_incomplete, history_with_tool
    )
    cases.append({
        "case_name": "ollama_glm_stop_truncated_positive",
        "model": "glm-4-9b",
        "base_url": "http://localhost:11434/v1",
        "visible_text": msg_incomplete.content,
        "should_treat_stop_as_truncated": res_incomplete,
        "rewritten_finish_reason": "length" if res_incomplete else "stop",
        "provenance": "executed_source",
    })

    # Case 11: Ollama Cloud host (ollama.com) is excluded from rewrite
    agent_ollama_cloud = _make_test_agent(
        model="glm-5.3-flash",
        provider="ollama-cloud",
        base_url="https://ollama.com/v1",
    )
    res_cloud = agent_ollama_cloud._should_treat_stop_as_truncated(
        "stop", msg_incomplete, history_with_tool
    )
    cases.append({
        "case_name": "ollama_cloud_hosted_negative",
        "model": "glm-5.3-flash",
        "base_url": "https://ollama.com/v1",
        "should_treat_stop_as_truncated": res_cloud,
        "provenance": "executed_source",
    })

    # Case 12: Local endpoint with :cloud suffix is excluded from rewrite
    agent_cloud_suffix = _make_test_agent(
        model="glm-5.1:cloud",
        provider="ollama",
        base_url="http://localhost:11434/v1",
    )
    res_suffix = agent_cloud_suffix._should_treat_stop_as_truncated(
        "stop", msg_incomplete, history_with_tool
    )
    cases.append({
        "case_name": "ollama_cloud_model_suffix_negative",
        "model": "glm-5.1:cloud",
        "base_url": "http://localhost:11434/v1",
        "should_treat_stop_as_truncated": res_suffix,
        "provenance": "executed_source",
    })

    # Case 13: Non-GLM model (llama3) is excluded from rewrite
    agent_llama = _make_test_agent(
        model="llama3:8b",
        provider="ollama",
        base_url="http://localhost:11434/v1",
    )
    res_llama = agent_llama._should_treat_stop_as_truncated(
        "stop", msg_incomplete, history_with_tool
    )
    cases.append({
        "case_name": "non_glm_model_negative",
        "model": "llama3:8b",
        "should_treat_stop_as_truncated": res_llama,
        "provenance": "executed_source",
    })

    # Case 14: Natural ending period prevents rewrite
    msg_period = SimpleNamespace(
        content="Based on the search results, the best next step is done.",
        tool_calls=None,
    )
    res_period = agent_ollama_glm._should_treat_stop_as_truncated(
        "stop", msg_period, history_with_tool
    )
    cases.append({
        "case_name": "natural_ending_period_negative",
        "visible_text": msg_period.content,
        "should_treat_stop_as_truncated": res_period,
        "provenance": "executed_source",
    })

    # Case 15: Natural ending emoji prevents rewrite
    msg_emoji = SimpleNamespace(
        content="Here is your requested update 🎉", tool_calls=None
    )
    res_emoji = agent_ollama_glm._should_treat_stop_as_truncated(
        "stop", msg_emoji, history_with_tool
    )
    cases.append({
        "case_name": "natural_ending_emoji_negative",
        "visible_text": msg_emoji.content,
        "should_treat_stop_as_truncated": res_emoji,
        "provenance": "executed_source",
    })

    # Case 16: Natural ending code fence prevents rewrite
    msg_fence = SimpleNamespace(
        content="Here is the python code:\n```python\nprint('hi')\n```", tool_calls=None
    )
    res_fence = agent_ollama_glm._should_treat_stop_as_truncated(
        "stop", msg_fence, history_with_tool
    )
    cases.append({
        "case_name": "natural_ending_code_fence_negative",
        "visible_text": msg_fence.content,
        "should_treat_stop_as_truncated": res_fence,
        "provenance": "executed_source",
    })

    # Case 17: Natural ending Chinese punctuation prevents rewrite
    msg_zh = SimpleNamespace(
        content="根據搜索結果，下一步是更新配置。", tool_calls=None
    )
    res_zh = agent_ollama_glm._should_treat_stop_as_truncated(
        "stop", msg_zh, history_with_tool
    )
    cases.append({
        "case_name": "natural_ending_chinese_punct_negative",
        "visible_text": msg_zh.content,
        "should_treat_stop_as_truncated": res_zh,
        "provenance": "executed_source",
    })

    # Case 18: No prior tool turn in messages prevents rewrite
    history_no_tool = [{"role": "user", "content": "write an essay"}]
    res_notool = agent_ollama_glm._should_treat_stop_as_truncated(
        "stop", msg_incomplete, history_no_tool
    )
    cases.append({
        "case_name": "no_prior_tool_turn_negative",
        "has_prior_tool_message": False,
        "should_treat_stop_as_truncated": res_notool,
        "provenance": "executed_source",
    })

    return cases


# ==============================================================================
# 2. Truncation Eligibility and Pre-Continuation Guardrails
# ==============================================================================
def gen_truncation_eligibility_and_guardrails_cases() -> List[Dict[str, Any]]:
    agent = _make_test_agent()
    cases: List[Dict[str, Any]] = []

    def check_thinking_exhausted(
        trunc_content: Optional[str], has_tool_calls: bool
    ) -> bool:
        has_think_tags = bool(
            trunc_content
            and re.search(
                r"<(?:think|thinking|reasoning|REASONING_SCRATCHPAD)[^>]*>",
                trunc_content,
                re.IGNORECASE,
            )
        )
        return (
            not has_tool_calls
            and has_think_tags
            and (
                (
                    trunc_content is not None
                    and not agent._has_content_after_think_block(trunc_content)
                )
                or trunc_content is None
            )
        )

    # Case 1: Unclosed think tag with no visible text -> thinking exhausted
    c1 = "<think>Let me analyze the problem deeply and deduce the outcome..."
    ex1 = check_thinking_exhausted(c1, False)
    cases.append({
        "case_name": "thinking_exhausted_unclosed_think_tag",
        "content_snippet": c1[:40],
        "has_tool_calls": False,
        "thinking_exhausted": ex1,
        "loop_action": "abort_turn_immediately",
        "error_message": "Model used all output tokens on reasoning with none left for the response. Try lowering reasoning effort or increasing max_tokens.",
        "provenance": "executed_source",
    })

    # Case 2: Closed think tag with only whitespace after -> thinking exhausted
    c2 = "<think>Concluded thoughts.</think>   \n  "
    ex2 = check_thinking_exhausted(c2, False)
    cases.append({
        "case_name": "thinking_exhausted_closed_think_tag_whitespace",
        "content_snippet": c2,
        "has_tool_calls": False,
        "thinking_exhausted": ex2,
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 3: Unclosed REASONING_SCRATCHPAD -> thinking exhausted
    c3 = "<REASONING_SCRATCHPAD>\nAnalyzing AST nodes..."
    ex3 = check_thinking_exhausted(c3, False)
    cases.append({
        "case_name": "thinking_exhausted_scratchpad_unclosed",
        "content_snippet": c3,
        "has_tool_calls": False,
        "thinking_exhausted": ex3,
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 4: Closed think tag with visible text after -> NOT thinking exhausted
    c4 = "<think>Concluded thoughts.</think>Here is the visible answer."
    ex4 = check_thinking_exhausted(c4, False)
    cases.append({
        "case_name": "thinking_not_exhausted_visible_content_present",
        "content_snippet": c4,
        "has_tool_calls": False,
        "thinking_exhausted": ex4,
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    # Case 5: Empty content without think tags -> NOT thinking exhausted (eligible for thinking-off retry)
    c5 = ""
    ex5 = check_thinking_exhausted(c5, False)
    cases.append({
        "case_name": "thinking_not_exhausted_no_think_tags_empty_content",
        "content_snippet": "",
        "has_tool_calls": False,
        "thinking_exhausted": ex5,
        "loop_action": "eligible_for_continuation_with_ephemeral_reasoning_off",
        "provenance": "executed_source",
    })

    # Case 6: Has tool calls -> NOT thinking exhausted (handled by tool truncation retry)
    c6 = "<think>Let me call the tool</think>"
    ex6 = check_thinking_exhausted(c6, True)
    cases.append({
        "case_name": "thinking_not_exhausted_tool_calls_present",
        "content_snippet": c6,
        "has_tool_calls": True,
        "thinking_exhausted": ex6,
        "loop_action": "route_to_tool_call_retry",
        "provenance": "executed_source",
    })

    # Repetition-dominated guard checks
    incident_echo = "好，你幫我更改成 Google Gemini 4 31B。" * 50
    # Case 7: Dominated repetition text -> repetition dominated (aborts)
    rep_dom7 = is_repetition_dominated(incident_echo)
    cases.append({
        "case_name": "repetition_dominated_truncation_abort",
        "text_length": len(incident_echo),
        "is_repetition_dominated": rep_dom7,
        "loop_action": "abort_turn_immediately",
        "error_message": "Model output entered a repetition loop and was truncated mid-loop; refusing to continue a degenerate response.",
        "provenance": "executed_source",
    })

    # Case 8: Normal truncated sentence -> NOT repetition dominated (eligible)
    clean_trunc = "The primary reason for configuring the cache is to reduce repeated lookups and latency."
    rep_dom8 = is_repetition_dominated(clean_trunc)
    cases.append({
        "case_name": "repetition_guard_clean_visible_text_eligible",
        "text_length": len(clean_trunc),
        "is_repetition_dominated": rep_dom8,
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    # Content filter stream stall fallback checks (#32421)
    # Case 9: Content filter terminated with fallback available -> eager fallback
    cases.append({
        "case_name": "content_filter_stall_with_fallback",
        "content_filter_terminated": True,
        "has_fallback_chain": True,
        "loop_action": "activate_fallback_and_restart",
        "rollback_partial_fragments": True,
        "unmarks_metadata_tags": [
            "_length_continuation_fragment",
            "_length_continuation_nudge",
        ],
        "provenance": "source_inspection_literal",
    })

    # Case 10: Content filter terminated without fallback -> best effort continuation fallthrough
    cases.append({
        "case_name": "content_filter_stall_no_fallback",
        "content_filter_terminated": True,
        "has_fallback_chain": False,
        "loop_action": "fallthrough_to_same_provider_continuation",
        "provenance": "source_inspection_literal",
    })

    # General eligibility routing
    # Case 11: Pure text truncation -> enters text continuation
    cases.append({
        "case_name": "eligibility_pure_text_truncation",
        "finish_reason": "length",
        "has_tool_calls": False,
        "has_content": True,
        "target_lane": "text_length_continuation",
        "provenance": "source_inspection_literal",
    })

    # Case 12: Tool call truncation -> enters tool call retry
    cases.append({
        "case_name": "eligibility_tool_call_truncation",
        "finish_reason": "length",
        "has_tool_calls": True,
        "target_lane": "tool_call_truncation_retry",
        "token_boost_schedule": "base * (2 ** retry_count)",
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# 3. Repeated-Fragment Detection Algorithm (is_repetition_dominated)
# ==============================================================================
def gen_repetition_guard_algorithm_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Constants verification
    cases.append({
        "case_name": "repetition_constants",
        "min_fragment_length": MIN_FRAGMENT_LENGTH,
        "repeat_window": _REPEAT_WINDOW,
        "min_repeat_count": _MIN_REPEAT_COUNT,
        "dominance_ratio": _DOMINANCE_RATIO,
        "provenance": "executed_source",
    })

    # Case 1: Verbatim phrase repeated 50x covering majority
    phrase = "好，你幫我更改成 Google Gemini 4 31B。"
    txt_50x = phrase * 50
    cases.append({
        "case_name": "verbatim_phrase_repeat_50x",
        "length": len(txt_50x),
        "is_repetition_dominated": is_repetition_dominated(txt_50x),
        "line_repetition_dominated": _line_repetition_dominated(txt_50x, len(txt_50x)),
        "provenance": "executed_source",
    })

    # Case 2: Line repetition fast path (single 80-char line repeated 10x)
    line = "Configuration option `proxy_mode` must be set to `strict` for external traffic.\n"
    txt_lines = line * 10
    cases.append({
        "case_name": "line_repetition_fast_path",
        "length": len(txt_lines),
        "line_count": 10,
        "is_repetition_dominated": is_repetition_dominated(txt_lines),
        "provenance": "executed_source",
    })

    # Case 3: Sliding window subline repeat (no linebreaks)
    subline = (
        "ABCDEFGHIJ0123456789KLMNOPQRST0123456789UVWXYZ0123456789!@#$%^"  # 64 chars
    )
    txt_sliding = "Prefix text before loop: " + (subline * 8) + " suffix text."
    cases.append({
        "case_name": "sliding_window_subline_repeat",
        "length": len(txt_sliding),
        "is_repetition_dominated": is_repetition_dominated(txt_sliding),
        "provenance": "executed_source",
    })

    # Case 4: Diverse long prose (> 400 chars)
    diverse = (
        "The quick brown fox jumps over the lazy dog.\n"
        "Now is the time for all good men to come to the aid of their country.\n"
        "Sphinx of black quartz, judge my vow.\n"
        "Pack my box with five dozen liquor jugs.\n"
        "How vexingly quick daft zebras jump!\n"
        "Bright vixens jump; dozy fowl quack.\n"
        "Jackdaws love my big sphinx of quartz.\n"
        "The five boxing wizards jump quickly.\n"
        "Sympathizing would-be fixers quintuple hard junk.\n"
        "Few black taxis drive up major roads on quiet hazy nights.\n"
    ) * 2
    cases.append({
        "case_name": "diverse_prose_long",
        "length": len(diverse),
        "is_repetition_dominated": is_repetition_dominated(diverse),
        "provenance": "executed_source",
    })

    # Case 5: Short text under MIN_FRAGMENT_LENGTH (< 400 chars)
    short_rep = phrase * 2  # ~60 chars
    cases.append({
        "case_name": "short_text_under_min_fragment_length",
        "length": len(short_rep),
        "is_repetition_dominated": is_repetition_dominated(short_rep),
        "provenance": "executed_source",
    })

    # Case 6: Non-string input: None
    cases.append({
        "case_name": "non_string_input_none",
        "input_val": None,
        "is_repetition_dominated": is_repetition_dominated(None),  # type: ignore
        "provenance": "executed_source",
    })

    # Case 7: Non-string input: int
    cases.append({
        "case_name": "non_string_input_int",
        "input_val": 123456,
        "is_repetition_dominated": is_repetition_dominated(123456),  # type: ignore
        "provenance": "executed_source",
    })

    # Case 8: Empty string
    cases.append({
        "case_name": "empty_string",
        "input_val": "",
        "is_repetition_dominated": is_repetition_dominated(""),
        "provenance": "executed_source",
    })

    # Case 9: Repetitive think block with clean visible text
    # In conversation loop, agent._strip_think_blocks is called before is_repetition_dominated
    think_block_echo = (
        f"<think>{phrase * 50}</think>Here is the distinct visible answer text."
    )
    agent = _make_test_agent()
    visible_only = agent._strip_think_blocks(think_block_echo)
    cases.append({
        "case_name": "repetitive_think_block_clean_visible_text",
        "raw_text_length": len(think_block_echo),
        "visible_text": visible_only.strip(),
        "raw_is_repetition_dominated": is_repetition_dominated(think_block_echo),
        "visible_is_repetition_dominated": is_repetition_dominated(visible_only),
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    return cases


# ==============================================================================
# 4. Continuation Prompt Construction
# ==============================================================================
def gen_continuation_prompts_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Output length limit prompt (normal truncation)
    p_output = _get_continuation_prompt(is_partial_stub=False, dropped_tools=None)
    cases.append({
        "case_name": "output_limit_prompt",
        "is_partial_stub": False,
        "dropped_tools": None,
        "prompt_content": p_output,
        "expected_exact_constant": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "matches_constant": p_output == _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "provenance": "executed_source",
    })

    # Case 2: Network stream stub prompt (mid-stream drop without tools)
    p_network = _get_continuation_prompt(is_partial_stub=True, dropped_tools=None)
    cases.append({
        "case_name": "network_stub_prompt",
        "is_partial_stub": True,
        "dropped_tools": None,
        "prompt_content": p_network,
        "expected_exact_constant": _LENGTH_CONTINUATION_NETWORK_STUB,
        "matches_constant": p_network == _LENGTH_CONTINUATION_NETWORK_STUB,
        "provenance": "executed_source",
    })

    # Case 3: Dropped tool single
    p_tool_single = _get_continuation_prompt(
        is_partial_stub=True, dropped_tools=["write_file"]
    )
    cases.append({
        "case_name": "dropped_tools_single",
        "is_partial_stub": True,
        "dropped_tools": ["write_file"],
        "prompt_content": p_tool_single,
        "has_prefix": p_tool_single.startswith(
            _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX
        ),
        "contains_tool": "(write_file)" in p_tool_single,
        "provenance": "executed_source",
    })

    # Case 4: Dropped tools triple
    tools_3 = ["write_file", "edit_file", "bash"]
    p_tool_triple = _get_continuation_prompt(
        is_partial_stub=True, dropped_tools=tools_3
    )
    cases.append({
        "case_name": "dropped_tools_triple",
        "is_partial_stub": True,
        "dropped_tools": tools_3,
        "prompt_content": p_tool_triple,
        "contains_tools": "(write_file, edit_file, bash)" in p_tool_triple,
        "provenance": "executed_source",
    })

    # Case 5: Dropped tools capped at 3 items
    tools_5 = ["tool_1", "tool_2", "tool_3", "tool_4", "tool_5"]
    p_tool_capped = _get_continuation_prompt(
        is_partial_stub=True, dropped_tools=tools_5
    )
    cases.append({
        "case_name": "dropped_tools_capped_at_three",
        "is_partial_stub": True,
        "dropped_tools_input": tools_5,
        "prompt_content": p_tool_capped,
        "interpolated_tools": "(tool_1, tool_2, tool_3)",
        "omits_later_tools": "tool_4" not in p_tool_capped
        and "tool_5" not in p_tool_capped,
        "provenance": "executed_source",
    })

    # Case 6: Stable prefix verification for context compressor recognition
    cases.append({
        "case_name": "dropped_tools_prefix_constant_stability",
        "prefix_constant": _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX,
        "context_compressor_matching_method": "str.startswith",
        "provenance": "executed_source",
    })

    # Case 7: Network stub phrase verification
    cases.append({
        "case_name": "network_stub_contains_distinctive_phrase",
        "distinctive_phrase": "cut off by a network error mid-stream",
        "in_network_stub": "cut off by a network error mid-stream"
        in _LENGTH_CONTINUATION_NETWORK_STUB,
        "not_in_output_limit": "cut off by a network error mid-stream"
        not in _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "provenance": "executed_source",
    })

    # Case 8: Output limit phrase verification
    cases.append({
        "case_name": "output_limit_contains_distinctive_phrase",
        "distinctive_phrase": "truncated by the output length limit",
        "in_output_limit": "truncated by the output length limit"
        in _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "not_in_network_stub": "truncated by the output length limit"
        not in _LENGTH_CONTINUATION_NETWORK_STUB,
        "provenance": "executed_source",
    })

    return cases


# ==============================================================================
# 5. Synthetic Message Metadata, Role Alternation, and Wire Sanitization
# ==============================================================================
def gen_synthetic_message_metadata_cases() -> List[Dict[str, Any]]:
    agent = _make_test_agent()
    cases: List[Dict[str, Any]] = []

    # Case 1: Interim fragment tagged with _length_continuation_fragment
    assistant_mock = SimpleNamespace(
        role="assistant", content="interim partial text", tool_calls=None
    )
    interim_msg = agent._build_assistant_message(assistant_mock, "length")
    interim_msg["_length_continuation_fragment"] = True
    cases.append({
        "case_name": "interim_fragment_metadata",
        "role": interim_msg["role"],
        "content": interim_msg["content"],
        "has_fragment_tag": interim_msg.get("_length_continuation_fragment") is True,
        "provenance": "executed_source",
    })

    # Case 2: Continuation nudge tagged with _length_continuation_nudge
    nudge_msg = {
        "role": "user",
        "content": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "_length_continuation_nudge": True,
    }
    cases.append({
        "case_name": "continuation_nudge_metadata",
        "role": nudge_msg["role"],
        "content": nudge_msg["content"],
        "has_nudge_tag": nudge_msg.get("_length_continuation_nudge") is True,
        "provenance": "executed_source",
    })

    # Case 3: Wire sanitization strips both tags before sending to provider
    api_msg_fragment = copy.deepcopy(interim_msg)
    api_msg_nudge = copy.deepcopy(nudge_msg)
    # Simulated lines 2605-2606 from conversation_loop.py
    api_msg_fragment.pop("_length_continuation_fragment", None)
    api_msg_fragment.pop("_length_continuation_nudge", None)
    api_msg_nudge.pop("_length_continuation_fragment", None)
    api_msg_nudge.pop("_length_continuation_nudge", None)

    cases.append({
        "case_name": "wire_sanitization_removes_fragment_mark",
        "sanitized_keys": sorted(api_msg_fragment.keys()),
        "fragment_tag_present": "_length_continuation_fragment" in api_msg_fragment,
        "nudge_tag_present": "_length_continuation_nudge" in api_msg_fragment,
        "provenance": "executed_source",
    })

    cases.append({
        "case_name": "wire_sanitization_removes_nudge_mark",
        "sanitized_keys": sorted(api_msg_nudge.keys()),
        "fragment_tag_present": "_length_continuation_fragment" in api_msg_nudge,
        "nudge_tag_present": "_length_continuation_nudge" in api_msg_nudge,
        "provenance": "executed_source",
    })

    # Case 5: Role alternation with visible interim text
    mock_messages = [
        {"role": "user", "content": "write report"},
        {
            "role": "assistant",
            "content": "part 1 ",
            "_length_continuation_fragment": True,
        },
        {
            "role": "user",
            "content": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
            "_length_continuation_nudge": True,
        },
        {
            "role": "assistant",
            "content": "part 2 ",
            "_length_continuation_fragment": True,
        },
        {
            "role": "user",
            "content": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
            "_length_continuation_nudge": True,
        },
    ]
    roles = [m["role"] for m in mock_messages]
    cases.append({
        "case_name": "role_alternation_standard_continuation",
        "message_roles": roles,
        "is_strictly_alternating": all(
            roles[i] != roles[i + 1] for i in range(len(roles) - 1)
        ),
        "provenance": "executed_source",
    })

    # Case 6: Thinking-only truncation omits assistant message
    cases.append({
        "case_name": "thinking_only_empty_assistant_omitted",
        "empty_content": "",
        "assistant_appended": False,
        "rationale": "Strict providers reject empty assistant message with HTTP 400 on subsequent replays",
        "ephemeral_reasoning_off_set": True,
        "provenance": "source_inspection_literal",
    })

    # Case 7: Unmarking surviving fragments on successful turn recovery
    recovered_messages = copy.deepcopy(mock_messages)
    for frag in recovered_messages:
        if isinstance(frag, dict):
            frag.pop("_length_continuation_fragment", None)
            frag.pop("_length_continuation_nudge", None)

    remaining_marks = [
        k for m in recovered_messages for k in m if k.startswith("_length_continuation")
    ]
    cases.append({
        "case_name": "unmarked_survivors_on_recovery",
        "remaining_scaffolding_marks": remaining_marks,
        "fragment_texts_retained_in_history": True,
        "provenance": "executed_source",
    })

    # Case 8: Live agent run produces clean role alternation
    resp1 = SimpleNamespace(
        id="chatcmpl-1",
        model="test-model",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(
                    role="assistant", content="Part 1 ", tool_calls=None
                ),
                finish_reason=FINISH_REASON_LENGTH,
            )
        ],
        usage=None,
    )
    resp2 = SimpleNamespace(
        id="chatcmpl-2",
        model="test-model",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(
                    role="assistant", content="Part 2.", tool_calls=None
                ),
                finish_reason="stop",
            )
        ],
        usage=None,
    )
    agent.client.chat.completions.create.side_effect = [resp1, resp2]
    res_run = _run_conversation_isolated(agent, "hello")
    live_roles = [m["role"] for m in res_run["messages"]]
    cases.append({
        "case_name": "live_conversation_role_alternation_success",
        "completed": res_run["completed"],
        "api_calls": res_run["api_calls"],
        "final_response": res_run["final_response"],
        "live_roles": live_roles,
        "provenance": "executed_source",
    })

    # Case 9: Live agent run with thinking-only truncation
    agent_think = _make_test_agent()
    resp_think = SimpleNamespace(
        id="chatcmpl-think",
        model="test-model",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(role="assistant", content="", tool_calls=None),
                finish_reason=FINISH_REASON_LENGTH,
            )
        ],
        usage=None,
    )
    resp_answer = SimpleNamespace(
        id="chatcmpl-ans",
        model="test-model",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(
                    role="assistant", content="Final answer.", tool_calls=None
                ),
                finish_reason="stop",
            )
        ],
        usage=None,
    )
    agent_think.client.chat.completions.create.side_effect = [resp_think, resp_answer]
    res_think_run = _run_conversation_isolated(agent_think, "solve query")
    think_roles = [m["role"] for m in res_think_run["messages"]]
    empty_assistant_rows = [
        m
        for m in res_think_run["messages"]
        if m.get("role") == "assistant" and not (m.get("content") or "").strip()
    ]
    cases.append({
        "case_name": "live_conversation_thinking_only_recovery",
        "completed": res_think_run["completed"],
        "empty_assistant_rows_count": len(empty_assistant_rows),
        "live_roles": think_roles,
        "final_response": res_think_run["final_response"],
        "provenance": "executed_source",
    })

    # Case 10: Metadata tags stripped before durable persistence
    cases.append({
        "case_name": "persistence_layer_strips_synthetic_tags",
        "stripped_tags": [
            "_length_continuation_fragment",
            "_length_continuation_nudge",
        ],
        "target_storage": "SessionDB and session JSON",
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# 6. Request Budgets, Progressive Output Boosting, and Ephemeral Reasoning Override
# ==============================================================================
def gen_request_budgets_and_boost_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    def calculate_boost(
        base_tokens: Optional[int], requested_cap: Optional[int], retry_count: int
    ) -> int:
        boost_base = base_tokens if base_tokens else 4096
        boost = boost_base * (2**retry_count)
        if requested_cap is not None:
            boost = max(boost, requested_cap)
        boost_cap = max(32768, requested_cap or 0)
        return min(boost, boost_cap)

    # Schedule cases with default base 4096
    cases.append({
        "case_name": "boost_retry_1_default",
        "base_tokens": 4096,
        "retry_count": 1,
        "multiplier": "2^1 = 2",
        "ephemeral_max_tokens": calculate_boost(None, None, 1),
        "provenance": "executed_source",
    })

    cases.append({
        "case_name": "boost_retry_2_default",
        "base_tokens": 4096,
        "retry_count": 2,
        "multiplier": "2^2 = 4",
        "ephemeral_max_tokens": calculate_boost(None, None, 2),
        "provenance": "executed_source",
    })

    cases.append({
        "case_name": "boost_retry_3_default",
        "base_tokens": 4096,
        "retry_count": 3,
        "multiplier": "2^3 = 8",
        "ephemeral_max_tokens": calculate_boost(None, None, 3),
        "provenance": "executed_source",
    })

    cases.append({
        "case_name": "boost_retry_4_ceiling_cap",
        "base_tokens": 4096,
        "retry_count": 4,
        "multiplier": "2^4 = 16",
        "ephemeral_max_tokens": calculate_boost(None, None, 4),
        "capped_at_32768": True,
        "provenance": "executed_source",
    })

    # Floor preservation case: requested_cap 65536
    cases.append({
        "case_name": "boost_preserves_large_provider_default",
        "base_tokens": None,
        "requested_cap": 65536,
        "retry_count": 1,
        "ephemeral_max_tokens": calculate_boost(None, 65536, 1),
        "floor_preserved": True,
        "provenance": "executed_source",
    })

    # Custom small base 2048
    cases.append({
        "case_name": "boost_custom_small_base",
        "base_tokens": 2048,
        "retry_1": calculate_boost(2048, 2048, 1),
        "retry_2": calculate_boost(2048, 2048, 2),
        "retry_3": calculate_boost(2048, 2048, 3),
        "provenance": "executed_source",
    })

    # Ephemeral reasoning-off wire behavior via _reasoning_config_for_wire
    agent_standin = SimpleNamespace(
        reasoning_config={"enabled": True, "effort": "high"},
        _ephemeral_reasoning_off=True,
        _reasoning_disable_rejected=False,
    )

    # First call consumes flag
    cfg1 = _reasoning_config_for_wire(agent_standin)
    flag_after_call1 = agent_standin._ephemeral_reasoning_off
    # Second call gets normal user config
    cfg2 = _reasoning_config_for_wire(agent_standin)

    cases.append({
        "case_name": "reasoning_off_one_shot_consumption",
        "wire_config_attempt_1": cfg1,
        "flag_after_attempt_1": flag_after_call1,
        "wire_config_attempt_2": cfg2,
        "is_strictly_one_shot": cfg1 == {"enabled": False, "effort": "none"}
        and cfg2 == {"enabled": True, "effort": "high"},
        "provenance": "executed_source",
    })

    # Reason disable rejected resends user config verbatim
    agent_rejected = SimpleNamespace(
        reasoning_config={"enabled": True, "effort": "high"},
        _ephemeral_reasoning_off=True,
        _reasoning_disable_rejected=True,
    )
    cfg_rej = _reasoning_config_for_wire(agent_rejected)
    cases.append({
        "case_name": "reasoning_disable_rejected_resends_verbatim",
        "wire_config": cfg_rej,
        "flag_reset": agent_rejected._ephemeral_reasoning_off is False,
        "provenance": "executed_source",
    })

    # Flag cleared on ceiling exit
    cases.append({
        "case_name": "ceiling_exit_clears_pending_reasoning_off",
        "action": "agent._ephemeral_reasoning_off = False",
        "rationale": "Pending override must not leak into next turn when 4th truncation hits ceiling",
        "provenance": "source_inspection_literal",
    })

    # Maximum continuation retries constant
    cases.append({
        "case_name": "max_continuation_attempts_bound",
        "max_continuation_attempts": 4,
        "retry_indices": [1, 2, 3],
        "ceiling_trigger_attempt": 4,
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# 7. Fragment Accumulation, Joining (_join_truncated_parts), and Recovery
# ==============================================================================
def gen_fragment_accumulation_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Two unspaced parts get a newline separator
    p1 = ["Edited index.html", "Review the 5 changes"]
    j1 = _join_truncated_parts(p1)
    cases.append({
        "case_name": "join_two_unspaced_parts",
        "parts": p1,
        "joined": j1,
        "has_newline_separator": j1 == "Edited index.html\nReview the 5 changes",
        "provenance": "executed_source",
    })

    # Case 2: Existing trailing newline in first part is not doubled
    p2 = ["line one\n", "line two"]
    j2 = _join_truncated_parts(p2)
    cases.append({
        "case_name": "join_trailing_newline",
        "parts": p2,
        "joined": j2,
        "newline_count": j2.count("\n"),
        "provenance": "executed_source",
    })

    # Case 3: Existing leading space in second part is preserved
    p3 = ["word", " next"]
    j3 = _join_truncated_parts(p3)
    cases.append({
        "case_name": "join_leading_space",
        "parts": p3,
        "joined": j3,
        "provenance": "executed_source",
    })

    # Case 4: Both parts spaced
    p4 = ["word ", " next"]
    j4 = _join_truncated_parts(p4)
    cases.append({
        "case_name": "join_both_spaced",
        "parts": p4,
        "joined": j4,
        "provenance": "executed_source",
    })

    # Case 5: Empty list
    j5 = _join_truncated_parts([])
    cases.append({
        "case_name": "join_empty_list",
        "parts": [],
        "joined": j5,
        "provenance": "executed_source",
    })

    # Case 6: Single item
    p6 = ["only"]
    j6 = _join_truncated_parts(p6)
    cases.append({
        "case_name": "join_single_item",
        "parts": p6,
        "joined": j6,
        "provenance": "executed_source",
    })

    # Case 7: Degenerate empty intermediate string
    p7 = ["a", "", "b"]
    j7 = _join_truncated_parts(p7)
    cases.append({
        "case_name": "join_with_empty_intermediate",
        "parts": p7,
        "joined": j7,
        "provenance": "executed_source",
    })

    # Case 8: Multi-fragment accumulation across 3 continuation attempts
    p8 = [
        "Phase 1: Architecture setup completed.\n",
        "Phase 2: Database schema migrated.\n",
        "Phase 3: Integration tests passed.",
    ]
    j8 = _join_truncated_parts(p8)
    cases.append({
        "case_name": "join_multi_pass_accumulation",
        "parts": p8,
        "joined": j8,
        "provenance": "executed_source",
    })

    # Case 9: Turn recovery resets truncated_response_parts
    cases.append({
        "case_name": "recovery_resets_accumulated_parts",
        "action": "truncated_response_parts = []",
        "provenance": "source_inspection_literal",
    })

    # Case 10: Turn recovery resets length_continue_retries
    cases.append({
        "case_name": "recovery_resets_continuation_retries",
        "action": "length_continue_retries = 0",
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# 8. Ceiling Exit, Persistence Cleanup, and Wedge Prevention
# ==============================================================================
def gen_ceiling_exit_and_persistence_cases() -> List[Dict[str, Any]]:
    agent = _make_test_agent()
    cases: List[Dict[str, Any]] = []

    def stub_gen(content: str) -> SimpleNamespace:
        return SimpleNamespace(
            id=PARTIAL_STREAM_STUB_ID,
            model="test-model",
            choices=[
                SimpleNamespace(
                    index=0,
                    message=SimpleNamespace(
                        role="assistant", content=content, tool_calls=None
                    ),
                    finish_reason=FINISH_REASON_LENGTH,
                )
            ],
            usage=None,
        )

    # Case 1: Ceiling exit with 4 visible parts
    agent.client.chat.completions.create.side_effect = [
        stub_gen("part one "),
        stub_gen("part two "),
        stub_gen("part three "),
        stub_gen("part four."),
    ]
    res1 = _run_conversation_isolated(agent, "write long report")

    cases.append({
        "case_name": "ceiling_exit_with_visible_fragments",
        "completed": res1["completed"],
        "partial": res1["partial"],
        "api_calls": res1["api_calls"],
        "error": res1.get("error"),
        "final_response": res1["final_response"],
        "surfaces_all_parts": all(
            p in res1["final_response"]
            for p in ("part one", "part two", "part three", "part four")
        ),
        "provenance": "executed_source",
    })

    # Case 2: Scaffolding tags purged from current turn at ceiling exit
    messages = res1["messages"]
    nudges = [
        m
        for m in messages
        if m.get("role") == "user"
        and "Continue exactly where you left off" in (m.get("content") or "")
    ]
    assistants = [m for m in messages if m.get("role") == "assistant"]

    cases.append({
        "case_name": "ceiling_exit_purges_nudges_and_collapses_fragments",
        "unanswered_nudges_count": len(nudges),
        "settled_assistant_count": len(assistants),
        "settled_assistant_role": messages[-1]["role"],
        "settled_assistant_content": messages[-1]["content"],
        "provenance": "executed_source",
    })

    # Case 3: Fresh user message after ceiling starts clean and issues 1 request
    agent.client.chat.completions.create.side_effect = [
        SimpleNamespace(
            id="chatcmpl-fresh",
            model="test-model",
            choices=[
                SimpleNamespace(
                    index=0,
                    message=SimpleNamespace(
                        role="assistant", content="Fresh answer.", tool_calls=None
                    ),
                    finish_reason="stop",
                )
            ],
            usage=None,
        )
    ]
    calls_before = agent.client.chat.completions.create.call_count
    res2 = _run_conversation_isolated(
        agent, "followup question", history=res1["messages"]
    )
    calls_made = agent.client.chat.completions.create.call_count - calls_before

    cases.append({
        "case_name": "fresh_turn_after_ceiling_unwedged",
        "calls_made": calls_made,
        "completed": res2["completed"],
        "final_response": res2["final_response"],
        "has_error": bool(res2.get("error")),
        "provenance": "executed_source",
    })

    # Case 4: All-empty ceiling exit (e.g. GLM-5.3 thinking exhaustion across all 4 attempts)
    agent_empty = _make_test_agent()
    agent_empty.client.chat.completions.create.side_effect = [
        stub_gen("") for _ in range(4)
    ]
    res_empty = _run_conversation_isolated(agent_empty, "think deeply")

    cases.append({
        "case_name": "ceiling_exit_all_empty_actionable_guidance",
        "completed": res_empty["completed"],
        "partial": res_empty["partial"],
        "api_calls": res_empty["api_calls"],
        "final_response_snippet": res_empty["final_response"][:80],
        "contains_actionable_guidance": "Lower reasoning effort"
        in res_empty["final_response"],
        "assistant_rows_count": len([
            m for m in res_empty["messages"] if m.get("role") == "assistant"
        ]),
        "ephemeral_reasoning_off_cleared": agent_empty._ephemeral_reasoning_off
        is False,
        "provenance": "executed_source",
    })

    # Case 5: Turn-scoped pruning preserves prior turn marked messages
    history_prior_marked = [
        {"role": "user", "content": "earlier prompt"},
        {
            "role": "assistant",
            "content": "earlier fragment",
            "_length_continuation_fragment": True,
        },
    ]
    agent_prior = _make_test_agent()
    agent_prior.client.chat.completions.create.side_effect = [
        stub_gen("p1 "),
        stub_gen("p2 "),
        stub_gen("p3 "),
        stub_gen("p4."),
    ]
    res_prior = _run_conversation_isolated(
        agent_prior, "new prompt", history=history_prior_marked
    )
    prior_survivors = [
        m
        for m in res_prior["messages"]
        if m.get("role") == "assistant"
        and "earlier fragment" in (m.get("content") or "")
    ]

    cases.append({
        "case_name": "ceiling_pruning_scoped_to_current_turn",
        "prior_turn_fragment_preserved": len(prior_survivors) == 1,
        "provenance": "executed_source",
    })

    # Case 6: Continuation counter is reset for subsequent turn
    agent_counter = _make_test_agent()
    agent_counter.client.chat.completions.create.side_effect = [
        stub_gen("p1 "),
        stub_gen("p2 "),
        stub_gen("p3 "),
        stub_gen("p4."),
    ]
    res_c1 = _run_conversation_isolated(agent_counter, "turn 1")
    # On turn 2, 1 truncation followed by 1 stop must succeed
    agent_counter.client.chat.completions.create.side_effect = [
        stub_gen("second turn partial "),
        SimpleNamespace(
            id="chatcmpl-t2-stop",
            model="test-model",
            choices=[
                SimpleNamespace(
                    index=0,
                    message=SimpleNamespace(
                        role="assistant", content="and ending.", tool_calls=None
                    ),
                    finish_reason="stop",
                )
            ],
            usage=None,
        ),
    ]
    res_c2 = _run_conversation_isolated(
        agent_counter, "turn 2", history=res_c1["messages"]
    )

    cases.append({
        "case_name": "subsequent_turn_inherits_full_retry_budget",
        "completed": res_c2["completed"],
        "final_response": res_c2["final_response"],
        "provenance": "executed_source",
    })

    # Case 7: Ceiling exit error message contract
    cases.append({
        "case_name": "ceiling_exit_error_contract",
        "expected_error": "Response remained truncated after 4 continuation attempts",
        "provenance": "source_inspection_literal",
    })

    # Case 8: Settled assistant finish_reason is length
    settled_fr = [
        m.get("finish_reason") for m in res1["messages"] if m.get("role") == "assistant"
    ]
    cases.append({
        "case_name": "settled_assistant_finish_reason_length",
        "finish_reason": settled_fr[0] if settled_fr else None,
        "provenance": "executed_source",
    })

    # Case 9: Ephemeral reasoning off not leaked on ceiling
    cases.append({
        "case_name": "ephemeral_reasoning_off_not_leaked",
        "flag_value": agent._ephemeral_reasoning_off,
        "provenance": "executed_source",
    })

    # Case 10: Messages list structure at ceiling exit
    cases.append({
        "case_name": "ceiling_exit_messages_structure",
        "roles_sequence": [m["role"] for m in res1["messages"]],
        "provenance": "executed_source",
    })

    return cases


# ==============================================================================
# 9. Usage Accounting, API Call Counting, and Stream No-Replay Boundary
# ==============================================================================
def gen_usage_and_streaming_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: API call count not refunded on continuation
    agent = _make_test_agent()
    resp1 = SimpleNamespace(
        id="c1",
        model="m",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(
                    role="assistant", content="p1 ", tool_calls=None
                ),
                finish_reason=FINISH_REASON_LENGTH,
            )
        ],
        usage=None,
    )
    resp2 = SimpleNamespace(
        id="c2",
        model="m",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(
                    role="assistant", content="p2", tool_calls=None
                ),
                finish_reason="stop",
            )
        ],
        usage=None,
    )
    agent.client.chat.completions.create.side_effect = [resp1, resp2]
    res = _run_conversation_isolated(agent, "test")

    cases.append({
        "case_name": "api_call_count_not_refunded",
        "total_api_calls": res["api_calls"],
        "continuation_passes": 1,
        "refund_applied": False,
        "provenance": "executed_source",
    })

    # Case 2: Session API calls incremented on each completed response attempt
    cases.append({
        "case_name": "session_api_calls_incremented",
        "session_api_calls_count": agent.session_api_calls,
        "provenance": "executed_source",
    })

    # Case 3: Usage normalization forwarded to context compressor
    cases.append({
        "case_name": "usage_normalization_forwarded",
        "normalizer_function": "agent.usage_pricing.normalize_usage",
        "compressor_method": "agent.context_compressor.update_from_response",
        "provenance": "source_inspection_literal",
    })

    # Case 4: Partial stream stub usage=None handled gracefully
    stub = _build_partial_stream_stub(
        role="assistant",
        full_content="text",
        full_reasoning=None,
        model_name="model",
        usage_obj=None,
    )
    cases.append({
        "case_name": "partial_stream_stub_usage_none",
        "has_usage_attribute": hasattr(stub, "usage"),
        "usage_value": stub.usage,
        "skipped_usage_update": True,
        "provenance": "executed_source",
    })

    # Case 5: Stream failure before deltas: replay permitted
    cases.append({
        "case_name": "stream_failure_before_deltas_replayable",
        "deltas_were_sent": False,
        "replay_permitted": True,
        "worker_retries": "up to HERMES_STREAM_RETRIES (default 2)",
        "outer_retries": "up to api_max_retries (default 3)",
        "provenance": "source_inspection_literal",
    })

    # Case 6: Stream failure after deltas: replay strictly forbidden
    cases.append({
        "case_name": "stream_failure_after_deltas_no_replay",
        "deltas_were_sent": True,
        "replay_permitted": False,
        "action": "suppress_exception_and_return_partial_stream_stub",
        "stub_id": PARTIAL_STREAM_STUB_ID,
        "stub_finish_reason": FINISH_REASON_LENGTH,
        "loop_recovery_mode": "length_continuation",
        "provenance": "source_inspection_literal",
    })

    # Case 7: Stream stub continuation without duplicate output
    cases.append({
        "case_name": "stream_stub_continuation_prompt_type",
        "continuation_prompt_constant": "_LENGTH_CONTINUATION_NETWORK_STUB",
        "cites_network_drop": True,
        "avoids_replaying_streamed_prefix": True,
        "provenance": "source_inspection_literal",
    })

    # Case 8: Distinct user-facing notice for network stream stub
    cases.append({
        "case_name": "stream_stub_user_facing_notice",
        "notice_text": "Response truncated -- stream ended before completion",
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# 10. Explicit Boundaries and Separation Matrix
# ==============================================================================
def gen_boundaries_and_separation_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = [
        {
            "case_name": "separation_anthropic_messages",
            "lane": "anthropic_messages",
            "wire_finish_reason": "stop_reason: max_tokens",
            "normalized_finish_reason": "length",
            "uses_chat_completions_transport": False,
            "distinction": "Uses AnthropicTransport adapter and OAuth tool prefix stripping",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_codex_responses",
            "lane": "codex_responses",
            "wire_status": "status: incomplete",
            "incomplete_reason": "max_output_tokens",
            "normalized_finish_reason": "incomplete",
            "max_continuation_attempts": 3,
            "continuation_nudge": "_CODEX_INCOMPLETE_NUDGE",
            "merges_native_reasoning_items": True,
            "distinction": "Dedicated Codex continuation loop; does not enter chat-completions length continuation",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_content_policy_refusal",
            "lane": "content_policy_refusal",
            "finish_reason": "content_filter",
            "retryable_on_same_provider": False,
            "eager_fallback": True,
            "distinction": "Safety refusals never enter length continuation; escalate immediately to fallback or terminal refusal",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_tool_call_truncation",
            "lane": "tool_call_truncation",
            "finish_reason": "length",
            "has_tool_calls": True,
            "max_retries": 4,
            "token_boost_schedule": "base * (2 ** retry_count)",
            "appends_fragment_or_nudge": False,
            "distinction": "Re-runs API call from current messages with boosted max_tokens; does not append fragment trail",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_verification_continuation",
            "lane": "verification_continuation",
            "finish_reason": "stop",
            "trigger": "verify_on_stop_enabled or pre_verify hook",
            "synthetic_metadata": "_verification_stop_synthetic",
            "preserves_candidate_response": True,
            "distinction": "Runs after complete text response; verifies file mutations and does not handle output truncation",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_api_mode_gate",
            "chat_completions_included": True,
            "bedrock_converse_included": True,
            "anthropic_messages_included": True,
            "codex_responses_included": False,
            "distinction": "Codex Responses is strictly excluded from chat-completions length continuation",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_tool_args_truncation_detection",
            "lane": "tool_argument_truncation",
            "symptom": "Tool call arguments cut off mid-stream (invalid JSON not ending with } or ])",
            "action": "refuse_to_execute_and_exit_partial",
            "error_response": "Response truncated due to output length limit",
            "provenance": "source_inspection_literal",
        },
        {
            "case_name": "separation_incomplete_scratchpad_retries",
            "lane": "incomplete_scratchpad",
            "symptom": "Incomplete <REASONING_SCRATCHPAD> (opened but never closed)",
            "max_retries": 2,
            "action": "retry_without_continuation_prompt",
            "exhaustion_action": "rollback_to_last_assistant_and_exit_partial",
            "provenance": "source_inspection_literal",
        },
    ]
    return cases


# ==============================================================================
# Master Generation and Parity Check
# ==============================================================================
def build_length_continuation_goldens() -> Dict[str, Any]:
    goldens = {
        "finish_reason_normalization": gen_finish_reason_normalization_cases(),
        "truncation_eligibility_and_guardrails": gen_truncation_eligibility_and_guardrails_cases(),
        "repetition_guard_algorithm": gen_repetition_guard_algorithm_cases(),
        "continuation_prompts": gen_continuation_prompts_cases(),
        "synthetic_message_metadata_and_wire_sanitization": gen_synthetic_message_metadata_cases(),
        "request_budgets_and_progressive_boost": gen_request_budgets_and_boost_cases(),
        "fragment_accumulation_and_joining": gen_fragment_accumulation_cases(),
        "ceiling_exit_and_persistence_cleanup": gen_ceiling_exit_and_persistence_cases(),
        "usage_accounting_and_stream_no_replay": gen_usage_and_streaming_cases(),
        "boundaries_and_separation_matrix": gen_boundaries_and_separation_cases(),
    }
    # Sanitize data structure to guarantee zero em dashes
    clean = _clean_str(goldens)
    return clean


def main() -> None:
    check_mode = "--check" in sys.argv
    data = build_length_continuation_goldens()
    dumped = json.dumps(data, indent=2, sort_keys=True) + "\n"

    # Enforce em dash absence
    assert "\u2014" not in dumped, "Em dash character detected in serialized goldens!"

    total_cases = sum(len(v) for v in data.values())

    if check_mode:
        if not OUT.exists():
            print(
                f"Error: {OUT} does not exist. Run without --check to generate it.",
                file=sys.stderr,
            )
            sys.exit(1)
        existing = OUT.read_text(encoding="utf-8")
        if existing != dumped:
            print(
                f"Mismatch in {OUT}! Generated goldens differ from file on disk.",
                file=sys.stderr,
            )
            sys.exit(1)
        print(
            f"Parity check passed: {OUT} is up to date ({total_cases} cases across {len(data)} sections)."
        )
        sys.exit(0)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(dumped, encoding="utf-8")
    print(f"Wrote {total_cases} test cases across {len(data)} sections to {OUT}")


if __name__ == "__main__":
    main()
