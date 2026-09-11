#!/usr/bin/env python3
"""Deterministic source-executed generator for Ollama GLM stop-to-length truncation correction.

This script executes the live Python helpers governing the conservative rewrite of
chat-completions finish_reason="stop" to "length" for Ollama-hosted GLM models:
- AIAgent._should_treat_stop_as_truncated (run_agent.py)
- AIAgent._is_ollama_glm_backend (run_agent.py)
- AIAgent._has_natural_response_ending (run_agent.py)
- agent.agent_runtime_helpers.strip_think_blocks (agent/agent_runtime_helpers.py)
- agent.conversation_loop._get_continuation_prompt (agent/conversation_loop.py)

It exhaustively covers every gate dimension and ordering rule:
1. Finish reason values (stop vs others)
2. API mode (chat_completions vs others)
3. Model / provider / backend identity
4. ollama.com and :cloud exclusions
5. History tool-message requirement
6. Assistant tool calls and presence
7. Content type validation
8. Visible think stripping and tag variants
9. 20-character and whitespace floors
10. Natural terminal punctuation (ASCII)
11. Code fences
12. Caret
13. CJK punctuation
14. Emoji threshold (ord(last) >= 0x1F300)
15. Positive unpunctuated cases
16. Downstream continuation envelope metadata
17. Exact parity with focused Python tests in tests/run_agent/test_run_agent.py

Usage:
    ./.venv/bin/python3 rust/tools/gen_ollama_glm_truncation_goldens.py          # write goldens
    ./.venv/bin/python3 rust/tools/gen_ollama_glm_truncation_goldens.py --check  # check byte parity
"""

from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/ollama-glm-truncation-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

from agent.agent_runtime_helpers import strip_think_blocks
from agent.conversation_loop import (
    _LENGTH_CONTINUATION_OUTPUT_LIMIT,
    _get_continuation_prompt,
)
from agent.transports.chat_completions import NormalizedResponse, ToolCall
from run_agent import AIAgent


def _assert_no_em_dash(val: Any) -> None:
    """Enforce strict ban on em dash characters (\u2014) across all golden values."""
    if isinstance(val, str):
        assert "\u2014" not in val, f"Em dash found in golden string: {val!r}"
    elif isinstance(val, list):
        for item in val:
            _assert_no_em_dash(item)
    elif isinstance(val, dict):
        for k, v in val.items():
            _assert_no_em_dash(k)
            _assert_no_em_dash(v)


def _make_test_agent(
    model: str = "glm-4-9b",
    provider: str = "ollama",
    base_url: str = "http://localhost:11434/v1",
    api_mode: str = "chat_completions",
) -> AIAgent:
    """Construct an AIAgent instance with mocked network client."""
    with (
        patch("run_agent.get_tool_definitions", return_value=[]),
        patch("run_agent.check_toolset_requirements", return_value={}),
        patch("run_agent.OpenAI"),
    ):
        agent = AIAgent(
            api_key="synth-primary-key",
            base_url=base_url,
            provider=provider,
            model=model,
            quiet_mode=True,
            skip_context_files=True,
            skip_memory=True,
        )
        agent.api_mode = api_mode
        agent.client = MagicMock()
        return agent


_TEST_AGENT: Optional[AIAgent] = None


def _get_shared_test_agent() -> AIAgent:
    """Retrieve or lazily construct a shared AIAgent instance."""
    global _TEST_AGENT
    if _TEST_AGENT is None:
        _TEST_AGENT = _make_test_agent()
    return _TEST_AGENT


