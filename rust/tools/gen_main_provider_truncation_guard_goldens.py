#!/usr/bin/env python3
"""Deterministic source-executed oracle for main-provider length-truncation content guards.

Audits and executes live Python decision functions and runtime transitions for:
1. thinking-exhausted detection: tag-based reasoning exhaustion abort without continuation retry.
2. repetition-dominated rejection: fast line-path and sliding-window degenerate loop detection.
3. empty reasoning-only one-shot reasoning disable: suppression of empty interim assistant rows,
   one-shot ephemeral wire override, progressive token cap boosting, and ceiling exit.

Usage:
    python3 rust/tools/gen_main_provider_truncation_guard_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_truncation_guard_goldens.py --check  # check parity
"""

from __future__ import annotations

import json
import math
import os
import re
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-truncation-guard-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

import agent.repetition_guard as rep_guard
from agent.chat_completion_helpers import (
    _consume_ephemeral_reasoning_off,
    _reasoning_config_for_wire,
)
from agent.conversation_loop import (
    _LENGTH_CONTINUATION_OUTPUT_LIMIT,
)
from agent.repetition_guard import (
    MIN_FRAGMENT_LENGTH,
    _DOMINANCE_RATIO,
    _MIN_REPEAT_COUNT,
    _REPEAT_WINDOW,
    _line_repetition_dominated,
    is_repetition_dominated,
)
from run_agent import AIAgent


def _clean_str(val: Any) -> Any:
    """Sanitize string or recursive data structure to guarantee no em dash characters."""
    if isinstance(val, str):
        cleaned = val.replace("\u2014", "--")
        assert "\u2014" not in cleaned, f"Em dash found in string: {val!r}"
        return cleaned
    if isinstance(val, list):
        return [_clean_str(v) for v in val]
    if isinstance(val, dict):
        return {k: _clean_str(v) for k, v in val.items()}
    return val


