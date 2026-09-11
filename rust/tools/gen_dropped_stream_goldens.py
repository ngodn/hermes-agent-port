#!/usr/bin/env python3
"""Deterministic source-executed oracle for dropped chat-completions stream recovery.

This generator executes and audits live Python behavior governing chat-completions
SSE stream drops, partial recovery, continuation prompting, and loop consumption:
1. Clean EOF with visible text and no finish reason (producer stub creation).
2. Usage-object presence including zero counts (#91373 completion discriminator).
3. Final lastOne terminal signals (#90848 clean stop without [DONE]).
4. Zero-output EOF (EmptyStreamError raise guard vs empty valid completion).
5. Provider-reported finish reasons as exclusions (stop, length, tool_calls, content_filter, merged).
6. Transport error boundaries before and after visible deltas (re-raise vs stub recovery).
7. Partial text and reasoning preservation (content, reasoning_content, tool suppression).
8. Continuation prompt selection (network stub vs dropped tools vs output limit).
9. Empty-stub interim suppression (avoiding HTTP 400 empty assistant turns on replay).
10. Retry and ceiling metadata (4-retry budget, fragment stripping, ceiling partial exit).
11. Content-filter tagging and eager fallback escalation (MiniMax, Azure, OpenAI policy errors).

Usage:
    ./.venv/bin/python3 rust/tools/gen_dropped_stream_goldens.py          # write goldens
    ./.venv/bin/python3 rust/tools/gen_dropped_stream_goldens.py --check  # check byte parity
"""

from __future__ import annotations

import copy
import json
import os
import sys
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/dropped-stream-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

from hermes_constants import (
    FINISH_REASON_LENGTH,
    PARTIAL_STREAM_STUB_ID,
)
from agent.chat_completion_helpers import (
    _build_partial_stream_stub,
    _consume_ephemeral_reasoning_off,
    _repair_tool_call_arguments,
)
from agent.conversation_loop import (
    _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX,
    _LENGTH_CONTINUATION_NETWORK_STUB,
    _LENGTH_CONTINUATION_OUTPUT_LIMIT,
    _get_continuation_prompt,
    _join_truncated_parts,
)
from agent.error_classifier import (
    FailoverReason,
    classify_api_error,
)
from agent.errors import EmptyStreamError
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


def _make_stream_chunk(
    content: Optional[str] = None,
    tool_calls: Optional[List[Any]] = None,
    finish_reason: Optional[str] = None,
    reasoning_content: Optional[str] = None,
    reasoning: Optional[str] = None,
    model: str = "test-model",
    usage: Optional[Any] = None,
    last_one: Optional[Any] = None,
    model_extra: Optional[Dict[str, Any]] = None,
) -> SimpleNamespace:
    """Create an SSE stream chunk mimicking an OpenAI delta."""
    delta = SimpleNamespace(
        content=content,
        tool_calls=tool_calls,
        reasoning_content=reasoning_content,
        reasoning=reasoning,
    )
    choice = SimpleNamespace(index=0, delta=delta, finish_reason=finish_reason)
    chunk = SimpleNamespace(choices=[choice], model=model, usage=usage)
    if last_one is not None:
        chunk.lastOne = last_one
    if model_extra is not None:
        chunk.model_extra = model_extra
    return chunk


def _make_tool_call_delta(
    index: int = 0,
    tc_id: Optional[str] = None,
    name: Optional[str] = None,
    arguments: Optional[str] = None,
) -> SimpleNamespace:
    """Create a tool call delta SimpleNamespace."""
    func = SimpleNamespace(name=name, arguments=arguments)
    return SimpleNamespace(index=index, id=tc_id, function=func, type="function")


def _make_test_agent(
    model: str = "test/model", provider: str = "openrouter"
) -> AIAgent:
    """Instantiate a minimal AIAgent configured for chat_completions streaming."""
    agent = AIAgent(
        api_key="test-key-oracle-12345",
        base_url="https://example.com/v1",
        model=model,
        provider=provider,
        quiet_mode=True,
        skip_context_files=True,
        skip_memory=True,
    )
    agent.api_mode = "chat_completions"
    agent._interrupt_requested = False
    agent._fire_stream_delta = lambda text: None
    return agent