def _eval_case(
    case_id: str,
    gate_dimension: str,
    description: str,
    model: str,
    provider: str,
    base_url: str,
    api_mode: str,
    finish_reason: Any,
    messages: Optional[List[Any]],
    assistant_message: Any,
    expected_should_rewrite: bool,
) -> Dict[str, Any]:
    """Execute live Python decision functions and record intermediate and terminal outputs."""
    agent = _get_shared_test_agent()
    agent.model = model
    agent.provider = provider
    agent.base_url = base_url
    agent._base_url_lower = base_url.lower() if base_url else ""
    agent.api_mode = api_mode

    # 1. Component evaluations
    is_stop = finish_reason == "stop"
    is_chat_completions = agent.api_mode == "chat_completions"
    is_ollama_glm = agent._is_ollama_glm_backend()

    has_tool_msg = any(
        isinstance(m, dict) and m.get("role") == "tool" for m in (messages or [])
    )

    assistant_not_none = assistant_message is not None
    assistant_tool_calls = (
        getattr(assistant_message, "tool_calls", None) if assistant_not_none else None
    )
    has_no_tool_calls = not bool(assistant_tool_calls)

    content = (
        getattr(assistant_message, "content", None) if assistant_not_none else None
    )
    content_is_str = isinstance(content, str)

    visible_text: Optional[str] = None
    visible_len: Optional[int] = None
    len_at_least_20: Optional[bool] = None
    has_whitespace: Optional[bool] = None
    has_natural_ending: Optional[bool] = None

    if content_is_str and content is not None:
        visible_text = agent._strip_think_blocks(content).strip()
        visible_len = len(visible_text)
        len_at_least_20 = visible_len >= 20
        has_whitespace = bool(re.search(r"\s", visible_text))
        if visible_text and len_at_least_20 and has_whitespace:
            has_natural_ending = agent._has_natural_response_ending(visible_text)

    # 2. Live execution of the top-level helper
    should_treat_stop_as_truncated = agent._should_treat_stop_as_truncated(
        finish_reason,
        assistant_message,
        messages,
    )

    # 3. Expected computation check (guarantees internal truth table consistency)
    expected_result = bool(
        is_stop
        and is_chat_completions
        and is_ollama_glm
        and has_tool_msg
        and assistant_not_none
        and has_no_tool_calls
        and content_is_str
        and visible_text
        and len_at_least_20
        and has_whitespace
        and not has_natural_ending
    )
    assert should_treat_stop_as_truncated == expected_result, (
        f"Case {case_id} failed parity: helper={should_treat_stop_as_truncated} vs expected={expected_result}"
    )
    assert should_treat_stop_as_truncated is expected_should_rewrite, (
        f"Case {case_id} contradicted its declared expectation: "
        f"helper={should_treat_stop_as_truncated} vs declared={expected_should_rewrite}"
    )

    # 4. Downstream continuation envelope metadata
    rewritten_finish_reason = (
        "length"
        if should_treat_stop_as_truncated
        else (str(finish_reason) if finish_reason is not None else None)
    )

    continuation_envelope: Optional[Dict[str, Any]] = None
    if should_treat_stop_as_truncated:
        prompt = _get_continuation_prompt(is_partial_stub=False, dropped_tools=None)
        assert prompt == _LENGTH_CONTINUATION_OUTPUT_LIMIT
        continuation_envelope = {
            "is_partial_stream_stub": False,
            "dropped_tools": None,
            "continuation_prompt": prompt,
            "assistant_fragment_tag": "_length_continuation_fragment",
            "user_nudge_tag": "_length_continuation_nudge",
            "retry_flag": "restart_with_length_continuation",
            "interim_assistant_message": {
                "role": "assistant",
                "content": visible_text,
                "finish_reason": "length",
                "_length_continuation_fragment": True,
            },
            "continuation_nudge_message": {
                "role": "user",
                "content": prompt,
                "_length_continuation_nudge": True,
            },
        }

    # Format assistant tool calls representation for JSON
    tool_calls_repr: Any = None
    if assistant_tool_calls is not None:
        if isinstance(assistant_tool_calls, list):
            tool_calls_repr = [
                {"id": getattr(tc, "id", None), "name": getattr(tc, "name", None)}
                for tc in assistant_tool_calls
            ]
        else:
            tool_calls_repr = str(assistant_tool_calls)

    return {
        "case_id": case_id,
        "gate_dimension": gate_dimension,
        "description": description,
        "inputs": {
            "finish_reason": finish_reason,
            "api_mode": api_mode,
            "model": model,
            "provider": provider,
            "base_url": base_url,
            "messages": messages,
            "has_tool_message_in_history": has_tool_msg,
            "assistant_content": content,
            "assistant_tool_calls": tool_calls_repr,
        },
        "intermediate_gates": {
            "finish_reason_is_stop": is_stop,
            "api_mode_is_chat_completions": is_chat_completions,
            "is_ollama_glm_backend": is_ollama_glm,
            "history_has_tool_message": has_tool_msg,
            "assistant_is_not_none": assistant_not_none,
            "assistant_has_no_tool_calls": has_no_tool_calls,
            "content_is_str": content_is_str,
            "visible_text": visible_text,
            "visible_text_len": visible_len,
            "visible_text_len_at_least_20": len_at_least_20,
            "visible_text_has_whitespace": has_whitespace,
            "has_natural_response_ending": has_natural_ending,
        },
        "outputs": {
            "should_treat_stop_as_truncated": should_treat_stop_as_truncated,
            "rewritten_finish_reason": rewritten_finish_reason,
        },
        "downstream_continuation_envelope": continuation_envelope,
        "provenance": "executed_source",
    }