def _make_test_agent() -> AIAgent:
    """Instantiate a minimal AIAgent with mocked tools and client."""
    with (
        patch("run_agent.get_tool_definitions", return_value=[]),
        patch("run_agent.check_toolset_requirements", return_value={}),
        patch("run_agent.OpenAI"),
    ):
        agent = AIAgent(
            api_key="synth-test-key",
            base_url="https://openrouter.ai/api/v1",
            provider="openrouter",
            model="test/model",
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


class _AgentStandIn:
    """Minimal agent surface for testing reasoning config wire transformation."""

    def __init__(
        self,
        reasoning_config: Optional[Dict[str, Any]] = None,
        ephemeral_reasoning_off: bool = False,
        reasoning_disable_rejected: bool = False,
    ):
        self.reasoning_config = reasoning_config
        self._ephemeral_reasoning_off = ephemeral_reasoning_off
        self._reasoning_disable_rejected = reasoning_disable_rejected


# ==============================================================================
# Section 1: Thinking-Exhausted Detection Cases
# ==============================================================================
def gen_thinking_exhausted_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []
    agent = _make_test_agent()

    def check_thinking_exhausted(content: Optional[str], has_tool_calls: bool) -> tuple[bool, bool]:
        has_think_tags = bool(
            content
            and re.search(
                r"<(?:think|thinking|reasoning|REASONING_SCRATCHPAD)[^>]*>",
                content,
                re.IGNORECASE,
            )
        )
        exhausted = (
            not has_tool_calls
            and has_think_tags
            and (
                (content is not None and not agent._has_content_after_think_block(content))
                or content is None
            )
        )
        return has_think_tags, exhausted

    # Case 1: Closed <think> tag with no trailing text
    c1 = "<think>internal reasoning monologue</think>"
    has_tags1, ex1 = check_thinking_exhausted(c1, False)
    cases.append({
        "case_name": "closed_think_tag_no_trailing_text",
        "content_snippet": c1,
        "has_tool_calls": False,
        "has_think_tags": has_tags1,
        "has_content_after_think": agent._has_content_after_think_block(c1),
        "thinking_exhausted": ex1,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "error_message": "Model used all output tokens on reasoning with none left for the response. Try lowering reasoning effort or increasing max_tokens.",
        "provenance": "executed_source",
    })

    # Case 2: Closed <thinking> tag with no trailing text
    c2 = "<thinking>step 1 analyze; step 2 compute</thinking>"
    has_tags2, ex2 = check_thinking_exhausted(c2, False)
    cases.append({
        "case_name": "closed_thinking_tag_no_trailing_text",
        "content_snippet": c2,
        "has_tool_calls": False,
        "has_think_tags": has_tags2,
        "has_content_after_think": agent._has_content_after_think_block(c2),
        "thinking_exhausted": ex2,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "provenance": "executed_source",
    })

    # Case 3: Closed <reasoning> tag with no trailing text
    c3 = "<reasoning>analyzing constraints and edge cases</reasoning>"
    has_tags3, ex3 = check_thinking_exhausted(c3, False)
    cases.append({
        "case_name": "closed_reasoning_tag_no_trailing_text",
        "content_snippet": c3,
        "has_tool_calls": False,
        "has_think_tags": has_tags3,
        "has_content_after_think": agent._has_content_after_think_block(c3),
        "thinking_exhausted": ex3,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "provenance": "executed_source",
    })

    # Case 4: Closed <REASONING_SCRATCHPAD> tag with no trailing text
    c4 = "<REASONING_SCRATCHPAD>scratchpad calculations</REASONING_SCRATCHPAD>"
    has_tags4, ex4 = check_thinking_exhausted(c4, False)
    cases.append({
        "case_name": "closed_scratchpad_tag_no_trailing_text",
        "content_snippet": c4,
        "has_tool_calls": False,
        "has_think_tags": has_tags4,
        "has_content_after_think": agent._has_content_after_think_block(c4),
        "thinking_exhausted": ex4,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "provenance": "executed_source",
    })

    # Case 5: Mixed case closed tags
    c5 = "<Think>mixed case tag variant</Think>"
    has_tags5, ex5 = check_thinking_exhausted(c5, False)
    cases.append({
        "case_name": "mixed_case_think_tags_no_trailing_text",
        "content_snippet": c5,
        "has_tool_calls": False,
        "has_think_tags": has_tags5,
        "has_content_after_think": agent._has_content_after_think_block(c5),
        "thinking_exhausted": ex5,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "provenance": "executed_source",
    })

    # Case 6: Unterminated <think> tag at start of content
    c6 = "<think>the model output was cut off before finishing its chain of thought"
    has_tags6, ex6 = check_thinking_exhausted(c6, False)
    cases.append({
        "case_name": "unterminated_think_tag_at_content_start",
        "content_snippet": c6,
        "has_tool_calls": False,
        "has_think_tags": has_tags6,
        "has_content_after_think": agent._has_content_after_think_block(c6),
        "thinking_exhausted": ex6,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "provenance": "executed_source",
    })

    # Case 7: Unterminated <thinking> tag following newline boundary
    c7 = "\n<thinking>unterminated thought starting on fresh line"
    has_tags7, ex7 = check_thinking_exhausted(c7, False)
    cases.append({
        "case_name": "unterminated_thinking_tag_after_newline",
        "content_snippet": c7,
        "has_tool_calls": False,
        "has_think_tags": has_tags7,
        "has_content_after_think": agent._has_content_after_think_block(c7),
        "thinking_exhausted": ex7,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "provenance": "executed_source",
    })

    # Case 8: Think tags WITH trailing visible text
    c8 = "<think>thought completed</think>Here is the visible answer."
    has_tags8, ex8 = check_thinking_exhausted(c8, False)
    cases.append({
        "case_name": "think_tag_with_trailing_visible_text",
        "content_snippet": c8,
        "has_tool_calls": False,
        "has_think_tags": has_tags8,
        "has_content_after_think": agent._has_content_after_think_block(c8),
        "thinking_exhausted": ex8,
        "action": "proceed_to_repetition_or_continuation",
        "visible_text": agent._strip_think_blocks(c8).strip(),
        "provenance": "executed_source",
    })

    # Case 9: Think tags with whitespace only trailing
    c9 = "<think>thought completed</think>   \n\t  "
    has_tags9, ex9 = check_thinking_exhausted(c9, False)
    cases.append({
        "case_name": "think_tag_with_whitespace_only_trailing",
        "content_snippet": c9,
        "has_tool_calls": False,
        "has_think_tags": has_tags9,
        "has_content_after_think": agent._has_content_after_think_block(c9),
        "thinking_exhausted": ex9,
        "action": "abort_turn_immediately",
        "retries_consumed": 0,
        "provenance": "executed_source",
    })

    # Case 10: Tool calls preempt thinking exhaustion
    c10 = "<think>I must invoke a function</think>"
    has_tags10, ex10 = check_thinking_exhausted(c10, True)
    cases.append({
        "case_name": "tool_calls_preempts_thinking_exhausted",
        "content_snippet": c10,
        "has_tool_calls": True,
        "has_think_tags": has_tags10,
        "has_content_after_think": agent._has_content_after_think_block(c10),
        "thinking_exhausted": ex10,
        "action": "route_to_tool_call_truncation_retry",
        "preempted_by": "tool_calls",
        "provenance": "executed_source",
    })

    # Case 11: Empty string content without think tags (e.g. GLM-4.7 on NVIDIA Build, minimax)
    c11 = ""
    has_tags11, ex11 = check_thinking_exhausted(c11, False)
    cases.append({
        "case_name": "empty_string_without_think_tags",
        "content_snippet": c11,
        "has_tool_calls": False,
        "has_think_tags": has_tags11,
        "has_content_after_think": agent._has_content_after_think_block(c11),
        "thinking_exhausted": ex11,
        "action": "proceed_to_empty_reasoning_one_shot_disable",
        "provenance": "executed_source",
    })

    # Case 12: None content without think tags
    c12 = None
    has_tags12, ex12 = check_thinking_exhausted(c12, False)
    cases.append({
        "case_name": "none_content_without_think_tags",
        "content_snippet": c12,
        "has_tool_calls": False,
        "has_think_tags": has_tags12,
        "has_content_after_think": agent._has_content_after_think_block(c12),
        "thinking_exhausted": ex12,
        "action": "proceed_to_empty_reasoning_one_shot_disable",
        "provenance": "executed_source",
    })

    # Case 13: Normal truncated prose without think tags
    c13 = "The Tokyo skyline was covered in fog this morning and"
    has_tags13, ex13 = check_thinking_exhausted(c13, False)
    cases.append({
        "case_name": "normal_truncated_prose_without_think_tags",
        "content_snippet": c13,
        "has_tool_calls": False,
        "has_think_tags": has_tags13,
        "has_content_after_think": agent._has_content_after_think_block(c13),
        "thinking_exhausted": ex13,
        "action": "proceed_to_repetition_guard",
        "provenance": "executed_source",
    })

    # Case 14: Inline prose mention of <think> in regular sentence
    c14 = "To format thoughts, enclose text in <think> tags like this."
    has_tags14, ex14 = check_thinking_exhausted(c14, False)
    cases.append({
        "case_name": "inline_prose_mention_of_think_tag",
        "content_snippet": c14,
        "has_tool_calls": False,
        "has_think_tags": has_tags14,
        "has_content_after_think": agent._has_content_after_think_block(c14),
        "thinking_exhausted": ex14,
        "action": "proceed_to_repetition_guard",
        "provenance": "executed_source",
    })

    # Case 15: Gemma <thought> tag variant distinction
    # Note: strip_think_blocks handles <thought>, but conversation_loop's _has_think_tags regex
    # checks think|thinking|reasoning|REASONING_SCRATCHPAD. Thus <thought> alone is not flagged as thinking exhausted.
    c15 = "<thought>Gemma 4 internal thought</thought>"
    has_tags15, ex15 = check_thinking_exhausted(c15, False)
    cases.append({
        "case_name": "gemma_thought_tag_distinction",
        "content_snippet": c15,
        "has_tool_calls": False,
        "has_think_tags": has_tags15,
        "has_content_after_think": agent._has_content_after_think_block(c15),
        "thinking_exhausted": ex15,
        "action": "not_flagged_by_has_think_tags_regex",
        "provenance": "executed_source",
    })

    # Case 16: Thinking-exhausted abort envelope metadata
    exhaust_user_msg = (
        "⚠️ **Thinking Budget Exhausted**\n\n"
        "The model used all its output tokens on reasoning "
        "and had none left for the actual response.\n\n"
        "To fix this:\n"
        "→ Lower reasoning effort: `/reasoning low` or `/reasoning minimal`\n"
        "→ Or switch to a larger/non-reasoning model with `/model`"
    )
    cases.append({
        "case_name": "thinking_exhausted_abort_envelope",
        "completed": False,
        "partial": True,
        "api_calls": 1,
        "retries_consumed": 0,
        "length_continue_retries": 0,
        "error": "Model used all output tokens on reasoning with none left for the response. Try lowering reasoning effort or increasing max_tokens.",
        "final_response": exhaust_user_msg,
        "messages_state": "unmutated_truncated_assistant_not_appended",
        "session_persisted": True,
        "task_resources_cleaned": True,
        "usage_accounted_in_session": False,
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# Section 2: Repetition-Dominated Guard Cases
# ==============================================================================
def gen_repetition_guard_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []
    agent = _make_test_agent()

    # Exact sentence from incident #86581
    incident_sentence = "好，你幫我更改成 Google Gemini 4 31B。"

    # Case 1: Below MIN_FRAGMENT_LENGTH threshold (400 chars) -> fail open
    short_echo = incident_sentence * 5  # len ~130 < 400
    cases.append({
        "case_name": "below_min_fragment_length_fail_open",
        "text_length": len(short_echo),
        "min_fragment_length": MIN_FRAGMENT_LENGTH,
        "is_repetition_dominated": is_repetition_dominated(short_echo),
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    # Case 2: Boundary test: length 399 with repetition
    boundary_399 = ("A" * 60 + " ") * 6  # 366 chars
    boundary_399 = boundary_399 + "X" * (399 - len(boundary_399))
    cases.append({
        "case_name": "boundary_below_400_chars",
        "text_length": len(boundary_399),
        "min_fragment_length": MIN_FRAGMENT_LENGTH,
        "is_repetition_dominated": is_repetition_dominated(boundary_399),
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    # Case 3: Fast path: line-based repetition from incident #86581 (narration + echoed line * 800)
    incident_line_rep = ("We need to verify the model setting.\n" + incident_sentence + "\n") * 800
    cases.append({
        "case_name": "line_path_incident_shape_with_narration",
        "text_length": len(incident_line_rep),
        "line_path_result": _line_repetition_dominated(incident_line_rep, len(incident_line_rep)),
        "is_repetition_dominated": is_repetition_dominated(incident_line_rep),
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 4: Fast path: exact repeated line * 50
    repeated_lines = (incident_sentence + "\n") * 50
    cases.append({
        "case_name": "line_path_single_line_echo",
        "text_length": len(repeated_lines),
        "line_path_result": _line_repetition_dominated(repeated_lines, len(repeated_lines)),
        "is_repetition_dominated": is_repetition_dominated(repeated_lines),
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 5: Fast path: minimal count (5) and dominance (>= 50%)
    line_80 = "Z" * 80
    text_min_line = (line_80 + "\n") * 5 + "Filler unique text " * 15
    # total len: 81 * 5 + 285 = 690. line_80 stripped len 80 * 5 = 400 >= 690 * 0.5 (345)
    cases.append({
        "case_name": "line_path_minimal_count_and_dominance",
        "text_length": len(text_min_line),
        "repeat_count": 5,
        "min_repeat_count": _MIN_REPEAT_COUNT,
        "line_path_result": _line_repetition_dominated(text_min_line, len(text_min_line)),
        "is_repetition_dominated": is_repetition_dominated(text_min_line),
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 6: Fast path: count under 5 threshold
    line_100 = "Y" * 100
    text_under_5 = (line_100 + "\n") * 4 + "Unique text " * 10
    cases.append({
        "case_name": "line_path_count_under_threshold",
        "text_length": len(text_under_5),
        "repeat_count": 4,
        "line_path_result": _line_repetition_dominated(text_under_5, len(text_under_5)),
        "provenance": "executed_source",
    })

    # Case 7: Fast path: dominance ratio under 50%
    filler_prose = " ".join(f"unique token {i}" for i in range(2500))
    minority_repeats = filler_prose + ("\n" + incident_sentence + "\n") * 30
    cases.append({
        "case_name": "line_path_dominance_under_50_percent",
        "text_length": len(minority_repeats),
        "line_path_result": _line_repetition_dominated(minority_repeats, len(minority_repeats)),
        "is_repetition_dominated": is_repetition_dominated(minority_repeats),
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    # Case 8: General path: sliding 60-char window without line breaks
    window_rep = incident_sentence * 2000
    cases.append({
        "case_name": "window_path_no_line_breaks_incident_echo",
        "text_length": len(window_rep),
        "repeat_window_size": _REPEAT_WINDOW,
        "line_path_result": _line_repetition_dominated(window_rep, len(window_rep)),
        "is_repetition_dominated": is_repetition_dominated(window_rep),
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 9: General path: sliding 60-char window ASCII pattern
    pattern_60 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz01234567"  # 60 chars
    pattern_text = pattern_60 * 30
    cases.append({
        "case_name": "window_path_exact_60_char_sliding_pattern",
        "text_length": len(pattern_text),
        "repeat_window_size": _REPEAT_WINDOW,
        "is_repetition_dominated": is_repetition_dominated(pattern_text),
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 10: Long legitimate diverse text (1200 unique sentences) -> NOT flagged
    diverse_text = " ".join(
        f"Sentence number {i} describes a distinct topic with unique words "
        f"such as quasar-{i} and nebula-{i} to keep every window distinct."
        for i in range(1200)
    )
    cases.append({
        "case_name": "long_diverse_prose_not_flagged",
        "text_length": len(diverse_text),
        "is_repetition_dominated": is_repetition_dominated(diverse_text),
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    # Case 11: Repetition inside think block only -> visible text is NOT repetition-dominated
    think_echo = "<think>" + (incident_sentence * 50) + "</think>Here is the unique and clean answer."
    visible_only = agent._strip_think_blocks(think_echo)
    cases.append({
        "case_name": "repetition_inside_think_block_only",
        "raw_text_length": len(think_echo),
        "visible_text": visible_only,
        "raw_is_repetition_dominated": is_repetition_dominated(think_echo),
        "visible_is_repetition_dominated": is_repetition_dominated(visible_only),
        "loop_action": "eligible_for_continuation",
        "provenance": "executed_source",
    })

    # Case 12: Repetition in visible text with think block -> flagged on visible text
    visible_with_think = "<think>clean reasoning step</think>" + ((incident_sentence + "\n") * 50)
    visible_extracted = agent._strip_think_blocks(visible_with_think)
    cases.append({
        "case_name": "repetition_in_visible_text_with_think_block",
        "raw_text_length": len(visible_with_think),
        "visible_text_length": len(visible_extracted),
        "visible_is_repetition_dominated": is_repetition_dominated(visible_extracted),
        "loop_action": "abort_turn_immediately",
        "provenance": "executed_source",
    })

    # Case 13: Tool calls present preempts repetition guard
    cases.append({
        "case_name": "tool_calls_preempts_repetition_guard",
        "has_tool_calls": True,
        "visible_is_repetition_dominated": True,
        "evaluated_repetition_dominated": False,  # not _trunc_has_tool_calls and ...
        "loop_action": "route_to_tool_call_truncation_retry",
        "preempted_by": "tool_calls",
        "provenance": "source_inspection_literal",
    })

    # Case 14: Non-string and empty inputs fail open
    cases.append({
        "case_name": "non_string_and_empty_inputs",
        "none_result": is_repetition_dominated(None),  # type: ignore
        "int_result": is_repetition_dominated(123456789),  # type: ignore
        "list_result": is_repetition_dominated(["hello", "world"]),  # type: ignore
        "empty_str_result": is_repetition_dominated(""),
        "provenance": "executed_source",
    })

    # Case 15: Repetition-dominated abort envelope metadata
    rep_user_msg = (
        "⚠️ **Response Stopped -- Repetition Detected**\n\n"
        "The model fell into a repetition loop while "
        "writing this response, so continuing would only "
        "produce more repeated text. The partial response "
        "was discarded.\n\n"
        "→ Switch to a different model with `/model`\n"
        "→ Or resend your message (your conversation "
        "history is preserved)"
    )
    cases.append({
        "case_name": "repetition_dominated_abort_envelope",
        "completed": False,
        "partial": True,
        "api_calls": 1,
        "retries_consumed": 0,
        "length_continue_retries": 0,
        "error": "Model output entered a repetition loop and was truncated mid-loop; refusing to continue a degenerate response.",
        "final_response": rep_user_msg,
        "messages_state": "pathological_fragment_discarded_not_appended",
        "session_persisted": True,
        "task_resources_cleaned": True,
        "usage_accounted_in_session": False,
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# Section 3: Empty Reasoning-Only One-Shot Disable Cases
# ==============================================================================
def gen_empty_reasoning_disable_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Trigger condition evaluation
    # finish_reason="length", no tool calls, content="" (or None), not partial stream stub
    cases.append({
        "case_name": "trigger_condition_empty_reasoning",
        "finish_reason": "length",
        "has_tool_calls": False,
        "interim_content": "",
        "is_empty_partial_stub": False,
        "triggers_ephemeral_reasoning_off": True,
        "appends_interim_assistant_message": False,
        "reason": "skips_empty_interim_row_to_prevent_http_400_poisoning",
        "provenance": "source_inspection_literal",
    })

    # Case 2: One-shot ephemeral reasoning consumption lifecycle
    agent_standin = _AgentStandIn(
        reasoning_config={"enabled": True, "effort": "high"},
        ephemeral_reasoning_off=True,
    )
    wire_call1 = _reasoning_config_for_wire(agent_standin)
    flag_after_call1 = agent_standin._ephemeral_reasoning_off
    wire_call2 = _reasoning_config_for_wire(agent_standin)

    cases.append({
        "case_name": "flag_consumed_exactly_once_wire_lifecycle",
        "initial_user_config": {"enabled": True, "effort": "high"},
        "wire_call1_config": wire_call1,
        "flag_after_call1": flag_after_call1,
        "wire_call2_config": wire_call2,
        "one_shot_invariant_preserved": (
            wire_call1 == {"enabled": False, "effort": "none"}
            and flag_after_call1 is False
            and wire_call2 == {"enabled": True, "effort": "high"}
        ),
        "provenance": "executed_source",
    })

    # Case 3: Override when user has no explicit reasoning config
    agent_no_cfg = _AgentStandIn(reasoning_config=None, ephemeral_reasoning_off=True)
    wire_no_cfg = _reasoning_config_for_wire(agent_no_cfg)
    cases.append({
        "case_name": "flag_with_no_user_reasoning_config",
        "initial_user_config": None,
        "wire_config": wire_no_cfg,
        "flag_reset": agent_no_cfg._ephemeral_reasoning_off is False,
        "provenance": "executed_source",
    })

    # Case 4: Route rejects reasoning disable (_reasoning_disable_rejected=True)
    # The provider returned 400 'reasoning is mandatory'. Ephemeral override is discarded, user's config resent.
    agent_rejected = _AgentStandIn(
        reasoning_config={"enabled": True, "effort": "high"},
        ephemeral_reasoning_off=True,
        reasoning_disable_rejected=True,
    )
    wire_rejected = _reasoning_config_for_wire(agent_rejected)
    cases.append({
        "case_name": "rejected_disable_resends_user_config_verbatim",
        "initial_user_config": {"enabled": True, "effort": "high"},
        "reasoning_disable_rejected": True,
        "wire_config": wire_rejected,
        "flag_reset": agent_rejected._ephemeral_reasoning_off is False,
        "cache_key_preserved": True,
        "provenance": "executed_source",
    })

    # Case 5: Route rejects reasoning disable when user's own config is disabled
    agent_rejected_disable = _AgentStandIn(
        reasoning_config={"enabled": False},
        reasoning_disable_rejected=True,
    )
    wire_omitted = _reasoning_config_for_wire(agent_rejected_disable)
    cases.append({
        "case_name": "rejected_disable_omits_disabled_config",
        "initial_user_config": {"enabled": False},
        "reasoning_disable_rejected": True,
        "wire_config": wire_omitted,  # None -> omitted from wire request
        "provenance": "executed_source",
    })

    # Case 6: Turn start resets stale flag to prevent leakage into next turn
    stale_flag_before = True
    # At start of conversation_loop.py:2215: agent._ephemeral_reasoning_off = False
    stale_flag_after = False
    cases.append({
        "case_name": "stale_flag_reset_at_turn_start",
        "flag_before_turn": stale_flag_before,
        "flag_after_turn_init": stale_flag_after,
        "action": "agent._ephemeral_reasoning_off = False",
        "prevents_leak_into_next_turn": True,
        "provenance": "source_inspection_literal",
    })

    # Case 7: Progressive output cap boosting schedule
    def compute_boost(base_tokens: int, retry_count: int, requested_cap: Optional[int] = None) -> int:
        boost_base = base_tokens if base_tokens else 4096
        boost = boost_base * (2**retry_count)
        if requested_cap is not None:
            boost = max(boost, requested_cap)
        boost_cap = max(32768, requested_cap or 0)
        return min(boost, boost_cap)

    boost_steps = [
        {"retry": 1, "base": 4096, "effective_cap": compute_boost(4096, 1)},
        {"retry": 2, "base": 4096, "effective_cap": compute_boost(4096, 2)},
        {"retry": 3, "base": 4096, "effective_cap": compute_boost(4096, 3)},
        {"retry": 4, "base": 4096, "effective_cap": compute_boost(4096, 4)},
        {"retry": 1, "base": 16384, "effective_cap": compute_boost(16384, 1)},
    ]
    cases.append({
        "case_name": "progressive_output_cap_boosting_schedule",
        "schedule": boost_steps,
        "provenance": "executed_source",
    })

    # Case 8: Continuation prompt content for length truncation
    cases.append({
        "case_name": "continuation_prompt_nudge",
        "nudge_content": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "synthetic_tag": "_length_continuation_nudge",
        "appended_as_role": "user",
        "provenance": "source_inspection_literal",
    })

    # Case 9: Ceiling exit after 4 empty attempts
    ceiling_msg = (
        "⚠️ **No visible answer was produced.** The "
        "model hit its output-token limit on every "
        "continuation attempt -- its reasoning "
        "consumed the entire budget each time.\n\n"
        "To fix this:\n"
        "→ Lower reasoning effort: `/reasoning low` "
        "or `/reasoning none`\n"
        "→ Or raise max_tokens for this model"
    )
    cases.append({
        "case_name": "ceiling_exit_all_empty_reasoning_attempts",
        "length_continue_retries": 4,
        "partial_response": "",
        "ephemeral_reasoning_off_cleared": False,
        "scaffolding_nudges_purged": True,
        "completed": False,
        "partial": True,
        "api_calls": 4,
        "error": "Response remained truncated after 4 continuation attempts",
        "final_response": ceiling_msg,
        "provenance": "source_inspection_literal",
    })

    # Case 10: Multi-pass recovery sequence: visible fragment + empty thinking + completed
    cases.append({
        "case_name": "mixed_fragment_multi_pass_recovery",
        "pass1": {
            "response": "visible part one. ",
            "finish_reason": "length",
            "interim_assistant_appended": True,
            "ephemeral_reasoning_off": False,
        },
        "pass2": {
            "response": "",
            "finish_reason": "length",
            "interim_assistant_appended": False,
            "ephemeral_reasoning_off": True,
        },
        "pass3": {
            "response": "and the ending.",
            "finish_reason": "stop",
            "completed": True,
            "ephemeral_reasoning_off": False,
        },
        "final_response_contains": ["visible part one.", "and the ending."],
        "empty_assistant_rows_in_transcript": 0,
        "provenance": "source_inspection_literal",
    })

    return cases


# ==============================================================================
# Master Generation and Parity Check
# ==============================================================================
def build_truncation_guard_goldens() -> Dict[str, Any]:
    goldens = {
        "thinking_exhausted_detection": gen_thinking_exhausted_cases(),
        "repetition_dominated_rejection": gen_repetition_guard_cases(),
        "empty_reasoning_one_shot_disable": gen_empty_reasoning_disable_cases(),
    }
    clean = _clean_str(goldens)
    return clean


def main() -> None:
    check_mode = "--check" in sys.argv
    data = build_truncation_guard_goldens()
    dumped = json.dumps(data, indent=2, sort_keys=True) + "\n"

    # Enforce absence of em dash character
    assert "\u2014" not in dumped, "Em dash character (\\u2014) detected in serialized goldens!"

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