def _make_loop_agent(
    model: str = "test/model", provider: str = "openrouter"
) -> AIAgent:
    """Instantiate a minimal AIAgent configured for driving the live conversation loop."""
    with (
        patch("run_agent.get_tool_definitions", return_value=[]),
        patch("run_agent.check_toolset_requirements", return_value={}),
        patch("run_agent.OpenAI"),
    ):
        agent = AIAgent(
            api_key="test-key-oracle-12345",
            base_url="https://openrouter.ai/api/v1",
            model=model,
            provider=provider,
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
    agent._interrupt_requested = False
    agent.suppress_status_output = True
    agent._emit_status = lambda *a, **kw: None
    agent._should_start_quiet_spinner = lambda: False
    return agent


def _execute_stream_call(
    chunks: List[Any],
    current_streamed_text: str = "",
    agent: Optional[AIAgent] = None,
    stream_error: Optional[Exception] = None,
) -> Any:
    """Execute live agent._interruptible_streaming_api_call with given chunks or error."""
    if agent is None:
        agent = _make_test_agent()
    agent._current_streamed_assistant_text = current_streamed_text

    def _stream_gen():
        for ch in chunks:
            yield ch
        if stream_error is not None:
            raise stream_error

    mock_client = MagicMock()
    mock_client.chat.completions.create.side_effect = lambda *a, **kw: _stream_gen()
    agent._create_request_openai_client = lambda *a, **kw: mock_client
    agent._close_request_openai_client = lambda *a, **kw: None

    old_retries = os.environ.get("HERMES_STREAM_RETRIES")
    os.environ["HERMES_STREAM_RETRIES"] = "0"
    try:
        return agent._interruptible_streaming_api_call({})
    finally:
        if old_retries is None:
            os.environ.pop("HERMES_STREAM_RETRIES", None)
        else:
            os.environ["HERMES_STREAM_RETRIES"] = old_retries


# ==============================================================================
# Section 1: Clean EOF with Visible Text and No Finish Reason
# ==============================================================================
def gen_clean_eof_stream_recovery_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Pure text delivery, clean EOF without finish reason or usage
    raw_chunks = [
        {"content": "Partial prose delivered before EOF.", "finish_reason": None}
    ]
    stream_chunks = [
        _make_stream_chunk(
            content="Partial prose delivered before EOF.", finish_reason=None
        )
    ]
    resp = _execute_stream_call(
        stream_chunks, current_streamed_text="Partial prose delivered before EOF."
    )
    expected_case1 = {
        "case_id": "clean_eof_text_only_drop",
        "description": "Text delivered but SSE ends cleanly with no finish_reason and no usage",
        "raw_chunks": raw_chunks,
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_content": "Partial prose delivered before EOF.",
        "expected_tool_calls": None,
        "expected_dropped_tool_names": None,
        "is_stub": True,
    }
    assert getattr(resp, "id", None) == expected_case1["expected_response_id"]
    assert resp.choices[0].finish_reason == expected_case1["expected_finish_reason"]
    assert resp.choices[0].message.content == expected_case1["expected_content"]
    assert resp.choices[0].message.tool_calls is expected_case1["expected_tool_calls"]
    assert (
        getattr(resp, "_dropped_tool_names", None)
        == expected_case1["expected_dropped_tool_names"]
    )
    assert (getattr(resp, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case1[
        "is_stub"
    ]
    cases.append(expected_case1)

    # Case 2: Incomplete tool arguments (brace only), clean EOF without finish reason
    raw_chunks_tc = [
        {"type": "text", "content": "\n"},
        {"type": "tool_call_start", "id": "call_x", "name": "execute_code"},
        {"type": "tool_call_args", "arguments": '{\n  "command": "ls'},
    ]
    stream_chunks_tc = [
        _make_stream_chunk(content="\n"),
        _make_stream_chunk(
            tool_calls=[_make_tool_call_delta(tc_id="call_x", name="execute_code")]
        ),
        _make_stream_chunk(
            tool_calls=[_make_tool_call_delta(arguments='{\n  "command": "ls')]
        ),
    ]
    resp_tc = _execute_stream_call(stream_chunks_tc)
    expected_case2 = {
        "case_id": "clean_eof_incomplete_tool_args_drop",
        "description": "Tool call name and opening unclosed JSON delivered before stream terminates",
        "raw_chunks": raw_chunks_tc,
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_content": "\n",
        "expected_tool_calls": None,
        "expected_dropped_tool_names": ["execute_code"],
        "is_stub": True,
    }
    assert getattr(resp_tc, "id", None) == expected_case2["expected_response_id"]
    assert resp_tc.choices[0].finish_reason == expected_case2["expected_finish_reason"]
    assert resp_tc.choices[0].message.content == expected_case2["expected_content"]
    assert (
        resp_tc.choices[0].message.tool_calls is expected_case2["expected_tool_calls"]
    )
    assert (
        getattr(resp_tc, "_dropped_tool_names", None)
        == expected_case2["expected_dropped_tool_names"]
    )
    assert (getattr(resp_tc, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case2[
        "is_stub"
    ]
    cases.append(expected_case2)

    # Case 3: Zero-byte tool arguments (#80498), clean EOF without finish reason
    raw_chunks_zero = [
        {"type": "tool_call_start", "id": "call_zero", "name": "write_file"}
    ]
    stream_chunks_zero = [
        _make_stream_chunk(
            tool_calls=[_make_tool_call_delta(tc_id="call_zero", name="write_file")]
        )
    ]
    resp_zero = _execute_stream_call(stream_chunks_zero)
    expected_case3 = {
        "case_id": "clean_eof_zero_byte_tool_args_drop",
        "description": "Tool call name arrived but zero argument bytes streamed before clean EOF",
        "raw_chunks": raw_chunks_zero,
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_content": None,
        "expected_tool_calls": None,
        "expected_dropped_tool_names": ["write_file"],
        "is_stub": True,
    }
    assert getattr(resp_zero, "id", None) == expected_case3["expected_response_id"]
    assert (
        resp_zero.choices[0].finish_reason == expected_case3["expected_finish_reason"]
    )
    assert (
        getattr(resp_zero.choices[0].message, "content", None)
        == expected_case3["expected_content"]
    )
    assert (
        resp_zero.choices[0].message.tool_calls is expected_case3["expected_tool_calls"]
    )
    assert (
        getattr(resp_zero, "_dropped_tool_names", None)
        == expected_case3["expected_dropped_tool_names"]
    )
    assert (getattr(resp_zero, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case3[
        "is_stub"
    ]
    cases.append(expected_case3)

    # Case 4: Mixed parallel tool calls: one complete, one zero-byte (#80498)
    raw_chunks_mixed = [
        {
            "type": "tool_call_0",
            "name": "read_file",
            "arguments": '{"path": "config.yaml"}',
        },
        {"type": "tool_call_1", "name": "write_file", "arguments": ""},
    ]
    stream_chunks_mixed = [
        _make_stream_chunk(
            tool_calls=[
                _make_tool_call_delta(index=0, tc_id="call_0", name="read_file")
            ]
        ),
        _make_stream_chunk(
            tool_calls=[
                _make_tool_call_delta(index=0, arguments='{"path": "config.yaml"}')
            ]
        ),
        _make_stream_chunk(
            tool_calls=[
                _make_tool_call_delta(index=1, tc_id="call_1", name="write_file")
            ]
        ),
    ]
    resp_mixed = _execute_stream_call(stream_chunks_mixed)
    expected_case4 = {
        "case_id": "clean_eof_mixed_tool_calls_all_or_nothing_drop",
        "description": "One completed tool call and one dropped zero-byte tool call discards whole response as stub",
        "raw_chunks": raw_chunks_mixed,
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_tool_calls": None,
        "expected_dropped_tool_names": ["read_file", "write_file"],
        "is_stub": True,
    }
    assert getattr(resp_mixed, "id", None) == expected_case4["expected_response_id"]
    assert (
        resp_mixed.choices[0].finish_reason == expected_case4["expected_finish_reason"]
    )
    assert (
        resp_mixed.choices[0].message.tool_calls
        is expected_case4["expected_tool_calls"]
    )
    assert (
        getattr(resp_mixed, "_dropped_tool_names", None)
        == expected_case4["expected_dropped_tool_names"]
    )
    assert (
        getattr(resp_mixed, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case4["is_stub"]
    cases.append(expected_case4)

    # Case 5: Repaired tool call arguments complete cleanly without stub
    unrepaired_args = '{"path": "hello.txt",}'
    repaired_args = _repair_tool_call_arguments(unrepaired_args, "read_file")
    stream_chunks_repaired = [
        _make_stream_chunk(
            tool_calls=[_make_tool_call_delta(index=0, tc_id="c_rep", name="read_file")]
        ),
        _make_stream_chunk(
            tool_calls=[_make_tool_call_delta(index=0, arguments=unrepaired_args)]
        ),
        _make_stream_chunk(finish_reason="tool_calls"),
    ]
    resp_repaired = _execute_stream_call(stream_chunks_repaired)
    expected_case5 = {
        "case_id": "repaired_tool_args_clean_finish",
        "description": "Tool call arguments with repairable trailing comma survive repair and complete normally",
        "raw_arguments": unrepaired_args,
        "repaired_arguments": repaired_args,
        "expected_response_id_is_not_stub": True,
        "expected_finish_reason": "tool_calls",
        "is_stub": False,
    }
    assert repaired_args == expected_case5["repaired_arguments"]
    assert (
        getattr(resp_repaired, "id", None) != PARTIAL_STREAM_STUB_ID
    ) is expected_case5["expected_response_id_is_not_stub"]
    assert (
        resp_repaired.choices[0].finish_reason
        == expected_case5["expected_finish_reason"]
    )
    assert (
        resp_repaired.choices[0].message.tool_calls[0].function.arguments
        == expected_case5["repaired_arguments"]
    )
    assert (
        getattr(resp_repaired, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case5["is_stub"]
    cases.append(expected_case5)

    return cases


# ==============================================================================
# Section 2: Usage-Object Presence Semantics (#91373)
# ==============================================================================
def gen_usage_object_presence_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Final usage chunk with positive tokens proves clean stop (#91373)
    raw_usage_std = {"prompt_tokens": 100, "completion_tokens": 15, "total_tokens": 115}
    std_usage_obj = SimpleNamespace(
        prompt_tokens=100, completion_tokens=15, total_tokens=115
    )
    chunks_std = [
        _make_stream_chunk(content="Complete response verified by usage."),
        SimpleNamespace(choices=[], model="vllm/model", usage=std_usage_obj),
    ]
    resp_std = _execute_stream_call(chunks_std)
    expected_case1 = {
        "case_id": "final_usage_chunk_standard_tokens_is_clean_stop",
        "description": "Trailing usage chunk with choices=[] and nonzero tokens proves completion",
        "raw_usage": raw_usage_std,
        "expected_finish_reason": "stop",
        "expected_content": "Complete response verified by usage.",
        "expected_has_usage": True,
        "expected_prompt_tokens": 100,
        "expected_completion_tokens": 15,
        "is_stub": False,
    }
    assert getattr(resp_std, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (getattr(resp_std, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case1[
        "is_stub"
    ]
    assert resp_std.choices[0].finish_reason == expected_case1["expected_finish_reason"]
    assert resp_std.choices[0].message.content == expected_case1["expected_content"]
    assert (getattr(resp_std, "usage", None) is not None) is expected_case1[
        "expected_has_usage"
    ]
    assert resp_std.usage.prompt_tokens == expected_case1["expected_prompt_tokens"]
    assert (
        resp_std.usage.completion_tokens == expected_case1["expected_completion_tokens"]
    )
    cases.append(expected_case1)

    # Case 2: Final usage chunk with zero tokens proves clean stop (presence not magnitude)
    raw_usage_zero = {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    zero_usage_obj = SimpleNamespace(
        prompt_tokens=0, completion_tokens=0, total_tokens=0
    )
    chunks_zero = [
        _make_stream_chunk(content="Zero token usage chunk test."),
        SimpleNamespace(choices=[], model="vllm/model", usage=zero_usage_obj),
    ]
    resp_zero = _execute_stream_call(chunks_zero)
    expected_case2 = {
        "case_id": "final_usage_chunk_zero_tokens_is_clean_stop",
        "description": "Trailing usage chunk with choices=[] and zero tokens proves completion by object presence",
        "raw_usage": raw_usage_zero,
        "expected_finish_reason": "stop",
        "expected_content": "Zero token usage chunk test.",
        "expected_has_usage": True,
        "expected_prompt_tokens": 0,
        "expected_completion_tokens": 0,
        "is_stub": False,
    }
    assert getattr(resp_zero, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (getattr(resp_zero, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case2[
        "is_stub"
    ]
    assert (
        resp_zero.choices[0].finish_reason == expected_case2["expected_finish_reason"]
    )
    assert resp_zero.choices[0].message.content == expected_case2["expected_content"]
    assert (getattr(resp_zero, "usage", None) is not None) is expected_case2[
        "expected_has_usage"
    ]
    assert resp_zero.usage.prompt_tokens == expected_case2["expected_prompt_tokens"]
    assert (
        resp_zero.usage.completion_tokens
        == expected_case2["expected_completion_tokens"]
    )
    cases.append(expected_case2)

    # Case 3: Inline usage object on final content chunk proves clean stop
    inline_usage_obj = SimpleNamespace(
        prompt_tokens=50, completion_tokens=10, total_tokens=60
    )
    chunks_inline = [
        _make_stream_chunk(
            content="Inline usage present.", usage=inline_usage_obj, finish_reason=None
        )
    ]
    resp_inline = _execute_stream_call(chunks_inline)
    expected_case3 = {
        "case_id": "inline_usage_on_content_chunk_is_clean_stop",
        "description": "Usage attached to content chunk proves stream completed cleanly despite finish_reason None",
        "expected_finish_reason": "stop",
        "expected_content": "Inline usage present.",
        "expected_has_usage": True,
        "is_stub": False,
    }
    assert getattr(resp_inline, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (
        getattr(resp_inline, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case3["is_stub"]
    assert (
        resp_inline.choices[0].finish_reason == expected_case3["expected_finish_reason"]
    )
    assert resp_inline.choices[0].message.content == expected_case3["expected_content"]
    assert (getattr(resp_inline, "usage", None) is not None) is expected_case3[
        "expected_has_usage"
    ]
    assert resp_inline.usage.prompt_tokens == 50
    cases.append(expected_case3)

    # Case 4: No usage chunk at all and no finish reason routes to stub
    chunks_no_usage = [_make_stream_chunk(content="Abrupt drop without usage.")]
    resp_no_usage = _execute_stream_call(chunks_no_usage)
    expected_case4 = {
        "case_id": "missing_usage_and_no_finish_routes_to_stub",
        "description": "When usage_obj is None and finish_reason is None, drop detector classifies as stub",
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_content": "Abrupt drop without usage.",
        "expected_has_usage": False,
        "is_stub": True,
    }
    assert getattr(resp_no_usage, "id", None) == expected_case4["expected_response_id"]
    assert (
        resp_no_usage.choices[0].finish_reason
        == expected_case4["expected_finish_reason"]
    )
    assert (
        resp_no_usage.choices[0].message.content == expected_case4["expected_content"]
    )
    assert (getattr(resp_no_usage, "usage", None) is not None) is expected_case4[
        "expected_has_usage"
    ]
    assert (
        getattr(resp_no_usage, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case4["is_stub"]
    cases.append(expected_case4)

    return cases


# ==============================================================================
# Section 3: Final lastOne Signals (#90848)
# ==============================================================================
def gen_final_last_one_signal_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: lastOne=True on choices=[] chunk
    chunks_bool_true = [
        _make_stream_chunk(content="Longcat stream complete."),
        SimpleNamespace(choices=[], model="meituan/longcat", lastOne=True),
    ]
    resp_bool = _execute_stream_call(chunks_bool_true)
    expected_case1 = {
        "case_id": "last_one_bool_true_clean_stop",
        "description": "Portal lastOne=True frame forces clean stop without [DONE] sentinel",
        "raw_last_one": True,
        "expected_finish_reason": "stop",
        "expected_content": "Longcat stream complete.",
        "is_stub": False,
    }
    assert getattr(resp_bool, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (getattr(resp_bool, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case1[
        "is_stub"
    ]
    assert (
        resp_bool.choices[0].finish_reason == expected_case1["expected_finish_reason"]
    )
    assert resp_bool.choices[0].message.content == expected_case1["expected_content"]
    cases.append(expected_case1)

    # Case 2: lastOne=1 integer truthy
    chunks_int_one = [
        _make_stream_chunk(content="Integer lastOne complete."),
        SimpleNamespace(choices=[], model="upstream/model", lastOne=1),
    ]
    resp_int = _execute_stream_call(chunks_int_one)
    expected_case2 = {
        "case_id": "last_one_int_one_clean_stop",
        "description": "Relabelled upstream sending integer lastOne=1 treated as clean stop",
        "raw_last_one": 1,
        "expected_finish_reason": "stop",
        "expected_content": "Integer lastOne complete.",
        "is_stub": False,
    }
    assert getattr(resp_int, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (getattr(resp_int, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case2[
        "is_stub"
    ]
    assert resp_int.choices[0].finish_reason == expected_case2["expected_finish_reason"]
    assert resp_int.choices[0].message.content == expected_case2["expected_content"]
    cases.append(expected_case2)

    # Case 3: lastOne='true' string truthy
    chunks_str_true = [
        _make_stream_chunk(content="String lastOne complete."),
        SimpleNamespace(choices=[], model="upstream/model", lastOne="true"),
    ]
    resp_str = _execute_stream_call(chunks_str_true)
    expected_case3 = {
        "case_id": "last_one_str_true_clean_stop",
        "description": "Relabelled upstream sending string lastOne='true' treated as clean stop",
        "raw_last_one": "true",
        "expected_finish_reason": "stop",
        "expected_content": "String lastOne complete.",
        "is_stub": False,
    }
    assert getattr(resp_str, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (getattr(resp_str, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case3[
        "is_stub"
    ]
    assert resp_str.choices[0].finish_reason == expected_case3["expected_finish_reason"]
    assert resp_str.choices[0].message.content == expected_case3["expected_content"]
    cases.append(expected_case3)

    # Case 4: lastOne in model_extra dict
    chunks_extra = [
        _make_stream_chunk(content="Extra dict lastOne complete."),
        SimpleNamespace(
            choices=[], model="upstream/model", model_extra={"lastOne": True}
        ),
    ]
    resp_extra = _execute_stream_call(chunks_extra)
    expected_case4 = {
        "case_id": "last_one_in_model_extra_dict_clean_stop",
        "description": "lastOne located inside chunk.model_extra dict treated as clean stop",
        "raw_last_one": True,
        "expected_finish_reason": "stop",
        "expected_content": "Extra dict lastOne complete.",
        "is_stub": False,
    }
    assert getattr(resp_extra, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (
        getattr(resp_extra, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case4["is_stub"]
    assert (
        resp_extra.choices[0].finish_reason == expected_case4["expected_finish_reason"]
    )
    assert resp_extra.choices[0].message.content == expected_case4["expected_content"]
    cases.append(expected_case4)

    # Case 5: lastOne=False does NOT trigger clean stop; drops to stub if no usage
    chunks_false = [
        _make_stream_chunk(content="Incomplete stream."),
        SimpleNamespace(choices=[], model="upstream/model", lastOne=False),
    ]
    resp_false = _execute_stream_call(chunks_false)
    expected_case5 = {
        "case_id": "last_one_false_does_not_halt_drop_classification",
        "description": "lastOne=False leaves finish_reason None and falls through to stub classification",
        "raw_last_one": False,
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "is_stub": True,
    }
    assert getattr(resp_false, "id", None) == expected_case5["expected_response_id"]
    assert (
        resp_false.choices[0].finish_reason == expected_case5["expected_finish_reason"]
    )
    assert (
        getattr(resp_false, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case5["is_stub"]
    cases.append(expected_case5)

    return cases


# ==============================================================================
# Section 4: Zero-Output EOF (Empty Stream Guard)
# ==============================================================================
def gen_zero_output_eof_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Empty iterator raises EmptyStreamError
    raised_empty = False
    err_message = ""
    err_name = ""
    try:
        _execute_stream_call([])
    except EmptyStreamError as e:
        raised_empty = True
        err_message = str(e)
        err_name = e.__class__.__name__

    expected_case1 = {
        "case_id": "zero_chunks_raises_empty_stream_error",
        "description": "Zero chunks delivered with finish_reason None raises EmptyStreamError",
        "raw_chunks": [],
        "expected_exception": "EmptyStreamError",
        "expected_message_substring": "Provider returned an empty stream with no finish_reason",
        "raised": raised_empty,
    }
    assert raised_empty is expected_case1["raised"]
    assert err_name == expected_case1["expected_exception"]
    assert expected_case1["expected_message_substring"] in err_message
    cases.append(expected_case1)

    # Case 2: Chunks with empty content/reasoning/tools and finish_reason None raises EmptyStreamError
    raised_empty_deltas = False
    err_msg_deltas = ""
    err_deltas_name = ""
    chunks_empty_delta = [
        _make_stream_chunk(content=None, tool_calls=None, finish_reason=None)
    ]
    try:
        _execute_stream_call(chunks_empty_delta)
    except EmptyStreamError as e:
        raised_empty_deltas = True
        err_msg_deltas = str(e)
        err_deltas_name = e.__class__.__name__

    expected_case2 = {
        "case_id": "empty_deltas_no_finish_raises_empty_stream_error",
        "description": "Chunk with no content and no finish_reason raises EmptyStreamError",
        "expected_exception": "EmptyStreamError",
        "raised": raised_empty_deltas,
    }
    assert raised_empty_deltas is expected_case2["raised"]
    assert err_deltas_name == expected_case2["expected_exception"]
    assert "Provider returned an empty stream with no finish_reason" in err_msg_deltas
    cases.append(expected_case2)

    # Case 3: Empty content with explicit finish_reason="stop" is a valid completion
    chunks_stop_zero = [_make_stream_chunk(content="", finish_reason="stop")]
    resp_stop_zero = _execute_stream_call(chunks_stop_zero)
    expected_case3 = {
        "case_id": "zero_content_with_explicit_stop_completes_normally",
        "description": "Provider sent finish_reason='stop' with empty content; returns complete turn without error",
        "expected_finish_reason": "stop",
        "expected_content": "",
        "is_stub": False,
    }
    assert getattr(resp_stop_zero, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (
        getattr(resp_stop_zero, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case3["is_stub"]
    assert (
        resp_stop_zero.choices[0].finish_reason
        == expected_case3["expected_finish_reason"]
    )
    assert (resp_stop_zero.choices[0].message.content or "") == expected_case3[
        "expected_content"
    ]
    cases.append(expected_case3)

    return cases


# ==============================================================================
# Section 5: Provider-Reported Finish Reasons as Exclusions
# ==============================================================================
def gen_provider_reported_exclusions_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: finish_reason="stop"
    chunks_stop = [
        _make_stream_chunk(content="Finished normally.", finish_reason="stop")
    ]
    resp_stop = _execute_stream_call(chunks_stop)
    expected_case1 = {
        "case_id": "provider_reported_stop_exclusion",
        "description": "Provider sent explicit finish_reason='stop'; drop detector excluded",
        "raw_finish_reason": "stop",
        "expected_finish_reason": "stop",
        "is_stub": False,
    }
    assert getattr(resp_stop, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (getattr(resp_stop, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case1[
        "is_stub"
    ]
    assert (
        resp_stop.choices[0].finish_reason == expected_case1["expected_finish_reason"]
    )
    cases.append(expected_case1)

    # Case 2: finish_reason="length"
    chunks_length = [
        _make_stream_chunk(content="Cut off by token budget.", finish_reason="length")
    ]
    resp_length = _execute_stream_call(chunks_length)
    expected_case2 = {
        "case_id": "provider_reported_length_exclusion",
        "description": "Provider sent genuine finish_reason='length'; treated as real output cap, not stub",
        "raw_finish_reason": "length",
        "expected_finish_reason": "length",
        "is_stub": False,
    }
    assert getattr(resp_length, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (
        getattr(resp_length, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case2["is_stub"]
    assert (
        resp_length.choices[0].finish_reason == expected_case2["expected_finish_reason"]
    )
    cases.append(expected_case2)

    # Case 3: finish_reason="tool_calls"
    chunks_tools = [
        _make_stream_chunk(
            tool_calls=[
                _make_tool_call_delta(tc_id="c1", name="read_file", arguments="{}")
            ]
        ),
        _make_stream_chunk(finish_reason="tool_calls"),
    ]
    resp_tools = _execute_stream_call(chunks_tools)
    expected_case3 = {
        "case_id": "provider_reported_tool_calls_exclusion",
        "description": "Provider sent finish_reason='tool_calls'; treated as tool execution turn",
        "raw_finish_reason": "tool_calls",
        "expected_finish_reason": "tool_calls",
        "is_stub": False,
    }
    assert getattr(resp_tools, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (
        getattr(resp_tools, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case3["is_stub"]
    assert (
        resp_tools.choices[0].finish_reason == expected_case3["expected_finish_reason"]
    )
    cases.append(expected_case3)

    # Case 4: finish_reason="content_filter"
    chunks_cf = [
        _make_stream_chunk(content="Filtered.", finish_reason="content_filter")
    ]
    resp_cf = _execute_stream_call(chunks_cf)
    expected_case4 = {
        "case_id": "provider_reported_content_filter_exclusion",
        "description": "Provider sent finish_reason='content_filter'; treated as policy refusal",
        "raw_finish_reason": "content_filter",
        "expected_finish_reason": "content_filter",
        "is_stub": False,
    }
    assert getattr(resp_cf, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (getattr(resp_cf, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case4[
        "is_stub"
    ]
    assert resp_cf.choices[0].finish_reason == expected_case4["expected_finish_reason"]
    cases.append(expected_case4)

    # Case 5: vLLM merged finish content chunk (#94614)
    chunks_merged = [
        _make_stream_chunk(content=":"),
        _make_stream_chunk(content=" uniform"),
        _make_stream_chunk(content=".", finish_reason="stop"),
    ]
    resp_merged = _execute_stream_call(chunks_merged)
    expected_case5 = {
        "case_id": "vllm_merged_finish_content_chunk_exclusion",
        "description": "Terminal finish_reason merged onto content chunk extracted before content continue",
        "expected_finish_reason": "stop",
        "expected_content": ": uniform.",
        "is_stub": False,
    }
    assert getattr(resp_merged, "id", None) != PARTIAL_STREAM_STUB_ID
    assert (
        getattr(resp_merged, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case5["is_stub"]
    assert (
        resp_merged.choices[0].finish_reason == expected_case5["expected_finish_reason"]
    )
    assert resp_merged.choices[0].message.content == expected_case5["expected_content"]
    cases.append(expected_case5)

    return cases


# ==============================================================================
# Section 6: Transport Error Boundary (Pre vs Post Visible Deltas)
# ==============================================================================
def gen_transport_error_boundary_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Transport error before any visible deltas are delivered -> raises
    raised_pre = False
    err_pre_name = ""
    try:
        _execute_stream_call(
            [], stream_error=ConnectionResetError("Socket reset before first token")
        )
    except ConnectionResetError as e:
        raised_pre = True
        err_pre_name = e.__class__.__name__

    expected_case1 = {
        "case_id": "transport_error_pre_visible_raises_directly",
        "description": "Stream transport error before visible deltas re-raises for outer retry ladder",
        "deltas_were_sent": False,
        "raised_exception": err_pre_name,
        "expected_stub": False,
    }
    assert raised_pre is True
    assert err_pre_name == expected_case1["raised_exception"]
    assert (not raised_pre) is expected_case1["expected_stub"]
    cases.append(expected_case1)

    # Case 2: Transport error after visible deltas -> swallowed into partial stub
    chunks_post = [_make_stream_chunk(content="Delivered text before disconnect.")]
    resp_post = _execute_stream_call(
        chunks_post,
        current_streamed_text="Delivered text before disconnect.",
        stream_error=RuntimeError("Connection terminated mid-stream"),
    )
    expected_case2 = {
        "case_id": "transport_error_post_visible_swallowed_into_stub",
        "description": "Stream error after visible tokens swallowed into length stub preserving text",
        "deltas_were_sent": True,
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_content": "Delivered text before disconnect.",
        "expected_tool_calls": None,
        "is_stub": True,
    }
    assert getattr(resp_post, "id", None) == expected_case2["expected_response_id"]
    assert (
        resp_post.choices[0].finish_reason == expected_case2["expected_finish_reason"]
    )
    assert resp_post.choices[0].message.content == expected_case2["expected_content"]
    assert (
        resp_post.choices[0].message.tool_calls is expected_case2["expected_tool_calls"]
    )
    assert (getattr(resp_post, "id", None) == PARTIAL_STREAM_STUB_ID) is expected_case2[
        "is_stub"
    ]
    cases.append(expected_case2)

    # Case 3: Transport error after visible deltas with dropped tool names surfaces user warning
    chunks_tc_post = [
        _make_stream_chunk(content="Beginning computation: "),
        _make_stream_chunk(
            tool_calls=[_make_tool_call_delta(tc_id="c_err", name="execute_cmd")]
        ),
        _make_stream_chunk(
            tool_calls=[_make_tool_call_delta(arguments='{"cmd": "make')]
        ),
    ]
    resp_tc_post = _execute_stream_call(
        chunks_tc_post,
        current_streamed_text="Beginning computation: ",
        stream_error=RuntimeError("Upstream timeout mid tool-call arguments"),
    )
    content_warn = resp_tc_post.choices[0].message.content or ""
    expected_case3 = {
        "case_id": "transport_error_post_visible_with_tools_surfaces_warning",
        "description": "Transport error mid tool-call appends user-visible warning naming dropped tools",
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "warning_substring": "Stream stalled mid tool-call (execute_cmd)",
        "expected_dropped_tool_names": ["execute_cmd"],
        "is_stub": True,
    }
    assert getattr(resp_tc_post, "id", None) == expected_case3["expected_response_id"]
    assert (
        resp_tc_post.choices[0].finish_reason
        == expected_case3["expected_finish_reason"]
    )
    assert expected_case3["warning_substring"] in content_warn
    assert (
        getattr(resp_tc_post, "_dropped_tool_names", None)
        == expected_case3["expected_dropped_tool_names"]
    )
    assert (
        getattr(resp_tc_post, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case3["is_stub"]
    cases.append(expected_case3)

    # Case 4: Transport error after visible deltas with zero recovered chars leaves content empty
    chunks_empty_post = [_make_stream_chunk(content="transient delta")]
    agent_zero = _make_test_agent()
    resp_zero_post = _execute_stream_call(
        chunks_empty_post,
        current_streamed_text="",
        agent=agent_zero,
        stream_error=RuntimeError("Reset after header with no text"),
    )
    expected_case4 = {
        "case_id": "transport_error_post_visible_empty_text_stays_empty",
        "description": "Empty recovered text deliberately kept empty to enable loop interim suppression",
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_content_falsy": True,
        "is_stub": True,
    }
    assert getattr(resp_zero_post, "id", None) == expected_case4["expected_response_id"]
    assert (
        resp_zero_post.choices[0].finish_reason
        == expected_case4["expected_finish_reason"]
    )
    assert (
        not getattr(resp_zero_post.choices[0].message, "content", None)
    ) is expected_case4["expected_content_falsy"]
    assert (
        getattr(resp_zero_post, "id", None) == PARTIAL_STREAM_STUB_ID
    ) is expected_case4["is_stub"]
    cases.append(expected_case4)

    return cases


# ==============================================================================
# Section 7: Partial Text and Reasoning Preservation
# ==============================================================================
def gen_partial_preservation_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Pure text preservation
    stub1 = _build_partial_stream_stub(
        role="assistant",
        full_content="Preserved visible prose part 1.",
        full_reasoning=None,
        model_name="model-a",
        usage_obj=None,
    )
    expected_case1 = {
        "case_id": "partial_text_only_preservation",
        "description": "Stub faithfully preserves accumulated visible content with null reasoning",
        "expected_role": "assistant",
        "expected_content": "Preserved visible prose part 1.",
        "expected_reasoning_content": None,
        "expected_tool_calls": None,
        "expected_id": PARTIAL_STREAM_STUB_ID,
    }
    assert stub1.choices[0].message.content == expected_case1["expected_content"]
    assert stub1.choices[0].message.reasoning_content is None
    assert stub1.choices[0].message.tool_calls is None
    cases.append(expected_case1)

    # Case 2: Reasoning and text preservation
    stub2 = _build_partial_stream_stub(
        role="assistant",
        full_content="Visible answer fragment.",
        full_reasoning="Chain of thought reasoning before drop.",
        model_name="model-b",
        usage_obj=None,
    )
    expected_case2 = {
        "case_id": "partial_reasoning_and_text_preservation",
        "description": "Stub preserves both internal reasoning content and visible prose content",
        "expected_content": "Visible answer fragment.",
        "expected_reasoning_content": "Chain of thought reasoning before drop.",
        "expected_tool_calls": None,
    }
    assert stub2.choices[0].message.content == expected_case2["expected_content"]
    assert (
        stub2.choices[0].message.reasoning_content
        == expected_case2["expected_reasoning_content"]
    )
    cases.append(expected_case2)

    # Case 3: Reasoning only preservation (no visible text)
    stub3 = _build_partial_stream_stub(
        role="assistant",
        full_content="",
        full_reasoning="Extensive reasoning before drop.",
        model_name="model-c",
        usage_obj=None,
    )
    expected_case3 = {
        "case_id": "partial_reasoning_only_preservation",
        "description": "Stub preserves reasoning content when visible prose is empty",
        "expected_content": "",
        "expected_reasoning_content": "Extensive reasoning before drop.",
        "expected_tool_calls": None,
    }
    assert stub3.choices[0].message.content == ""
    assert (
        stub3.choices[0].message.reasoning_content
        == expected_case3["expected_reasoning_content"]
    )
    cases.append(expected_case3)

    # Case 4: Tool call suppression (tool_calls must always be None on stub)
    stub4 = _build_partial_stream_stub(
        role="assistant",
        full_content="Text before tool drop",
        full_reasoning=None,
        model_name="model-d",
        usage_obj=None,
        dropped_tool_names=["bash", "python"],
    )
    expected_case4 = {
        "case_id": "partial_tool_call_suppression",
        "description": "Stub message unconditionally sets tool_calls=None so broken calls never auto-execute",
        "expected_tool_calls": None,
        "expected_dropped_tool_names": ["bash", "python"],
    }
    assert stub4.choices[0].message.tool_calls is None
    assert stub4._dropped_tool_names == ["bash", "python"]
    cases.append(expected_case4)

    return cases


# ==============================================================================
# Section 8: Continuation Prompt Selection
# ==============================================================================
def gen_continuation_prompt_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Partial stub without dropped tools selects network prompt
    p1 = _get_continuation_prompt(is_partial_stub=True, dropped_tools=None)
    expected_case1 = {
        "case_id": "prompt_partial_stub_network_error",
        "description": "Partial stub with no dropped tools selects network error prompt",
        "is_partial_stub": True,
        "dropped_tools": None,
        "expected_prompt": _LENGTH_CONTINUATION_NETWORK_STUB,
        "must_contain": "cut off by a network error mid-stream",
        "must_not_contain": "output length limit",
    }
    assert p1 == _LENGTH_CONTINUATION_NETWORK_STUB
    assert "cut off by a network error mid-stream" in p1
    assert "output length limit" not in p1
    cases.append(expected_case1)

    # Case 2: Partial stub with empty tool list selects network prompt
    p2 = _get_continuation_prompt(is_partial_stub=True, dropped_tools=[])
    expected_case2 = {
        "case_id": "prompt_partial_stub_empty_tools_network_error",
        "description": "Partial stub with empty dropped_tools list selects network error prompt",
        "is_partial_stub": True,
        "dropped_tools": [],
        "expected_prompt": _LENGTH_CONTINUATION_NETWORK_STUB,
    }
    assert p2 == _LENGTH_CONTINUATION_NETWORK_STUB
    cases.append(expected_case2)

    # Case 3: Partial stub with dropped tools selects chunking guidance prompt
    p3 = _get_continuation_prompt(is_partial_stub=True, dropped_tools=["write_file"])
    expected_case3 = {
        "case_id": "prompt_partial_stub_single_dropped_tool",
        "description": "Partial stub with dropped tool names requests chunking under ~8K tokens",
        "is_partial_stub": True,
        "dropped_tools": ["write_file"],
        "expected_prompt": p3,
        "starts_with": _LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX,
        "must_contain": ["(write_file)", "under ~8K tokens"],
    }
    assert p3.startswith(_LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX)
    assert "(write_file)" in p3
    assert "under ~8K tokens" in p3
    cases.append(expected_case3)

    # Case 4: Partial stub with >3 dropped tools is capped at 3 names
    p4 = _get_continuation_prompt(
        is_partial_stub=True,
        dropped_tools=["tool_a", "tool_b", "tool_c", "tool_d", "tool_e"],
    )
    expected_case4 = {
        "case_id": "prompt_partial_stub_tool_cap_at_three",
        "description": "More than 3 dropped tools caps interpolated list at exactly first 3 names",
        "is_partial_stub": True,
        "dropped_tools": ["tool_a", "tool_b", "tool_c", "tool_d", "tool_e"],
        "expected_prompt": p4,
        "must_contain": "(tool_a, tool_b, tool_c)",
        "must_not_contain": "tool_d",
    }
    assert "(tool_a, tool_b, tool_c)" in p4
    assert "tool_d" not in p4
    cases.append(expected_case4)

    # Case 5: Non-stub response selects standard output limit prompt
    p5 = _get_continuation_prompt(is_partial_stub=False, dropped_tools=None)
    expected_case5 = {
        "case_id": "prompt_non_stub_output_limit",
        "description": "Genuine output-length truncation selects output limit prompt",
        "is_partial_stub": False,
        "dropped_tools": None,
        "expected_prompt": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
        "must_contain": "truncated by the output length limit",
        "must_not_contain": "network error",
    }
    assert p5 == _LENGTH_CONTINUATION_OUTPUT_LIMIT
    assert "truncated by the output length limit" in p5
    assert "network error" not in p5
    cases.append(expected_case5)

    # Case 6: Non-stub response ignores dropped_tools list
    p6 = _get_continuation_prompt(is_partial_stub=False, dropped_tools=["write_file"])
    expected_case6 = {
        "case_id": "prompt_non_stub_ignores_dropped_tools",
        "description": "Non-stub response ignores dropped_tools and uses output limit prompt",
        "is_partial_stub": False,
        "dropped_tools": ["write_file"],
        "expected_prompt": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
    }
    assert p6 == _LENGTH_CONTINUATION_OUTPUT_LIMIT
    cases.append(expected_case6)

    return cases


# ==============================================================================
# Section 9: Empty-Stub Interim Suppression
# ==============================================================================
def gen_empty_stub_interim_suppression_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Empty partial stub suppresses assistant message
    agent1 = _make_loop_agent()
    empty_stub1 = SimpleNamespace(
        id=PARTIAL_STREAM_STUB_ID,
        model="test/model",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(content="", tool_calls=None),
                finish_reason=FINISH_REASON_LENGTH,
            )
        ],
        usage=None,
        _dropped_tool_names=["write_file"],
    )
    recovery1 = SimpleNamespace(
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(content="Done.", tool_calls=None),
                finish_reason="stop",
            )
        ],
        model="test/model",
        usage=None,
    )
    snapshots1: List[List[Dict[str, Any]]] = []

    def _create_call1(*a, **kw):
        snapshots1.append(copy.deepcopy(agent1._session_messages))
        if len(snapshots1) == 1:
            return empty_stub1
        return recovery1

    agent1.client.chat.completions.create.side_effect = _create_call1
    with (
        patch.object(agent1, "_persist_session"),
        patch.object(agent1, "_save_trajectory"),
        patch.object(agent1, "_cleanup_task_resources"),
    ):
        res1 = agent1.run_conversation("test user turn")

    msgs1_after_stub = snapshots1[1]
    asst_msgs1 = [m for m in msgs1_after_stub if m.get("role") == "assistant"]
    nudge_msgs1 = [m for m in msgs1_after_stub if m.get("_length_continuation_nudge")]

    expected_case1 = {
        "case_id": "empty_partial_stub_suppresses_assistant_message",
        "description": "Empty stub content avoids appending empty assistant turn to protect Moonshot/Kimi",
        "response_id": PARTIAL_STREAM_STUB_ID,
        "content": "",
        "assistant_messages_count": len(asst_msgs1),
        "total_messages_count": len(msgs1_after_stub),
        "nudge_appended": len(nudge_msgs1) > 0,
        "ephemeral_reasoning_off": getattr(agent1, "_ephemeral_reasoning_off", False),
    }
    assert expected_case1["assistant_messages_count"] == 0
    assert expected_case1["total_messages_count"] == 2
    assert expected_case1["nudge_appended"] is True
    assert expected_case1["ephemeral_reasoning_off"] is False
    assert getattr(empty_stub1, "id", None) == expected_case1["response_id"]
    assert empty_stub1.choices[0].message.content == expected_case1["content"]
    assert len(asst_msgs1) == expected_case1["assistant_messages_count"]
    assert len(msgs1_after_stub) == expected_case1["total_messages_count"]
    assert (len(nudge_msgs1) > 0) is expected_case1["nudge_appended"]
    assert (
        getattr(agent1, "_ephemeral_reasoning_off", False)
        is expected_case1["ephemeral_reasoning_off"]
    )
    cases.append(expected_case1)

    # Case 2: Non-empty partial stub appends assistant fragment
    agent2 = _make_loop_agent()
    non_empty_stub2 = SimpleNamespace(
        id=PARTIAL_STREAM_STUB_ID,
        model="test/model",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(
                    content="First section delivered.", tool_calls=None
                ),
                finish_reason=FINISH_REASON_LENGTH,
            )
        ],
        usage=None,
    )
    snapshots2: List[List[Dict[str, Any]]] = []

    def _create_call2(*a, **kw):
        snapshots2.append(copy.deepcopy(agent2._session_messages))
        if len(snapshots2) == 1:
            return non_empty_stub2
        return recovery1

    agent2.client.chat.completions.create.side_effect = _create_call2
    with (
        patch.object(agent2, "_persist_session"),
        patch.object(agent2, "_save_trajectory"),
        patch.object(agent2, "_cleanup_task_resources"),
    ):
        res2 = agent2.run_conversation("test user turn")

    msgs2_after_stub = snapshots2[1]
    asst_msgs2 = [m for m in msgs2_after_stub if m.get("role") == "assistant"]
    frag_msgs2 = [m for m in msgs2_after_stub if m.get("_length_continuation_fragment")]

    expected_case2 = {
        "case_id": "non_empty_partial_stub_appends_fragment",
        "description": "Non-empty stub content appends assistant fragment tagged _length_continuation_fragment",
        "response_id": PARTIAL_STREAM_STUB_ID,
        "content": "First section delivered.",
        "assistant_messages_count": len(asst_msgs2),
        "truncated_parts": [m.get("content", "") for m in frag_msgs2],
        "fragment_tagged": len(frag_msgs2) > 0,
    }
    assert expected_case2["assistant_messages_count"] == 1
    assert expected_case2["fragment_tagged"] is True
    assert expected_case2["truncated_parts"] == ["First section delivered."]
    assert getattr(non_empty_stub2, "id", None) == expected_case2["response_id"]
    assert non_empty_stub2.choices[0].message.content == expected_case2["content"]
    assert len(asst_msgs2) == expected_case2["assistant_messages_count"]
    assert (len(frag_msgs2) > 0) is expected_case2["fragment_tagged"]
    assert [m.get("content", "") for m in frag_msgs2] == expected_case2[
        "truncated_parts"
    ]
    cases.append(expected_case2)

    # Case 3: Non-stub empty content (thinking-only output cap) sets ephemeral reasoning off
    agent3 = _make_loop_agent()
    thinking_cap_response3 = SimpleNamespace(
        id="chatcmpl-real-uuid",
        model="test/model",
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(content="", tool_calls=None),
                finish_reason=FINISH_REASON_LENGTH,
            )
        ],
        usage=None,
    )
    snapshots3: List[List[Dict[str, Any]]] = []
    consumed_reasoning_off_3: List[tuple[bool, bool]] = []
    orig_consume = _consume_ephemeral_reasoning_off

    def _track_consume_3(a):
        flag_before = getattr(a, "_ephemeral_reasoning_off", False)
        consumed = orig_consume(a)
        consumed_reasoning_off_3.append((flag_before, consumed))
        return consumed

    def _create_call3(*a, **kw):
        snapshots3.append(copy.deepcopy(agent3._session_messages))
        if len(snapshots3) == 1:
            return thinking_cap_response3
        return recovery1

    agent3.client.chat.completions.create.side_effect = _create_call3
    with (
        patch.object(agent3, "_persist_session"),
        patch.object(agent3, "_save_trajectory"),
        patch.object(agent3, "_cleanup_task_resources"),
        patch(
            "agent.chat_completion_helpers._consume_ephemeral_reasoning_off",
            side_effect=_track_consume_3,
        ),
    ):
        res3 = agent3.run_conversation("test user turn")

    msgs3_after_stub = snapshots3[1]
    asst_msgs3 = [m for m in msgs3_after_stub if m.get("role") == "assistant"]
    reasoning_off_consumed = (
        len(consumed_reasoning_off_3) >= 2
        and consumed_reasoning_off_3[1][0] is True
        and consumed_reasoning_off_3[1][1] is True
    )

    expected_case3 = {
        "case_id": "non_stub_thinking_only_sets_reasoning_off",
        "description": "Non-stub empty content (reasoning exhausted cap) sets ephemeral_reasoning_off=True",
        "response_id": "chatcmpl-real-uuid",
        "content": "",
        "assistant_messages_count": len(asst_msgs3),
        "ephemeral_reasoning_off": reasoning_off_consumed,
    }
    assert expected_case3["assistant_messages_count"] == 0
    assert expected_case3["ephemeral_reasoning_off"] is True
    assert getattr(thinking_cap_response3, "id", None) == expected_case3["response_id"]
    assert (
        thinking_cap_response3.choices[0].message.content == expected_case3["content"]
    )
    assert len(asst_msgs3) == expected_case3["assistant_messages_count"]
    assert reasoning_off_consumed is expected_case3["ephemeral_reasoning_off"]
    cases.append(expected_case3)

    return cases


# ==============================================================================
# Section 10: Retry and Ceiling Metadata
# ==============================================================================
def gen_retry_and_ceiling_metadata_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: Ceiling exit with stitched partial text
    agent_ceil1 = _make_loop_agent()
    parts = ["Quantum mechanics ", "is the ", "fundamental branch ", "of physics"]
    stubs_ceil1 = [
        SimpleNamespace(
            id=PARTIAL_STREAM_STUB_ID,
            choices=[
                SimpleNamespace(
                    index=0,
                    message=SimpleNamespace(content=p, tool_calls=None),
                    finish_reason=FINISH_REASON_LENGTH,
                )
            ],
            model="test/model",
            usage=None,
        )
        for p in parts
    ]
    agent_ceil1.client.chat.completions.create.side_effect = stubs_ceil1

    with (
        patch.object(agent_ceil1, "_persist_session"),
        patch.object(agent_ceil1, "_save_trajectory"),
        patch.object(agent_ceil1, "_cleanup_task_resources"),
    ):
        res_ceil1 = agent_ceil1.run_conversation("What is quantum mechanics?")

    final_resp_clean = _clean_str(res_ceil1.get("final_response", ""))
    actual_roles = [m["role"] for m in res_ceil1.get("messages", [])]
    call_count1 = agent_ceil1.client.chat.completions.create.call_count

    expected_case1 = {
        "case_id": "ceiling_exit_stitches_partial_and_strips_scaffolding",
        "description": "After 4 continuation attempts, intermediate fragments and nudges are replaced by stitched partial",
        "retries_allowed": call_count1,
        "expected_final_response": final_resp_clean,
        "expected_completed": res_ceil1.get("completed"),
        "expected_partial": res_ceil1.get("partial"),
        "expected_error": res_ceil1.get("error"),
        "remaining_message_roles": actual_roles,
    }
    assert expected_case1["retries_allowed"] == 4
    assert (
        expected_case1["expected_final_response"]
        == "Quantum mechanics is the fundamental branch of physics"
    )
    assert expected_case1["expected_completed"] is False
    assert expected_case1["expected_partial"] is True
    assert (
        expected_case1["expected_error"]
        == "Response remained truncated after 4 continuation attempts"
    )
    assert expected_case1["remaining_message_roles"] == ["user", "assistant"]
    assert res_ceil1["final_response"] == expected_case1["expected_final_response"]
    assert res_ceil1["completed"] is expected_case1["expected_completed"]
    assert res_ceil1["partial"] is expected_case1["expected_partial"]
    assert res_ceil1["error"] == expected_case1["expected_error"]
    assert actual_roles == expected_case1["remaining_message_roles"]
    assert call_count1 == expected_case1["retries_allowed"]
    assert not any(
        m.get("_length_continuation_fragment") for m in res_ceil1["messages"]
    )
    assert not any(m.get("_length_continuation_nudge") for m in res_ceil1["messages"])
    cases.append(expected_case1)

    # Case 2: Ceiling exit with zero visible text returns actionable notice
    agent_ceil2 = _make_loop_agent()
    stubs_ceil2 = [
        SimpleNamespace(
            id=PARTIAL_STREAM_STUB_ID,
            choices=[
                SimpleNamespace(
                    index=0,
                    message=SimpleNamespace(content="", tool_calls=None),
                    finish_reason=FINISH_REASON_LENGTH,
                )
            ],
            model="test/model",
            usage=None,
        )
        for _ in range(4)
    ]
    agent_ceil2.client.chat.completions.create.side_effect = stubs_ceil2

    with (
        patch.object(agent_ceil2, "_persist_session"),
        patch.object(agent_ceil2, "_save_trajectory"),
        patch.object(agent_ceil2, "_cleanup_task_resources"),
    ):
        res_ceil2 = agent_ceil2.run_conversation("Complex logic puzzle")

    has_assistant_persisted = any(
        m.get("role") == "assistant" for m in res_ceil2.get("messages", [])
    )
    final_resp_ceil2 = _clean_str(res_ceil2.get("final_response", ""))

    expected_case2 = {
        "assistant_persisted": has_assistant_persisted,
        "case_id": "ceiling_exit_zero_visible_text_guidance",
        "description": "Ceiling exit when all fragments were empty returns actionable failure without assistant message",
        "expected_completed": res_ceil2.get("completed"),
        "expected_final_response": final_resp_ceil2,
        "expected_partial": res_ceil2.get("partial"),
    }
    assert expected_case2["assistant_persisted"] is False
    assert expected_case2["expected_completed"] is False
    assert expected_case2["expected_partial"] is True
    assert (
        "No visible answer was produced." in expected_case2["expected_final_response"]
    )
    assert res_ceil2["completed"] is expected_case2["expected_completed"]
    assert res_ceil2["partial"] is expected_case2["expected_partial"]
    assert (
        _clean_str(res_ceil2["final_response"])
        == expected_case2["expected_final_response"]
    )
    assert has_assistant_persisted is expected_case2["assistant_persisted"]
    cases.append(expected_case2)

    return cases


# ==============================================================================
# Section 11: Content-Filter Tagging and Escalation
# ==============================================================================
def gen_content_filter_tagging_cases() -> List[Dict[str, Any]]:
    cases = []

    # Case 1: MiniMax output safety filter classified as content policy blocked
    err_minimax = RuntimeError("upstream output new_sensitive (1027)")
    cls1 = classify_api_error(err_minimax, provider="minimax", model="MiniMax-M2.7")
    expected_case1 = {
        "case_id": "error_classification_minimax_new_sensitive",
        "error_string": str(err_minimax),
        "expected_reason": FailoverReason.content_policy_blocked.value,
        "expected_should_fallback": True,
        "expected_retryable": False,
    }
    assert cls1.reason.value == expected_case1["expected_reason"]
    assert cls1.should_fallback is expected_case1["expected_should_fallback"]
    assert cls1.retryable is expected_case1["expected_retryable"]
    cases.append(expected_case1)

    # Case 2: Azure content filter pattern classified as content policy blocked
    err_azure = RuntimeError("Request was rejected by content_filter")
    cls2 = classify_api_error(err_azure, provider="azure", model="gpt-4o")
    expected_case2 = {
        "case_id": "error_classification_azure_content_filter",
        "error_string": str(err_azure),
        "expected_reason": FailoverReason.content_policy_blocked.value,
        "expected_should_fallback": True,
        "expected_retryable": False,
    }
    assert cls2.reason.value == expected_case2["expected_reason"]
    assert cls2.should_fallback is expected_case2["expected_should_fallback"]
    assert cls2.retryable is expected_case2["expected_retryable"]
    cases.append(expected_case2)

    # Case 3: OpenAI usage policy pattern classified as content policy blocked
    err_oai = RuntimeError("The generated response violates our usage policies")
    cls3 = classify_api_error(err_oai, provider="openai", model="gpt-4o")
    expected_case3 = {
        "case_id": "error_classification_openai_usage_policy",
        "error_string": str(err_oai),
        "expected_reason": FailoverReason.content_policy_blocked.value,
        "expected_should_fallback": True,
    }
    assert cls3.reason.value == expected_case3["expected_reason"]
    assert cls3.should_fallback is expected_case3["expected_should_fallback"]
    cases.append(expected_case3)

    # Case 4: Non-policy transport error is not classified as content policy blocked
    err_net = ConnectionResetError("Connection reset by peer")
    cls4 = classify_api_error(err_net, provider="openai", model="gpt-4o")
    expected_case4 = {
        "case_id": "error_classification_socket_reset_is_not_content_filter",
        "error_string": str(err_net),
        "is_content_policy_blocked": False,
    }
    assert (cls4.reason == FailoverReason.content_policy_blocked) is expected_case4[
        "is_content_policy_blocked"
    ]
    cases.append(expected_case4)

    # Case 5: End-to-end stream call stamps _content_filter_terminated on stub
    chunks_cf = [_make_stream_chunk(content="Partial text before safety halt.")]
    agent_cf = _make_test_agent(model="MiniMax-M2.7", provider="minimax")
    resp_cf = _execute_stream_call(
        chunks_cf,
        current_streamed_text="Partial text before safety halt.",
        agent=agent_cf,
        stream_error=RuntimeError("output new_sensitive (1027)"),
    )
    expected_case5 = {
        "case_id": "stream_call_stamps_content_filter_terminated_on_stub",
        "description": "Stream transport error matching content filter stamps _content_filter_terminated=True on stub",
        "expected_response_id": PARTIAL_STREAM_STUB_ID,
        "expected_finish_reason": FINISH_REASON_LENGTH,
        "expected_content_filter_terminated": True,
    }
    assert getattr(resp_cf, "id", None) == expected_case5["expected_response_id"]
    assert resp_cf.choices[0].finish_reason == expected_case5["expected_finish_reason"]
    assert (
        getattr(resp_cf, "_content_filter_terminated", False)
        is expected_case5["expected_content_filter_terminated"]
    )
    cases.append(expected_case5)

    # Case 6: Conversation loop activates fallback first pass on tagged stub
    agent_loop = _make_loop_agent(model="MiniMax-M2.7", provider="minimax")
    agent_loop._fallback_chain = [
        {"provider": "openrouter", "model": "anthropic/claude-sonnet-4.7"}
    ]
    agent_loop._fallback_index = 0
    fb_calls = []

    def _fake_activate(reason=None):
        fb_calls.append(reason)
        agent_loop._fallback_index = len(agent_loop._fallback_chain)
        return True

    recovery_fb = SimpleNamespace(
        choices=[
            SimpleNamespace(
                index=0,
                message=SimpleNamespace(content="Done on fallback.", tool_calls=None),
                finish_reason="stop",
            )
        ],
        model="anthropic/claude-sonnet-4.7",
        usage=None,
    )

    agent_loop.client.chat.completions.create.side_effect = [resp_cf, recovery_fb]

    with (
        patch.object(agent_loop, "_persist_session"),
        patch.object(agent_loop, "_save_trajectory"),
        patch.object(agent_loop, "_cleanup_task_resources"),
        patch.object(agent_loop, "_try_activate_fallback", side_effect=_fake_activate),
    ):
        res_fb = agent_loop.run_conversation("write me a long file")

    fallback_activated = len(fb_calls) == 1
    # On the first pass, fallback was triggered without burning retries
    # (1 call on primary, 1 call on fallback; call_count == 2)
    length_retries_burned = (
        0
        if fallback_activated
        and agent_loop.client.chat.completions.create.call_count == 2
        else -1
    )
    messages_rolled_back = (
        [m["role"] for m in res_fb["messages"]] == ["user", "assistant"]
        and not any(m.get("_length_continuation_fragment") for m in res_fb["messages"])
        and not any(m.get("_length_continuation_nudge") for m in res_fb["messages"])
    )

    expected_case6 = {
        "case_id": "loop_content_filter_stub_activates_fallback_first_pass",
        "description": "Loop detecting _content_filter_terminated immediately falls back without burning retries",
        "fallback_activated": fallback_activated,
        "length_retries_burned": length_retries_burned,
        "messages_rolled_back": messages_rolled_back,
    }
    assert expected_case6["fallback_activated"] is True
    assert expected_case6["length_retries_burned"] == 0
    assert expected_case6["messages_rolled_back"] is True
    assert fallback_activated is expected_case6["fallback_activated"]
    assert length_retries_burned == expected_case6["length_retries_burned"]
    assert messages_rolled_back is expected_case6["messages_rolled_back"]
    cases.append(expected_case6)

    return cases


# ==============================================================================
# Master Generation and Determinism
# ==============================================================================
def build_dropped_stream_goldens() -> Dict[str, Any]:
    goldens = {
        "clean_eof_stream_recovery": gen_clean_eof_stream_recovery_cases(),
        "usage_object_presence_semantics": gen_usage_object_presence_cases(),
        "final_last_one_signals": gen_final_last_one_signal_cases(),
        "zero_output_eof": gen_zero_output_eof_cases(),
        "provider_reported_finish_reasons_exclusions": gen_provider_reported_exclusions_cases(),
        "transport_error_boundary": gen_transport_error_boundary_cases(),
        "partial_content_preservation": gen_partial_preservation_cases(),
        "continuation_prompt_selection": gen_continuation_prompt_cases(),
        "empty_stub_interim_suppression": gen_empty_stub_interim_suppression_cases(),
        "retry_and_ceiling_metadata": gen_retry_and_ceiling_metadata_cases(),
        "content_filter_tagging_and_escalation": gen_content_filter_tagging_cases(),
    }
    # Ensure zero em dashes
    clean = _clean_str(goldens)
    return clean


def main() -> None:
    check_mode = "--check" in sys.argv
    data = build_dropped_stream_goldens()
    dumped = json.dumps(data, indent=2, sort_keys=True) + "\n"

    # Strict em dash absence guard
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