def generate_cases() -> List[Dict[str, Any]]:
    """Build and execute the complete golden test corpus."""
    cases: List[Dict[str, Any]] = []

    # Standard positive fixture elements
    default_model = "glm-4-9b"
    default_provider = "ollama"
    default_base_url = "http://localhost:11434/v1"
    default_api_mode = "chat_completions"
    default_history = [
        {"role": "user", "content": "find files"},
        {"role": "tool", "content": "config.yaml, main.py"},
    ]
    positive_unpunctuated_text = "Based on the search results, the best next"
    default_positive_msg = NormalizedResponse(
        content=positive_unpunctuated_text,
        tool_calls=None,
        finish_reason="stop",
    )

    # ── 1. FINISH REASON DIMENSION ──────────────────────────────────────────
    fr_matrix = [
        ("fr_stop", "stop", True, "Standard stop finish reason eligible for rewrite"),
        (
            "fr_length",
            "length",
            False,
            "Literal length finish reason handled by native length branch",
        ),
        (
            "fr_tool_calls",
            "tool_calls",
            False,
            "Tool calls finish reason handled by tool invocation",
        ),
        (
            "fr_content_filter",
            "content_filter",
            False,
            "Content filter finish reason handled by safety handler",
        ),
        (
            "fr_error",
            "error",
            False,
            "Stream error finish reason handled by error retry",
        ),
        ("fr_none", None, False, "Missing finish reason handled by stream drop stub"),
        ("fr_empty", "", False, "Empty string finish reason treated as unknown"),
    ]
    for cid, fr, exp, desc in fr_matrix:
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="finish_reason",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason=fr,
                messages=default_history,
                assistant_message=default_positive_msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 2. API MODE DIMENSION ───────────────────────────────────────────────
    api_matrix = [
        (
            "api_chat_completions",
            "chat_completions",
            True,
            "Chat completions transport mode",
        ),
        (
            "api_anthropic_messages",
            "anthropic_messages",
            False,
            "Anthropic messages wire mode",
        ),
        ("api_codex_responses", "codex_responses", False, "Codex responses wire mode"),
        (
            "api_bedrock_converse",
            "bedrock_converse",
            False,
            "Bedrock converse wire mode",
        ),
    ]
    for cid, mode, exp, desc in api_matrix:
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="api_mode",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=default_positive_msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 3. MODEL AND PROVIDER IDENTITY DIMENSION ────────────────────────────
    backend_id_matrix = [
        (
            "id_glm4_ollama",
            "glm-4-9b",
            "ollama",
            "http://localhost:11434/v1",
            True,
            "Standard GLM-4 on local Ollama",
        ),
        (
            "id_glm51_ollama",
            "glm-5.1",
            "ollama",
            "http://localhost:11434/v1",
            True,
            "GLM-5.1 on local Ollama",
        ),
        (
            "id_glm_air_uppercase",
            "GLM-4.5-Air",
            "ollama",
            "http://localhost:11434/v1",
            True,
            "Uppercase GLM-4.5-Air model string",
        ),
        (
            "id_glm_hf_path",
            "THUDM/glm-4-9b-chat",
            "ollama",
            "http://localhost:11434/v1",
            True,
            "HuggingFace repo path containing glm",
        ),
        (
            "id_zai_custom_model",
            "custom-fast-chat",
            "zai",
            "http://localhost:11434/v1",
            True,
            "Provider zai qualifies non-glm model on local port",
        ),
        (
            "id_zai_uppercase",
            "custom-fast-chat",
            "ZAI",
            "http://localhost:11434/v1",
            True,
            "Uppercase ZAI provider matches case-insensitively",
        ),
        (
            "id_non_glm_llama",
            "llama3:8b",
            "ollama",
            "http://localhost:11434/v1",
            False,
            "Llama-3 model without GLM is excluded",
        ),
        (
            "id_non_glm_qwen",
            "qwen2.5:72b",
            "ollama",
            "http://localhost:11434/v1",
            False,
            "Qwen-2.5 model without GLM is excluded",
        ),
        (
            "id_non_glm_gpt4o",
            "gpt-4o",
            "openai",
            "http://localhost:11434/v1",
            False,
            "OpenAI model without GLM is excluded",
        ),
    ]
    for cid, m, p, u, exp, desc in backend_id_matrix:
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="model_provider_identity",
                description=desc,
                model=m,
                provider=p,
                base_url=u,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=default_positive_msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 4. OLLAMA.COM AND :CLOUD EXCLUSIONS ──────────────────────────────────
    cloud_matrix = [
        (
            "cloud_host_ollama_com",
            "glm-5.3-flash",
            "ollama-cloud",
            "https://ollama.com/v1",
            False,
            "Hosted Ollama Cloud endpoint excluded",
        ),
        (
            "cloud_host_api_ollama_com",
            "glm-4-9b",
            "ollama",
            "https://api.ollama.com/v1",
            False,
            "Subdomain api.ollama.com excluded",
        ),
        (
            "cloud_model_suffix_51",
            "glm-5.1:cloud",
            "ollama",
            "http://localhost:11434/v1",
            False,
            "Local proxy for cloud-hosted glm-5.1:cloud excluded",
        ),
        (
            "cloud_model_suffix_49",
            "glm-4-9b:cloud",
            "ollama",
            "http://localhost:11434/v1",
            False,
            "Local proxy for cloud-hosted glm-4-9b:cloud excluded",
        ),
        (
            "cloud_model_suffix_preview",
            "glm-5.3:cloud-preview",
            "ollama",
            "http://localhost:11434/v1",
            False,
            "Proxy model containing :cloud substring excluded",
        ),
        (
            "cloud_both_host_and_suffix",
            "glm-5.1:cloud",
            "ollama-cloud",
            "https://ollama.com/v1",
            False,
            "Both cloud host and cloud model suffix present",
        ),
    ]
    for cid, m, p, u, exp, desc in cloud_matrix:
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="cloud_exclusions",
                description=desc,
                model=m,
                provider=p,
                base_url=u,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=default_positive_msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 5. LOCAL SIGNATURES AND PRIVATE PROXIES ─────────────────────────────
    local_sig_matrix = [
        (
            "sig_localhost_11434",
            "glm-4-9b",
            "custom",
            "http://localhost:11434/v1",
            True,
            "Default Ollama port 11434 on localhost",
        ),
        (
            "sig_ip_11434",
            "glm-4-9b",
            "custom",
            "http://127.0.0.1:11434/v1",
            True,
            "Default Ollama port 11434 on loopback IP",
        ),
        (
            "sig_remote_ip_11434",
            "glm-4-9b",
            "custom",
            "http://192.168.1.100:11434/v1",
            True,
            "Default Ollama port 11434 on LAN IP",
        ),
        (
            "sig_ollama_in_hostname",
            "glm-4-9b",
            "custom",
            "http://ollama.local:8080/v1",
            True,
            "ollama in hostname with non-standard port",
        ),
        (
            "sig_ollama_in_path",
            "glm-4-9b",
            "custom",
            "http://proxy.internal/ollama/v1",
            True,
            "ollama in path component of base URL",
        ),
        (
            "sig_provider_ollama_custom_url",
            "glm-4-9b",
            "ollama",
            "http://ai-server:8000/v1",
            True,
            "provider explicitly set to ollama with custom URL",
        ),
        (
            "sig_private_proxy_litellm",
            "glm-4-9b",
            "litellm",
            "http://litellm.internal:8000/v1",
            False,
            "LiteLLM private proxy without ollama signatures excluded",
        ),
        (
            "sig_private_proxy_vllm",
            "glm-4-9b",
            "vllm",
            "http://vllm-node:8000/v1",
            False,
            "vLLM private endpoint without ollama signatures excluded",
        ),
        (
            "sig_private_proxy_tailscale",
            "glm-4-9b",
            "remote",
            "http://ts-node:8000/v1",
            False,
            "Tailscale private box without ollama signatures excluded",
        ),
    ]
    for cid, m, p, u, exp, desc in local_sig_matrix:
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="local_signatures",
                description=desc,
                model=m,
                provider=p,
                base_url=u,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=default_positive_msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 6. HISTORY TOOL-MESSAGE REQUIREMENT ──────────────────────────────────
    history_matrix = [
        (
            "hist_single_tool",
            [{"role": "tool", "content": "res"}],
            True,
            "Single tool role message in history",
        ),
        (
            "hist_multi_turn_with_tool",
            [
                {"role": "user", "content": "search code"},
                {"role": "assistant", "content": "", "tool_calls": [{"id": "c1"}]},
                {"role": "tool", "content": "file_contents"},
            ],
            True,
            "Full turn history containing prior tool message",
        ),
        (
            "hist_user_and_assistant_only",
            [
                {"role": "user", "content": "write a poem"},
                {"role": "assistant", "content": "first draft"},
            ],
            False,
            "History has user and assistant messages but no tool message",
        ),
        (
            "hist_user_only",
            [{"role": "user", "content": "hello"}],
            False,
            "History has only user message",
        ),
        (
            "hist_system_only",
            [{"role": "system", "content": "prompt"}],
            False,
            "History has only system prompt",
        ),
        ("hist_empty_list", [], False, "Empty history list"),
        ("hist_none", None, False, "History passed as None"),
        (
            "hist_malformed_string_items",
            ["not_a_dict", "still_not_a_dict"],
            False,
            "History items are strings rather than dicts",
        ),
        (
            "hist_dict_without_role",
            [{"content": "hello"}],
            False,
            "History item dict lacks role field",
        ),
    ]
    for cid, hist, exp, desc in history_matrix:
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="history_tool_requirement",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=hist,
                assistant_message=default_positive_msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 7. ASSISTANT TOOL CALLS AND NULL MESSAGE ─────────────────────────────
    tool_call_item = ToolCall(
        id="call_99",
        name="read_file",
        arguments='{"path": "foo.txt"}',
    )
    assistant_matrix = [
        ("asst_null", None, False, "Assistant message is None"),
        (
            "asst_with_tool_calls",
            NormalizedResponse(
                content=positive_unpunctuated_text,
                tool_calls=[tool_call_item],
                finish_reason="stop",
            ),
            False,
            "Assistant message has active tool calls",
        ),
        (
            "asst_tool_calls_none",
            NormalizedResponse(
                content=positive_unpunctuated_text,
                tool_calls=None,
                finish_reason="stop",
            ),
            True,
            "Assistant message tool_calls is None",
        ),
        (
            "asst_tool_calls_empty_list",
            NormalizedResponse(
                content=positive_unpunctuated_text, tool_calls=[], finish_reason="stop"
            ),
            True,
            "Assistant message tool_calls is empty list",
        ),
    ]
    for cid, asst, exp, desc in assistant_matrix:
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="assistant_tool_calls",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=asst,
                expected_should_rewrite=exp,
            )
        )

    # ── 8. CONTENT TYPE VALIDATION ──────────────────────────────────────────
    content_type_matrix = [
        (
            "content_type_str",
            positive_unpunctuated_text,
            True,
            "Standard string content",
        ),
        ("content_type_none", None, False, "None content"),
        (
            "content_type_list",
            [{"type": "text", "text": positive_unpunctuated_text}],
            False,
            "List of content blocks rejected before think strip",
        ),
        (
            "content_type_dict",
            {"text": positive_unpunctuated_text},
            False,
            "Dict content rejected before think strip",
        ),
        ("content_type_int", 42, False, "Integer content rejected before think strip"),
    ]
    for cid, raw_c, exp, desc in content_type_matrix:
        msg = SimpleNamespace(content=raw_c, tool_calls=None)
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="content_type",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 9. VISIBLE THINK STRIPPING AND TAG VARIANTS ─────────────────────────
    think_matrix = [
        (
            "think_tag_think",
            f"<think>internal step by step plan</think>{positive_unpunctuated_text}",
            True,
            "Standard <think> block stripped",
        ),
        (
            "think_tag_thinking",
            f"<thinking>evaluating best approach</thinking>{positive_unpunctuated_text}",
            True,
            "Variant <thinking> block stripped",
        ),
        (
            "think_tag_reasoning",
            f"<reasoning>deducing next steps</reasoning>{positive_unpunctuated_text}",
            True,
            "Variant <reasoning> block stripped",
        ),
        (
            "think_tag_thought",
            f"<thought>Gemma 4 model reflection</thought>{positive_unpunctuated_text}",
            True,
            "Variant <thought> block stripped",
        ),
        (
            "think_tag_scratchpad",
            f"<REASONING_SCRATCHPAD>scratch notes</REASONING_SCRATCHPAD>{positive_unpunctuated_text}",
            True,
            "Variant <REASONING_SCRATCHPAD> block stripped",
        ),
        (
            "think_tag_uppercase",
            f"<THINK>all caps thinking tag</THINK>{positive_unpunctuated_text}",
            True,
            "Mixed or uppercase reasoning tags stripped",
        ),
        (
            "think_tag_multiline",
            f"<think>\nline 1\nline 2\n</think>\n{positive_unpunctuated_text}",
            True,
            "Multiline reasoning block stripped",
        ),
        (
            "think_only_exhaustion",
            "<think>the model thought for the entire context budget</think>",
            False,
            "Content contains only reasoning with no visible text",
        ),
        (
            "think_unterminated",
            "<think>the model emitted thinking that was never closed",
            False,
            "Unterminated reasoning block strips all trailing text",
        ),
        (
            "think_tool_call_xml",
            f'<tool_call>{{"name": "exec"}}</tool_call>{positive_unpunctuated_text}',
            True,
            "Standalone tool_call XML block stripped",
        ),
    ]
    for cid, raw_c, exp, desc in think_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="visible_think_stripping",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 10. 20-CHARACTER AND WHITESPACE FLOORS ──────────────────────────────
    floor_matrix = [
        (
            "floor_under_20_chars",
            "Short reply",
            False,
            "Length 11 characters is below 20 character minimum",
        ),
        (
            "floor_exactly_19_chars",
            "Nineteen characters",
            False,
            "Length 19 characters is below 20 character minimum",
        ),
        (
            "floor_exactly_20_chars_with_space",
            "Twenty characters ok",
            True,
            "Length exactly 20 characters with space qualifies",
        ),
        (
            "floor_over_20_no_whitespace",
            "Supercalifragilisticexpialidocious",
            False,
            "Length 34 characters but no whitespace character",
        ),
        (
            "floor_repeated_char_no_space",
            "A" * 30,
            False,
            "Length 30 characters without space rejected",
        ),
        (
            "floor_multiline_whitespace",
            "First line here\nSecond line",
            True,
            "Newline counts as whitespace satisfying regex",
        ),
        (
            "floor_tab_whitespace",
            "First\tword\tsecond\tthird",
            True,
            "Tab counts as whitespace satisfying regex",
        ),
    ]
    for cid, raw_c, exp, desc in floor_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="length_and_whitespace_floors",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 11. NATURAL TERMINAL PUNCTUATION (ASCII) ────────────────────────────
    # In natural ending checks: has_natural_ending=True means NOT truncated (stays stop)
    ascii_punct_matrix = [
        (
            "term_ascii_period",
            "Based on the search results, the best next step is done.",
            False,
            "Ends with ASCII period .",
        ),
        (
            "term_ascii_exclamation",
            "Based on the search results, the best next step is done!",
            False,
            "Ends with ASCII exclamation !",
        ),
        (
            "term_ascii_question",
            "Based on the search results, should we proceed?",
            False,
            "Ends with ASCII question ?",
        ),
        (
            "term_ascii_colon",
            "Based on the search results, here are options:",
            False,
            "Ends with ASCII colon :",
        ),
        (
            "term_ascii_closing_paren",
            "Based on the search results (updated version)",
            False,
            "Ends with ASCII closing paren )",
        ),
        (
            "term_ascii_double_quote",
            'Based on the search results "complete"',
            False,
            'Ends with ASCII double quote "',
        ),
        (
            "term_ascii_single_quote",
            "Based on the search results 'complete'",
            False,
            "Ends with ASCII single quote '",
        ),
        (
            "term_ascii_closing_bracket",
            "Based on the search results [status: ready]",
            False,
            "Ends with ASCII closing bracket ]",
        ),
        (
            "term_ascii_closing_brace",
            "Based on the search results {status: ready}",
            False,
            "Ends with ASCII closing brace }",
        ),
        (
            "term_ascii_period_trailing_whitespace",
            "Based on the search results, update is complete.  \n\t",
            False,
            "ASCII period followed by trailing whitespace",
        ),
    ]
    for cid, raw_c, exp, desc in ascii_punct_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="natural_terminal_punctuation_ascii",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 12. CODE FENCES ─────────────────────────────────────────────────────
    code_fence_matrix = [
        (
            "code_fence_block",
            "Here is the python snippet:\n```python\nprint('hello')\n```",
            False,
            "Ends with code fence ```",
        ),
        (
            "code_fence_trailing_whitespace",
            "Here is the python snippet:\n```  \n",
            False,
            "Ends with code fence ``` plus trailing whitespace",
        ),
        (
            "code_inline_backtick_unpunctuated",
            "Use the function `run_process`",
            True,
            "Ends with single backtick which is not a fence or terminal punct",
        ),
    ]
    for cid, raw_c, exp, desc in code_fence_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="code_fences",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 13. CARET ───────────────────────────────────────────────────────────
    caret_matrix = [
        (
            "caret_ending",
            "See reference note in appendix [1]^",
            False,
            "Ends with caret ^",
        ),
        (
            "caret_trailing_whitespace",
            "See reference note in appendix [2]^ \n",
            False,
            "Ends with caret ^ plus trailing whitespace",
        ),
    ]
    for cid, raw_c, exp, desc in caret_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="caret",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 14. CJK PUNCTUATION ─────────────────────────────────────────────────
    cjk_punct_matrix = [
        (
            "cjk_fullwidth_period",
            "根據搜索結果，下一步是更新配置。",
            False,
            "Ends with CJK fullwidth period 。 (U+3002)",
        ),
        (
            "cjk_fullwidth_exclamation",
            "系統配置檢查已經順利完成！",
            False,
            "Ends with CJK fullwidth exclamation ！ (U+FF01)",
        ),
        (
            "cjk_fullwidth_question",
            "請問是否需要繼續執行下一步操作？",
            False,
            "Ends with CJK fullwidth question ？ (U+FF1F)",
        ),
        (
            "cjk_fullwidth_colon",
            "搜索結果已返回，具體詳情如下：",
            False,
            "Ends with CJK fullwidth colon ： (U+FF1A)",
        ),
        (
            "cjk_fullwidth_closing_paren",
            "配置更新已生效（參見官方文檔）",
            False,
            "Ends with CJK fullwidth closing paren ） (U+FF09)",
        ),
        (
            "cjk_closing_lenticular_bracket",
            "系統狀態更新完畢【全部通過】",
            False,
            "Ends with CJK right black lenticular bracket 】 (U+3011)",
        ),
        (
            "cjk_closing_corner_bracket",
            "已成功確認當前部署命令「執行完畢」",
            False,
            "Ends with CJK right corner bracket 」 (U+300D)",
        ),
        (
            "cjk_closing_white_corner_bracket",
            "已驗證最新版本日誌『無錯誤』",
            False,
            "Ends with CJK right white corner bracket 』 (U+300F)",
        ),
        (
            "cjk_closing_double_angle_bracket",
            "請詳細閱讀系統配置說明《使用手冊》",
            False,
            "Ends with CJK right double angle bracket 》 (U+300B)",
        ),
        (
            "cjk_punct_trailing_whitespace",
            "下一步操作指引詳見文檔。   \n",
            False,
            "CJK punctuation followed by trailing whitespace",
        ),
    ]
    for cid, raw_c, exp, desc in cjk_punct_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="cjk_punctuation",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 15. EMOJI THRESHOLD (ord(last) >= 0x1F300) ──────────────────────────
    emoji_matrix = [
        (
            "emoji_party_popper_1f389",
            "Here is your requested update 🎉",
            False,
            "U+1F389 party popper ord 0x1F389 >= 0x1F300 (natural ending)",
        ),
        (
            "emoji_rocket_1f680",
            "System launched successfully 🚀",
            False,
            "U+1F680 rocket ord 0x1F680 >= 0x1F300 (natural ending)",
        ),
        (
            "emoji_thumbs_up_1f44d",
            "Changes have been approved 👍",
            False,
            "U+1F44D thumbs up ord 0x1F44D >= 0x1F300 (natural ending)",
        ),
        (
            "emoji_cyclone_1f300_exact_boundary",
            "Processing pipeline completed 🌀",
            False,
            "U+1F300 cyclone ord exactly 0x1F300 (boundary natural ending)",
        ),
        (
            "dingbat_check_mark_2705_below_threshold",
            "Task finished successfully ✅",
            True,
            "U+2705 check mark ord 0x2705 < 0x1F300 treated as truncated",
        ),
        (
            "dingbat_sparkles_2728_below_threshold",
            "Feature deployed cleanly ✨",
            True,
            "U+2728 sparkles ord 0x2728 < 0x1F300 treated as truncated",
        ),
        (
            "symbol_warning_26a0_below_threshold",
            "Process completed with notice ⚠️",
            True,
            "U+26A0 warning sign ord 0x26A0 < 0x1F300 treated as truncated",
        ),
        (
            "cjk_enclosed_1f251_below_threshold",
            "Status confirmed valid 🉑",
            True,
            "U+1F251 CJK enclosed accept ord 0x1F251 < 0x1F300 treated as truncated",
        ),
    ]
    for cid, raw_c, exp, desc in emoji_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="emoji_threshold",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 16. POSITIVE UNPUNCTUATED CASES ─────────────────────────────────────
    positive_matrix = [
        (
            "pos_unpunctuated_english",
            "Based on the search results, the best next",
            True,
            "Cut off mid-sentence ending with letter t",
        ),
        (
            "pos_unpunctuated_chinese",
            "根據搜索結果 下一步需要更新本地系統配置文件內容",
            True,
            "Chinese text with whitespace cut off on unpunctuated character 容",
        ),
        (
            "pos_trailing_comma",
            "Based on the search results, next steps include,",
            True,
            "Cut off ending with comma , which is non-terminal punctuation",
        ),
        (
            "pos_trailing_semicolon",
            "First step completed; second step ongoing;",
            True,
            "Cut off ending with semicolon ; which is non-terminal punctuation",
        ),
        (
            "pos_trailing_digit",
            "Current progress count is 42",
            True,
            "Cut off ending with numeric digit 2",
        ),
        (
            "pos_trailing_hyphen",
            "Configuration option set to non-",
            True,
            "Cut off ending with hyphen -",
        ),
        (
            "pos_trailing_slash",
            "Configuration file path is /etc/",
            True,
            "Cut off ending with slash /",
        ),
    ]
    for cid, raw_c, exp, desc in positive_matrix:
        msg = NormalizedResponse(content=raw_c, tool_calls=None, finish_reason="stop")
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="positive_unpunctuated",
                description=desc,
                model=default_model,
                provider=default_provider,
                base_url=default_base_url,
                api_mode=default_api_mode,
                finish_reason="stop",
                messages=default_history,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    # ── 17. FOCUSED TESTS PARITY (tests/run_agent/test_run_agent.py) ────────
    # Exact mirror of test_ollama_glm_stop_after_tools_without_terminal_boundary_requests_continuation
    test_run_agent_cases = [
        (
            "focused_ollama_glm_stop_after_tools_truncated",
            "glm-4-9b",
            "ollama",
            "http://localhost:11434/v1",
            "Based on the search results, the best next",
            [{"role": "tool", "content": "search result"}],
            True,
            "Exact parity with test_run_agent.py line 4346: local GLM stop rewritten to length",
        ),
        (
            "focused_ollama_cloud_host_never_rewritten",
            "glm-5.3-flash",
            "ollama-cloud",
            "https://ollama.com/v1",
            "Based on the results the best next step is to update the config",
            [{"role": "tool", "content": "r"}],
            False,
            "Exact parity with test_run_agent.py line 4392: ollama.com host is never rewritten",
        ),
        (
            "focused_ollama_local_cloud_model_never_rewritten",
            "glm-5.1:cloud",
            "ollama",
            "http://localhost:11434/v1",
            "Based on the results the best next step is to update the config",
            [{"role": "tool", "content": "r"}],
            False,
            "Exact parity with test_run_agent.py line 4393: :cloud model on local port is never rewritten",
        ),
    ]
    for cid, m, p, u, text, hist, exp, desc in test_run_agent_cases:
        msg = SimpleNamespace(content=text, tool_calls=None)
        cases.append(
            _eval_case(
                case_id=cid,
                gate_dimension="focused_tests_parity",
                description=desc,
                model=m,
                provider=p,
                base_url=u,
                api_mode="chat_completions",
                finish_reason="stop",
                messages=hist,
                assistant_message=msg,
                expected_should_rewrite=exp,
            )
        )

    return cases


def build_golden_document() -> Dict[str, Any]:
    """Assemble top-level golden JSON payload."""
    cases = generate_cases()

    positive_cases = [
        c for c in cases if c["outputs"]["should_treat_stop_as_truncated"]
    ]
    negative_cases = [
        c for c in cases if not c["outputs"]["should_treat_stop_as_truncated"]
    ]

    doc = {
        "metadata": {
            "contract": "rust/analysis/ollama-glm-truncation-contract-agy.md",
            "generator": "rust/tools/gen_ollama_glm_truncation_goldens.py",
            "description": (
                "Deterministic behavioral goldens for Python local Ollama/GLM "
                "finish_reason='stop' to 'length' truncation correction."
            ),
            "source_helpers": [
                "run_agent.py:AIAgent._should_treat_stop_as_truncated",
                "run_agent.py:AIAgent._is_ollama_glm_backend",
                "run_agent.py:AIAgent._has_natural_response_ending",
                "agent/agent_runtime_helpers.py:strip_think_blocks",
                "agent/conversation_loop.py:_get_continuation_prompt",
                "agent/conversation_loop.py:_LENGTH_CONTINUATION_OUTPUT_LIMIT",
            ],
            "total_cases": len(cases),
            "positive_cases_count": len(positive_cases),
            "negative_cases_count": len(negative_cases),
            "continuation_prompt_constant": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        },
        "evaluation_gate_order": [
            "1. finish_reason == 'stop'",
            "2. api_mode == 'chat_completions'",
            "3. _is_ollama_glm_backend (model has 'glm' or provider is 'zai', exclude 'ollama.com' and ':cloud', match 'ollama' or ':11434' in URL or provider == 'ollama')",
            "4. history has at least one message with role == 'tool'",
            "5. assistant_message is not None and assistant_message has no active tool_calls",
            "6. assistant content is a string",
            "7. visible text after _strip_think_blocks is non-empty",
            "8. visible text length >= 20 and contains at least one whitespace character",
            "9. not _has_natural_response_ending(visible_text) (not code fence, caret, terminal punct, or emoji >= 0x1F300)",
        ],
        "continuation_envelope_specification": {
            "trigger_condition": "should_treat_stop_as_truncated == true",
            "target_finish_reason": "length",
            "is_partial_stream_stub": False,
            "dropped_tools": None,
            "continuation_prompt": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
            "assistant_fragment_tag": "_length_continuation_fragment",
            "user_nudge_tag": "_length_continuation_nudge",
            "retry_flag": "restart_with_length_continuation",
        },
        "cases": cases,
    }

    _assert_no_em_dash(doc)
    return doc


def main() -> None:
    doc = build_golden_document()
    rendered = json.dumps(doc, ensure_ascii=False, indent=2, sort_keys=True) + "\n"

    # Strict check for em dash character anywhere in rendered text
    assert "\u2014" not in rendered, "Em dash character detected in rendered output"

    if "--check" in sys.argv:
        if not OUT.exists():
            print(f"FAIL: {OUT} does not exist", file=sys.stderr)
            sys.exit(1)
        current = OUT.read_text(encoding="utf-8")
        if current != rendered:
            print(
                f"FAIL: {OUT} does not match generated output byte-for-byte",
                file=sys.stderr,
            )
            sys.exit(1)
        print(
            f"OK: {OUT} matches generated goldens byte-for-byte ({doc['metadata']['total_cases']} cases)"
        )
        sys.exit(0)

    OUT.write_text(rendered, encoding="utf-8")
    print(
        f"Wrote {doc['metadata']['total_cases']} goldens ({doc['metadata']['positive_cases_count']} positive, {doc['metadata']['negative_cases_count']} negative) to {OUT}"
    )


if __name__ == "__main__":
    main()
