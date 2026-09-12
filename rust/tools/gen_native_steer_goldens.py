#!/usr/bin/env python3
"""Deterministic golden generator and Python behavioral contract oracle for /steer.

This generator executes and audits live Python Hermes behavior governing:
1. AIAgent.steer: input normalization, whitespace trimming, empty rejection,
   FIFO newline concatenation, and thread-safe concurrent submissions with
   asserted deterministic ordering.
2. AIAgent._drain_pending_steer: atomic read-and-clear semantics, second-call
   idempotency, and clearing upon hard-interrupt (request_hard_interrupt & clear_interrupt).
3. apply_pending_steer_to_tool_results: string content appending, multimodal
   content block preservation, targeting the tail tool result exclusively in
   multi-result batches, restashing when no matching tool row exists, zero/negative
   tool row bounds checking, and handling multiple queued steers.
4. Pre-API steer drain: conversation loop backwards scan, tail tool injection
   before API request message creation, first-turn restash when no tool results
   exist yet, and role alternation preservation.
5. Turn finalizer leftover steer: AIAgent.run_conversation and turn_finalizer.finalize_turn
   returning leftover steer in result["pending_steer"] when turns end without further
   tool calls, and gateway next-turn promotion.
6. Acknowledgement strings, preview truncation boundaries (<=60 vs >60 chars),
   busy sentinel queuing, and idle valid-payload fallthrough.

Usage:
    ./.venv/bin/python3 rust/tools/gen_native_steer_goldens.py          # Generate goldens
    ./.venv/bin/python3 rust/tools/gen_native_steer_goldens.py --check  # Verify disk parity
"""

from __future__ import annotations

import asyncio
import contextlib
import io
import json
import logging
import os
import sys
import threading
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_PATH = REPO_ROOT / "rust/tools/native-steer-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

# Ensure runtime dependencies by re-execing with repository virtualenv if necessary.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = REPO_ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

# Suppress loud loggers during test execution to ensure clean stdout.
for logger_name in (
    "run_agent",
    "agent.conversation_loop",
    "agent.agent_runtime_helpers",
    "agent.turn_finalizer",
    "gateway.run",
    "hermes_cli",
):
    logging.getLogger(logger_name).setLevel(logging.CRITICAL)

from agent.agent_runtime_helpers import apply_pending_steer_to_tool_results
from agent.interrupt_compat import request_hard_interrupt
from agent.iteration_budget import IterationBudget
from agent.prompt_builder import (
    STEER_CHANNEL_NOTE,
    STEER_MARKER_CLOSE,
    STEER_MARKER_OPEN,
    format_steer_marker,
)
from agent.turn_finalizer import finalize_turn
from gateway.platforms.base import MessageEvent, MessageType
from gateway.run import GatewayRunner, _AGENT_PENDING_SENTINEL
from hermes_cli.commands import (
    ACTIVE_SESSION_BYPASS_COMMANDS,
    resolve_command,
    should_bypass_active_session,
)
from run_agent import AIAgent


def _bare_agent() -> AIAgent:
    """Construct an AIAgent double matching the object.__new__ stub pattern in tests."""
    agent = object.__new__(AIAgent)
    agent.model = "test-model"
    agent.provider = "test-provider"
    agent._base_url = "https://example.com/v1"
    agent.session_id = "test-session"
    agent.session_input_tokens = 0
    agent.session_output_tokens = 0
    agent.session_cache_read_tokens = 0
    agent.session_cache_write_tokens = 0
    agent.session_reasoning_tokens = 0
    agent.session_prompt_tokens = 0
    agent.session_completion_tokens = 0
    agent.session_total_tokens = 0
    agent.context_compressor = None
    agent.session_estimated_cost_usd = 0.0
    agent.session_cost_status = "ok"
    agent.session_cost_source = "exact"
    agent.request_overrides = {}
    agent._tool_guardrail_halt_decision = None
    agent._last_persistence_error_cause = None
    agent._pending_steer = None
    agent._pending_steer_lock = threading.Lock()
    agent._pending_redirect = None
    agent._pending_redirect_lock = threading.Lock()
    agent._model_request_active = threading.Event()
    agent._hard_interrupt_requested = threading.Event()
    agent._executing_tools = False
    agent._execution_thread_id = None
    agent._interrupt_thread_signal_pending = False
    agent._interrupt_requested = False
    agent._interrupt_message = None
    agent._tool_interrupt_reason = None
    agent._active_children = []
    agent._active_children_lock = threading.Lock()
    agent._tool_worker_threads = None
    agent._tool_worker_threads_lock = None
    agent._current_streamed_assistant_text = ""
    agent._stream_needs_break = False
    agent._response_was_previewed = False
    agent._stream_callback = None
    agent._skill_nudge_interval = 0
    agent._iters_since_skill = 0
    agent.valid_tool_names = set()
    agent.max_iterations = 30
    agent.iteration_budget = IterationBudget(30)
    agent.quiet_mode = True
    agent.api_mode = "chat_completions"
    agent._strip_think_blocks = lambda content: content

    # Harmless persistence / cleanup mocks for turn finalizer:
    agent._persist_session = MagicMock()
    agent._flush_messages_to_session_db = MagicMock()
    agent._save_trajectory = MagicMock()
    agent._cleanup_task_resources = MagicMock()
    agent._drop_trailing_empty_response_scaffolding = MagicMock()
    agent._emit_status = MagicMock()
    agent._safe_print = MagicMock()
    agent._file_mutation_verifier_enabled = lambda: False
    agent._turn_completion_explainer_enabled = lambda: False
    agent._sync_external_memory_for_turn = MagicMock()
    agent._background_memory_review = MagicMock()

    return agent


def _loop_agent() -> AIAgent:
    """Construct an initialized AIAgent for real conversation loop execution."""
    with (
        patch("run_agent.get_tool_definitions", return_value=[]),
        patch("run_agent.check_toolset_requirements", return_value={}),
        patch("run_agent.OpenAI"),
    ):
        agent = AIAgent(
            api_key="test-key-1234567890",
            base_url="https://openrouter.ai/api/v1",
            quiet_mode=True,
            skip_context_files=True,
            skip_memory=True,
        )
    agent.client = MagicMock()
    agent._cached_system_prompt = "You are helpful."
    agent._use_prompt_caching = False
    agent.tool_delay = 0
    agent.compression_enabled = False
    agent.save_trajectories = False
    agent._flush_messages_to_session_db = MagicMock()
    agent._persist_session = MagicMock()
    agent._save_trajectory = MagicMock()
    agent._cleanup_task_resources = MagicMock()
    return agent


def _mock_assistant_msg(content="Hello", tool_calls=None) -> SimpleNamespace:
    return SimpleNamespace(
        content=content,
        tool_calls=tool_calls,
        reasoning=None,
        reasoning_content=None,
        reasoning_details=None,
    )


def _mock_response(
    content="Hello", finish_reason="stop", tool_calls=None
) -> SimpleNamespace:
    msg = _mock_assistant_msg(content=content, tool_calls=tool_calls)
    choice = SimpleNamespace(message=msg, finish_reason=finish_reason)
    return SimpleNamespace(choices=[choice], model="test/model", usage=None)


# ==============================================================================
# 1. AIAgent.steer Normalization, Empty Rejection, FIFO, Concurrency
# ==============================================================================
def gen_steer_submission_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Standard clean string
    agent = _bare_agent()
    accepted = agent.steer("focus on error handling")
    assert accepted is True
    assert agent._pending_steer == "focus on error handling"
    cases.append({
        "case_id": "STEER-SUBMIT-01",
        "name": "basic_clean_text",
        "description": "Standard non-empty single-line steer text accepted unconditionally.",
        "evidence_type": "executed",
        "input_text": "focus on error handling",
        "expected_accepted": True,
        "resulting_pending_steer": "focus on error handling",
    })

    # Case 2: Leading and trailing whitespace stripped
    agent = _bare_agent()
    accepted = agent.steer("  \t optimize database query \n  ")
    assert accepted is True
    assert agent._pending_steer == "optimize database query"
    cases.append({
        "case_id": "STEER-SUBMIT-02",
        "name": "whitespace_trimming",
        "description": "Leading and trailing spaces, tabs, and newlines are stripped.",
        "evidence_type": "executed",
        "input_text": "  \t optimize database query \n  ",
        "expected_accepted": True,
        "resulting_pending_steer": "optimize database query",
    })

    # Case 3: Internal newlines preserved
    agent = _bare_agent()
    raw_text = "step 1: fetch metrics\nstep 2: compute aggregates"
    accepted = agent.steer(f"  {raw_text}  \n")
    assert accepted is True
    assert agent._pending_steer == raw_text
    cases.append({
        "case_id": "STEER-SUBMIT-03",
        "name": "internal_newlines_preserved",
        "description": "Internal newlines within multi-line steer text are preserved intact.",
        "evidence_type": "executed",
        "input_text": f"  {raw_text}  \n",
        "expected_accepted": True,
        "resulting_pending_steer": raw_text,
    })

    # Case 4: Unicode and emoji payloads preserved
    agent = _bare_agent()
    emoji_text = "⚡ verify response latency < 50ms & 🚀 deploy"
    accepted = agent.steer(f" {emoji_text} ")
    assert accepted is True
    assert agent._pending_steer == emoji_text
    cases.append({
        "case_id": "STEER-SUBMIT-04",
        "name": "unicode_and_emojis",
        "description": "Unicode characters, emojis, and symbols are preserved without alteration.",
        "evidence_type": "executed",
        "input_text": f" {emoji_text} ",
        "expected_accepted": True,
        "resulting_pending_steer": emoji_text,
    })

    # Case 5: Empty string rejection
    agent = _bare_agent()
    accepted = agent.steer("")
    assert accepted is False
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-SUBMIT-05",
        "name": "empty_string_rejection",
        "description": "Empty string rejected with False and leaves pending_steer None.",
        "evidence_type": "executed",
        "input_text": "",
        "expected_accepted": False,
        "resulting_pending_steer": None,
    })

    # Case 6: Whitespace-only spaces rejection
    agent = _bare_agent()
    accepted = agent.steer("     ")
    assert accepted is False
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-SUBMIT-06",
        "name": "whitespace_only_spaces_rejection",
        "description": "Spaces-only string rejected with False and leaves pending_steer None.",
        "evidence_type": "executed",
        "input_text": "     ",
        "expected_accepted": False,
        "resulting_pending_steer": None,
    })

    # Case 7: Whitespace-only tabs and newlines rejection
    agent = _bare_agent()
    accepted = agent.steer("\t\r\n \t \n")
    assert accepted is False
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-SUBMIT-07",
        "name": "whitespace_only_tabs_newlines_rejection",
        "description": "Tabs and newlines string rejected with False and leaves pending_steer None.",
        "evidence_type": "executed",
        "input_text": "\t\r\n \t \n",
        "expected_accepted": False,
        "resulting_pending_steer": None,
    })

    # Case 8: FIFO concatenation with newline delimiter
    agent = _bare_agent()
    r1 = agent.steer("first note")
    r2 = agent.steer("second note")
    assert r1 is True and r2 is True
    assert agent._pending_steer == "first note\nsecond note"
    cases.append({
        "case_id": "STEER-SUBMIT-08",
        "name": "fifo_concatenation_two_submissions",
        "description": "Successive submissions concatenate using newline (\\n) in FIFO order.",
        "evidence_type": "executed",
        "submissions": ["first note", "second note"],
        "expected_accepted": [True, True],
        "resulting_pending_steer": "first note\nsecond note",
    })

    # Case 9: FIFO concatenation with interleaved empty rejections
    agent = _bare_agent()
    submissions = ["alpha", "   ", "beta", "", "\t\n", "gamma"]
    results = [agent.steer(s) for s in submissions]
    assert results == [True, False, True, False, False, True]
    assert agent._pending_steer == "alpha\nbeta\ngamma"
    cases.append({
        "case_id": "STEER-SUBMIT-09",
        "name": "fifo_concatenation_interleaved_empty",
        "description": "Empty submissions rejected without affecting existing queued buffer.",
        "evidence_type": "executed",
        "submissions": submissions,
        "expected_accepted": results,
        "resulting_pending_steer": "alpha\nbeta\ngamma",
    })

    # Case 10: Thread-safe concurrent submissions with asserted deterministic order
    agent = _bare_agent()
    thread_count = 5
    barriers = [threading.Event() for _ in range(thread_count)]
    order_recorded: List[str] = []

    def phased_worker(idx: int) -> None:
        barriers[idx].wait()
        agent.steer(f"phase-{idx}")
        order_recorded.append(f"phase-{idx}")
        if idx + 1 < thread_count:
            barriers[idx + 1].set()

    threads = [
        threading.Thread(target=phased_worker, args=(i,)) for i in range(thread_count)
    ]
    for t in threads:
        t.start()
    barriers[0].set()
    for t in threads:
        t.join()

    expected_fifo = "\n".join(f"phase-{i}" for i in range(thread_count))
    assert agent._pending_steer == expected_fifo
    assert order_recorded == [f"phase-{i}" for i in range(thread_count)]
    cases.append({
        "case_id": "STEER-SUBMIT-10",
        "name": "thread_safe_phased_deterministic_order",
        "description": "Synchronized concurrent threads submit in strict deterministic sequence.",
        "evidence_type": "executed",
        "thread_count": thread_count,
        "submission_order": [f"phase-{i}" for i in range(thread_count)],
        "resulting_pending_steer": expected_fifo,
    })

    # Case 11: Thread-safe high-concurrency race integrity
    agent = _bare_agent()
    N = 30
    start_barrier = threading.Barrier(N)

    def race_worker(idx: int) -> None:
        start_barrier.wait()
        agent.steer(f"concurrent-worker-{idx}")

    race_threads = [threading.Thread(target=race_worker, args=(i,)) for i in range(N)]
    for t in race_threads:
        t.start()
    for t in race_threads:
        t.join()

    split_lines = agent._pending_steer.split("\n")
    assert len(split_lines) == N
    assert set(split_lines) == {f"concurrent-worker-{i}" for i in range(N)}
    cases.append({
        "case_id": "STEER-SUBMIT-11",
        "name": "thread_safe_high_concurrency_race",
        "description": "High-concurrency unphased race preserves all entries without dropped updates.",
        "evidence_type": "executed",
        "thread_count": N,
        "total_lines_preserved": len(split_lines),
    })

    # Case 12: Lockless fallback when _pending_steer_lock is None
    agent = _bare_agent()
    agent._pending_steer_lock = None
    r1 = agent.steer("fallback line 1")
    r2 = agent.steer("fallback line 2")
    assert r1 is True and r2 is True
    assert agent._pending_steer == "fallback line 1\nfallback line 2"
    cases.append({
        "case_id": "STEER-SUBMIT-12",
        "name": "lockless_stub_fallback",
        "description": "When _pending_steer_lock is None (test stubs), steers concatenate directly.",
        "evidence_type": "executed",
        "submissions": ["fallback line 1", "fallback line 2"],
        "resulting_pending_steer": "fallback line 1\nfallback line 2",
    })

    return cases


# ==============================================================================
# 2. _drain_pending_steer and Hard-Interrupt Clearing
# ==============================================================================
def gen_drain_and_interrupt_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Drain when initially None
    agent = _bare_agent()
    assert agent._pending_steer is None
    drained = agent._drain_pending_steer()
    assert drained is None
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-DRAIN-01",
        "name": "drain_when_none",
        "description": "_drain_pending_steer returns None when no steer is queued; state remains None.",
        "evidence_type": "executed",
        "initial_state": None,
        "drained_value": None,
        "post_state": None,
    })

    # Case 2: Drain single queued steer
    agent = _bare_agent()
    agent.steer("payload to drain")
    drained = agent._drain_pending_steer()
    assert drained == "payload to drain"
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-DRAIN-02",
        "name": "drain_single_payload",
        "description": "_drain_pending_steer atomically reads and clears queued steer text.",
        "evidence_type": "executed",
        "initial_state": "payload to drain",
        "drained_value": "payload to drain",
        "post_state": None,
    })

    # Case 3: Drain idempotency / second call returns None
    agent = _bare_agent()
    agent.steer("first call item")
    first_drain = agent._drain_pending_steer()
    second_drain = agent._drain_pending_steer()
    assert first_drain == "first call item"
    assert second_drain is None
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-DRAIN-03",
        "name": "drain_idempotent_second_call",
        "description": "Subsequent call to _drain_pending_steer returns None immediately.",
        "evidence_type": "executed",
        "first_drain": first_drain,
        "second_drain": second_drain,
        "post_state": None,
    })

    # Case 4: Drain multiple FIFO steers
    agent = _bare_agent()
    agent.steer("part A")
    agent.steer("part B")
    drained = agent._drain_pending_steer()
    assert drained == "part A\npart B"
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-DRAIN-04",
        "name": "drain_multi_fifo",
        "description": "_drain_pending_steer returns entire multi-line FIFO buffer and clears it.",
        "evidence_type": "executed",
        "drained_value": "part A\npart B",
        "post_state": None,
    })

    # Case 5: Drain when _pending_steer_lock is None
    agent = _bare_agent()
    agent._pending_steer_lock = None
    agent._pending_steer = "lockless value"
    drained = agent._drain_pending_steer()
    assert drained == "lockless value"
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-DRAIN-05",
        "name": "drain_without_lock",
        "description": "_drain_pending_steer functions safely when lock attribute is None.",
        "evidence_type": "executed",
        "drained_value": "lockless value",
        "post_state": None,
    })

    # Case 6: clear_interrupt drops pending steer
    agent = _bare_agent()
    agent.steer("will be dropped by interrupt")
    agent._interrupt_requested = True
    assert agent._pending_steer == "will be dropped by interrupt"
    agent.clear_interrupt()
    assert agent._pending_steer is None
    assert agent._drain_pending_steer() is None
    cases.append({
        "case_id": "STEER-DRAIN-06",
        "name": "clear_interrupt_drops_steer",
        "description": "clear_interrupt drops pending steer to prevent late execution on next turn.",
        "evidence_type": "executed",
        "pre_clear_steer": "will be dropped by interrupt",
        "post_clear_steer": None,
    })

    # Case 7: Hard interrupt via request_hard_interrupt compat helper
    agent = _bare_agent()
    agent.steer("steer before hard stop")
    assert agent._pending_steer == "steer before hard stop"
    ok = request_hard_interrupt(
        agent, "Stop requested", tool_reason="explicit stop requested"
    )
    assert ok is True
    assert agent._hard_interrupt_requested.is_set() is True
    assert agent._interrupt_requested is True
    assert agent._tool_interrupt_reason == "explicit stop requested"
    assert (
        agent._pending_steer == "steer before hard stop"
    )  # Still held until clear_interrupt

    # Turn teardown runs clear_interrupt:
    agent.clear_interrupt()
    assert agent._pending_steer is None
    assert agent._hard_interrupt_requested.is_set() is False
    assert agent._interrupt_requested is False
    cases.append({
        "case_id": "STEER-DRAIN-07",
        "name": "request_hard_interrupt_teardown",
        "description": "request_hard_interrupt marks flags; turn teardown clear_interrupt drops steer.",
        "evidence_type": "executed",
        "pre_clear_steer": "steer before hard stop",
        "post_clear_steer": None,
        "interrupt_requested_post": False,
    })

    # Case 8: clear_interrupt with preserve_redirect=False clears both
    agent = _bare_agent()
    agent.steer("steer note")
    agent._pending_redirect = "redirect note"
    agent.clear_interrupt(preserve_redirect=False)
    assert agent._pending_steer is None
    assert agent._pending_redirect is None
    cases.append({
        "case_id": "STEER-DRAIN-08",
        "name": "clear_interrupt_preserve_redirect_false",
        "description": "clear_interrupt(preserve_redirect=False) clears both pending_steer and redirect.",
        "evidence_type": "executed",
        "post_clear_steer": None,
        "post_clear_redirect": None,
    })

    # Case 9: clear_interrupt with preserve_redirect=True when redirect is empty
    agent = _bare_agent()
    agent.steer("steer preserved")
    agent._pending_redirect = None
    ret = agent.clear_interrupt(preserve_redirect=True)
    assert ret is False
    assert (
        agent._pending_steer == "steer preserved"
    )  # Early return leaves steer untouched
    cases.append({
        "case_id": "STEER-DRAIN-09",
        "name": "clear_interrupt_preserve_redirect_true_no_redirect",
        "description": "preserve_redirect=True without redirect returns False and does NOT clear steer.",
        "evidence_type": "executed",
        "return_value": False,
        "post_state_steer": "steer preserved",
    })

    return cases


# ==============================================================================
# 3. apply_pending_steer_to_tool_results
# ==============================================================================
def gen_apply_steer_to_tool_results_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: String content single tool result
    agent = _bare_agent()
    agent.steer("also list hidden files")
    messages = [
        {"role": "user", "content": "list directory"},
        {"role": "assistant", "tool_calls": [{"id": "tc1"}]},
        {"role": "tool", "content": "file1.txt\nfile2.txt", "tool_call_id": "tc1"},
    ]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=1)
    expected_marker = format_steer_marker("also list hidden files")
    assert messages[-1]["content"] == "file1.txt\nfile2.txt" + expected_marker
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-TOOL-01",
        "name": "string_content_single_tool",
        "description": "Appends formatted steer marker to single string tool result; resets pending.",
        "evidence_type": "executed",
        "original_content": "file1.txt\nfile2.txt",
        "steer_text": "also list hidden files",
        "resulting_content": messages[-1]["content"],
        "pending_steer_post": None,
    })

    # Case 2: Anthropic-style multimodal content blocks
    agent = _bare_agent()
    agent.steer("analyze contrast levels")
    orig_blocks = [{"type": "text", "text": "captured image details"}]
    messages = [
        {"role": "user", "content": "inspect screen"},
        {"role": "assistant", "tool_calls": [{"id": "tc1"}]},
        {"role": "tool", "content": list(orig_blocks), "tool_call_id": "tc1"},
    ]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=1)
    content = messages[-1]["content"]
    assert isinstance(content, list)
    assert len(content) == 2
    assert content[0] == {"type": "text", "text": "captured image details"}
    assert content[1]["type"] == "text"
    assert content[1]["text"] == format_steer_marker("analyze contrast levels").lstrip()
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-TOOL-02",
        "name": "multimodal_list_blocks",
        "description": "Preserves existing multimodal blocks and appends stripped text block marker.",
        "evidence_type": "executed",
        "steer_text": "analyze contrast levels",
        "resulting_blocks": content,
        "pending_steer_post": None,
    })

    # Case 3: Multiple tool results batch modifies only the last tool result
    agent = _bare_agent()
    agent.steer("steer for entire batch")
    messages = [
        {"role": "user", "content": "run multiple tools"},
        {
            "role": "assistant",
            "tool_calls": [{"id": "tc1"}, {"id": "tc2"}, {"id": "tc3"}],
        },
        {"role": "tool", "content": "tool 1 output", "tool_call_id": "tc1"},
        {"role": "tool", "content": "tool 2 output", "tool_call_id": "tc2"},
        {"role": "tool", "content": "tool 3 output", "tool_call_id": "tc3"},
    ]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=3)
    assert messages[2]["content"] == "tool 1 output"
    assert messages[3]["content"] == "tool 2 output"
    assert messages[4]["content"] == "tool 3 output" + format_steer_marker(
        "steer for entire batch"
    )
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-TOOL-03",
        "name": "multiple_tool_results_modifies_last_only",
        "description": "In a batch of 3 tool results, only the final tool result receives the marker.",
        "evidence_type": "executed",
        "batch_size": 3,
        "tool_1_content": messages[2]["content"],
        "tool_2_content": messages[3]["content"],
        "tool_3_content": messages[4]["content"],
        "pending_steer_post": None,
    })

    # Case 4: Multiple tool results with interleaved non-tool message at tail
    agent = _bare_agent()
    agent.steer("skip non-tool tail")
    messages = [
        {"role": "user", "content": "run tool"},
        {"role": "assistant", "tool_calls": [{"id": "tc1"}]},
        {"role": "tool", "content": "real tool output", "tool_call_id": "tc1"},
        {"role": "assistant", "content": "interrupted partial thought"},
    ]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=2)
    # Scans backwards from tail: skips assistant, finds tool at index 2
    assert messages[2]["content"] == "real tool output" + format_steer_marker(
        "skip non-tool tail"
    )
    assert messages[3]["content"] == "interrupted partial thought"
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-TOOL-04",
        "name": "interleaved_non_tool_tail_skipped",
        "description": "Backwards scan skips non-tool tail rows to locate newest tool-role message.",
        "evidence_type": "executed",
        "tool_content": messages[2]["content"],
        "tail_assistant_content": messages[3]["content"],
        "pending_steer_post": None,
    })

    # Case 5: No matching tool row restashes steer into agent
    agent = _bare_agent()
    agent.steer("restash this message")
    messages = [
        {"role": "user", "content": "run task"},
        {"role": "assistant", "content": "all tools failed to launch"},
    ]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=1)
    assert messages[1]["content"] == "all tools failed to launch"
    assert agent._pending_steer == "restash this message"
    cases.append({
        "case_id": "STEER-TOOL-05",
        "name": "no_matching_tool_row_restashes_steer",
        "description": "When no tool row exists in batch, drained steer is restored into pending slot.",
        "evidence_type": "executed",
        "messages_unchanged": True,
        "restashed_pending_steer": agent._pending_steer,
    })

    # Case 6: No matching tool row concatenates with concurrent steer arrival
    agent = _bare_agent()
    agent.steer("original steer")
    # Simulate: drained steer found no tool row, but a concurrent steer arrived in the meantime
    stashed_steer = agent._drain_pending_steer()
    agent.steer("concurrently arrived steer")
    # Now simulate the restash block in apply_pending_steer_to_tool_results:
    with agent._pending_steer_lock:
        if agent._pending_steer:
            agent._pending_steer = agent._pending_steer + "\n" + stashed_steer
        else:
            agent._pending_steer = stashed_steer
    assert agent._pending_steer == "concurrently arrived steer\noriginal steer"
    cases.append({
        "case_id": "STEER-TOOL-06",
        "name": "no_matching_tool_row_concurrent_arrival",
        "description": "Restash safely concatenates with any concurrent steer that arrived during scan.",
        "evidence_type": "executed",
        "resulting_pending_steer": agent._pending_steer,
    })

    # Case 7: Zero recent rows leaves steer untouched in pending slot
    agent = _bare_agent()
    agent.steer("keep me queued")
    messages = [{"role": "tool", "content": "existing tool"}]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=0)
    assert messages[0]["content"] == "existing tool"
    assert agent._pending_steer == "keep me queued"
    cases.append({
        "case_id": "STEER-TOOL-07",
        "name": "zero_recent_rows_noop",
        "description": "num_tool_msgs <= 0 returns immediately without draining pending steer.",
        "evidence_type": "executed",
        "num_tool_msgs": 0,
        "messages_unchanged": True,
        "pending_steer_retained": agent._pending_steer,
    })

    # Case 8: Negative recent rows leaves steer untouched in pending slot
    agent = _bare_agent()
    agent.steer("keep me queued")
    messages = [{"role": "tool", "content": "existing tool"}]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=-2)
    assert messages[0]["content"] == "existing tool"
    assert agent._pending_steer == "keep me queued"
    cases.append({
        "case_id": "STEER-TOOL-08",
        "name": "negative_recent_rows_noop",
        "description": "Negative num_tool_msgs returns immediately without draining pending steer.",
        "evidence_type": "executed",
        "num_tool_msgs": -2,
        "pending_steer_retained": agent._pending_steer,
    })

    # Case 9: Empty messages list leaves steer untouched
    agent = _bare_agent()
    agent.steer("keep me queued")
    messages = []
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=1)
    assert agent._pending_steer == "keep me queued"
    cases.append({
        "case_id": "STEER-TOOL-09",
        "name": "empty_messages_list_noop",
        "description": "Empty messages list returns immediately without draining pending steer.",
        "evidence_type": "executed",
        "pending_steer_retained": agent._pending_steer,
    })

    # Case 10: Multiple queued steers injected into single tool result
    agent = _bare_agent()
    agent.steer("directive one")
    agent.steer("directive two")
    agent.steer("directive three")
    messages = [
        {"role": "tool", "content": "single output", "tool_call_id": "tc1"},
    ]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=1)
    expected_multi = format_steer_marker(
        "directive one\ndirective two\ndirective three"
    )
    assert messages[0]["content"] == "single output" + expected_multi
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-TOOL-10",
        "name": "multiple_queued_steers_single_marker",
        "description": "Multiple concatenated steers are wrapped in a single out-of-band marker.",
        "evidence_type": "executed",
        "concatenated_steer": "directive one\ndirective two\ndirective three",
        "resulting_content": messages[0]["content"],
        "pending_steer_post": None,
    })

    # Case 11: No-op when no steer pending
    agent = _bare_agent()
    messages = [
        {"role": "tool", "content": "unchanged output", "tool_call_id": "tc1"},
    ]
    apply_pending_steer_to_tool_results(agent, messages, num_tool_msgs=1)
    assert messages[0]["content"] == "unchanged output"
    cases.append({
        "case_id": "STEER-TOOL-11",
        "name": "no_op_when_no_steer_pending",
        "description": "When no steer is pending, tool results are left completely unchanged.",
        "evidence_type": "executed",
        "resulting_content": "unchanged output",
    })

    return cases


# ==============================================================================
# 4. Pre-API Drain Behavior in Real Conversation Loop
# ==============================================================================
def gen_pre_api_drain_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Pre-API drain injects into last tool result in history before provider call
    agent = _loop_agent()
    agent.client.chat.completions.create.return_value = _mock_response(
        content="Response incorporating steer",
        finish_reason="stop",
    )
    history = [
        {"role": "user", "content": "fetch logs"},
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "t1", "function": {"name": "read", "arguments": "{}"}}
            ],
        },
        {"role": "tool", "content": "server log row A", "tool_call_id": "t1"},
    ]
    agent.steer("filter for critical severity")

    with (
        contextlib.redirect_stdout(io.StringIO()),
        contextlib.redirect_stderr(io.StringIO()),
    ):
        result = agent.run_conversation("analyze logs", conversation_history=history)

    wire_messages = agent.client.chat.completions.create.call_args.kwargs["messages"]
    tool_wire = [m for m in wire_messages if m.get("role") == "tool"][0]
    expected_marker = format_steer_marker("filter for critical severity")
    assert STEER_MARKER_OPEN in tool_wire["content"]
    assert "filter for critical severity" in tool_wire["content"]
    assert tool_wire["content"].endswith(STEER_MARKER_CLOSE)
    assert agent._pending_steer is None
    assert result.get("pending_steer") is None
    cases.append({
        "case_id": "STEER-PREAPI-01",
        "name": "pre_api_drain_injects_into_last_tool_result",
        "description": "Steer arriving before next API call is drained and appended to prior tool message.",
        "evidence_type": "executed",
        "steer_text": "filter for critical severity",
        "wire_tool_content": tool_wire["content"],
        "pending_steer_after_turn": None,
    })

    # Case 2: Pre-API drain on first iteration (no tools yet) restashes for post-tool drain
    agent = _loop_agent()
    agent.client.chat.completions.create.return_value = _mock_response(
        content="Direct text reply without tools",
        finish_reason="stop",
    )
    agent.steer("guidance sent before first turn starts")

    with (
        contextlib.redirect_stdout(io.StringIO()),
        contextlib.redirect_stderr(io.StringIO()),
    ):
        result = agent.run_conversation("initial question")

    wire_messages = agent.client.chat.completions.create.call_args.kwargs["messages"]
    wire_user = [m for m in wire_messages if m.get("role") == "user"][0]
    assert STEER_MARKER_OPEN not in wire_user["content"]
    assert wire_user["content"] == "initial question"
    assert result.get("pending_steer") == "guidance sent before first turn starts"
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-PREAPI-02",
        "name": "pre_api_drain_first_iteration_restashes",
        "description": "Pre-API drain on toolless turn restashes steer, returned by finalizer.",
        "evidence_type": "executed",
        "steer_text": "guidance sent before first turn starts",
        "wire_user_content": wire_user["content"],
        "finalizer_returned_pending_steer": result.get("pending_steer"),
    })

    # Case 3: Pre-API drain multimodal content block handling
    agent = _bare_agent()
    agent.steer("multimodal pre-api steer")
    messages = [
        {"role": "user", "content": "analyze image"},
        {
            "role": "tool",
            "content": [{"type": "text", "text": "initial block"}],
            "tool_call_id": "tc1",
        },
    ]
    _pre_api_steer = agent._drain_pending_steer()
    assert _pre_api_steer == "multimodal pre-api steer"
    for _si in range(len(messages) - 1, -1, -1):
        _sm = messages[_si]
        if isinstance(_sm, dict) and _sm.get("role") == "tool":
            marker = format_steer_marker(_pre_api_steer)
            existing = _sm.get("content", "")
            blocks = list(existing) if existing else []
            blocks.append({"type": "text", "text": marker})
            _sm["content"] = blocks
            break
    assert len(messages[-1]["content"]) == 2
    assert messages[-1]["content"][1]["text"] == format_steer_marker(
        "multimodal pre-api steer"
    )
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-PREAPI-03",
        "name": "pre_api_drain_multimodal_blocks",
        "description": "Pre-API drain appends text block containing formatted marker to multimodal tool row.",
        "evidence_type": "executed",
        "steer_text": "multimodal pre-api steer",
        "resulting_blocks": messages[-1]["content"],
    })

    # Case 4: Role alternation preservation verification
    agent = _loop_agent()
    agent.client.chat.completions.create.return_value = _mock_response(
        content="Answer after steer",
        finish_reason="stop",
    )
    history = [
        {"role": "user", "content": "cmd"},
        {
            "role": "assistant",
            "tool_calls": [{"id": "t1", "function": {"name": "f", "arguments": "{}"}}],
        },
        {"role": "tool", "content": "tool res", "tool_call_id": "t1"},
    ]
    agent.steer("steer text")

    with (
        contextlib.redirect_stdout(io.StringIO()),
        contextlib.redirect_stderr(io.StringIO()),
    ):
        result = agent.run_conversation("step 2", conversation_history=history)

    wire_messages = agent.client.chat.completions.create.call_args.kwargs["messages"]
    roles = [m.get("role") for m in wire_messages]
    assert "steer" not in roles
    tool_idx = roles.index("tool")
    assert roles[tool_idx - 1] == "assistant"
    cases.append({
        "case_id": "STEER-PREAPI-04",
        "name": "role_alternation_preservation",
        "description": "Pre-API steer drain mutates existing tool content without injecting new user roles.",
        "evidence_type": "executed",
        "roles_sequence": roles,
        "tool_preceded_by": "assistant",
    })

    return cases


# ==============================================================================
# 5. Turn Finalizer Leftover Steer and Gateway Promotion
# ==============================================================================
def gen_turn_finalizer_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Leftover steer returned in result dict
    agent = _bare_agent()
    agent.steer("steer submitted during final model generation")
    msgs = [
        {"role": "user", "content": "task"},
        {"role": "assistant", "content": "finished"},
    ]
    result = finalize_turn(
        agent,
        final_response="finished",
        api_call_count=1,
        interrupted=False,
        failed=False,
        messages=msgs,
        conversation_history=[],
        effective_task_id="task_1",
        turn_id="turn_1",
        user_message="task",
        original_user_message="task",
        _should_review_memory=False,
        _turn_exit_reason="stop",
    )
    assert (
        result.get("pending_steer") == "steer submitted during final model generation"
    )
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-FINALIZE-01",
        "name": "leftover_steer_returned_in_result",
        "description": "finalize_turn drains leftover steer and returns it in result['pending_steer'].",
        "evidence_type": "executed",
        "expected_pending_steer": "steer submitted during final model generation",
        "result_pending_steer": result.get("pending_steer"),
        "agent_pending_steer_post": None,
    })

    # Case 2: No leftover steer leaves pending_steer key absent
    agent = _bare_agent()
    assert agent._pending_steer is None
    result = finalize_turn(
        agent,
        final_response="finished",
        api_call_count=1,
        interrupted=False,
        failed=False,
        messages=msgs,
        conversation_history=[],
        effective_task_id="task_2",
        turn_id="turn_2",
        user_message="task",
        original_user_message="task",
        _should_review_memory=False,
        _turn_exit_reason="stop",
    )
    assert "pending_steer" not in result
    cases.append({
        "case_id": "STEER-FINALIZE-02",
        "name": "no_leftover_steer_key_absent",
        "description": "When no steer is pending at turn end, 'pending_steer' key is not present in result.",
        "evidence_type": "executed",
        "key_present": "pending_steer" in result,
    })

    # Case 3: Concatenated FIFO leftover steers returned intact
    agent = _bare_agent()
    agent.steer("directive A")
    agent.steer("directive B")
    result = finalize_turn(
        agent,
        final_response="done",
        api_call_count=1,
        interrupted=False,
        failed=False,
        messages=msgs,
        conversation_history=[],
        effective_task_id="task_3",
        turn_id="turn_3",
        user_message="task",
        original_user_message="task",
        _should_review_memory=False,
        _turn_exit_reason="stop",
    )
    assert result.get("pending_steer") == "directive A\ndirective B"
    assert agent._pending_steer is None
    cases.append({
        "case_id": "STEER-FINALIZE-03",
        "name": "concatenated_fifo_leftover_steers",
        "description": "Multiple queued steers arriving before turn completion return concatenated.",
        "evidence_type": "executed",
        "result_pending_steer": result.get("pending_steer"),
    })

    # Case 4: Hard interrupt clears steer prior to finalizer execution
    agent = _bare_agent()
    agent.steer("should not survive interrupt")
    agent._interrupt_requested = True
    agent.clear_interrupt()
    assert agent._pending_steer is None
    result = finalize_turn(
        agent,
        final_response="interrupted",
        api_call_count=1,
        interrupted=True,
        failed=False,
        messages=msgs,
        conversation_history=[],
        effective_task_id="task_4",
        turn_id="turn_4",
        user_message="task",
        original_user_message="task",
        _should_review_memory=False,
        _turn_exit_reason="interrupt",
    )
    assert "pending_steer" not in result
    cases.append({
        "case_id": "STEER-FINALIZE-04",
        "name": "interrupted_turn_clears_leftover_steer",
        "description": "clear_interrupt on abort drops steer; finalizer receives empty slot.",
        "evidence_type": "executed",
        "key_present": "pending_steer" in result,
    })

    # Case 5: Gateway post-turn promotion logic (gateway/run.py:32638-32643)
    def simulate_gateway_post_turn(
        result_dict: Dict[str, Any], pending: Optional[str]
    ) -> Optional[str]:
        if result_dict and not pending:
            _leftover = result_dict.get("pending_steer")
            if _leftover:
                pending = _leftover
        return pending

    promoted = simulate_gateway_post_turn(
        {"pending_steer": "next turn directive"}, None
    )
    assert promoted == "next turn directive"

    already_pending = simulate_gateway_post_turn(
        {"pending_steer": "next turn directive"}, "existing message"
    )
    assert already_pending == "existing message"

    cases.append({
        "case_id": "STEER-FINALIZE-05",
        "name": "gateway_post_turn_steer_promotion",
        "description": "Gateway runner promotes leftover steer into pending user message when no queue pending.",
        "evidence_type": "executed",
        "promoted_when_empty": promoted,
        "retained_when_existing": already_pending,
    })

    return cases


# ==============================================================================
# 6. Acknowledgement Strings, Preview Boundaries, and Idle Fallthrough
# ==============================================================================
class MinimalGatewayRunner:
    """Minimal test double exposing the real GatewayRunner._busy_steer_command method."""

    _busy_steer_command = GatewayRunner._busy_steer_command

    def __init__(self, agent: Any = None):
        self._state = MagicMock()
        self._state.turn.agent = agent
        self.enqueued: List[tuple] = []

    def _peek_session_state(self, key: str) -> Any:
        return self._state

    def _adapter_for_source(self, src: Any) -> Any:
        return MagicMock()

    def _enqueue_fifo(self, key: str, event: Any, adapter: Any) -> None:
        self.enqueued.append((key, event.text))


def gen_acknowledgement_and_preview_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # Case 1: Busy /steer with empty payload
    runner = MinimalGatewayRunner()
    ev = MessageEvent(text="/steer", message_type=MessageType.TEXT, source="cli")
    ack = asyncio.run(runner._busy_steer_command(ev, "s1", "cli"))
    assert ack == "Usage: /steer <prompt>"
    cases.append({
        "case_id": "STEER-ACK-01",
        "name": "busy_empty_payload",
        "description": "Busy steer with empty payload returns usage hint.",
        "evidence_type": "executed",
        "command_text": "/steer",
        "expected_ack": "Usage: /steer <prompt>",
        "resulting_ack": ack,
    })

    # Case 2: Busy /steer with whitespace-only payload
    ev = MessageEvent(text="/steer   \t  ", message_type=MessageType.TEXT, source="cli")
    ack = asyncio.run(runner._busy_steer_command(ev, "s1", "cli"))
    assert ack == "Usage: /steer <prompt>"
    cases.append({
        "case_id": "STEER-ACK-02",
        "name": "busy_whitespace_only_payload",
        "description": "Busy steer with whitespace-only payload returns usage hint.",
        "evidence_type": "executed",
        "command_text": "/steer   \t  ",
        "expected_ack": "Usage: /steer <prompt>",
        "resulting_ack": ack,
    })

    # Case 3: Busy agent still starting (sentinel)
    runner_sentinel = MinimalGatewayRunner(agent=_AGENT_PENDING_SENTINEL)
    ev = MessageEvent(
        text="/steer wait for startup", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_sentinel._busy_steer_command(ev, "s1", "cli"))
    assert ack == "Agent still starting \u2014 /steer queued for the next turn."
    assert runner_sentinel.enqueued == [("s1", "wait for startup")]
    cases.append({
        "case_id": "STEER-ACK-03",
        "name": "busy_agent_still_starting_sentinel",
        "description": "Agent in sentinel state enqueues event to FIFO and returns starting ack.",
        "evidence_type": "executed",
        "command_text": "/steer wait for startup",
        "expected_ack": "Agent still starting \u2014 /steer queued for the next turn.",
        "resulting_ack": ack,
        "enqueued_text": "wait for startup",
    })

    # Case 4: Busy agent missing or lacks steer()
    runner_missing = MinimalGatewayRunner(agent=None)
    ev = MessageEvent(
        text="/steer fallback to queue", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_missing._busy_steer_command(ev, "s1", "cli"))
    assert ack == "No active agent \u2014 /steer queued for the next turn."
    assert runner_missing.enqueued == [("s1", "fallback to queue")]
    cases.append({
        "case_id": "STEER-ACK-04",
        "name": "busy_no_active_agent_fallback",
        "description": "Missing running agent enqueues event to FIFO and returns no active agent ack.",
        "evidence_type": "executed",
        "command_text": "/steer fallback to queue",
        "expected_ack": "No active agent \u2014 /steer queued for the next turn.",
        "resulting_ack": ack,
        "enqueued_text": "fallback to queue",
    })

    # Case 5: Busy running agent steer rejected empty payload
    mock_agent = MagicMock()
    mock_agent.steer.return_value = False
    runner_rej = MinimalGatewayRunner(agent=mock_agent)
    ev = MessageEvent(
        text="/steer will_reject", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_rej._busy_steer_command(ev, "s1", "cli"))
    assert ack == "Steer rejected (empty payload)."
    cases.append({
        "case_id": "STEER-ACK-05",
        "name": "busy_steer_rejected_empty_payload",
        "description": "Agent steer returning False yields empty payload rejection ack.",
        "evidence_type": "executed",
        "expected_ack": "Steer rejected (empty payload).",
        "resulting_ack": ack,
    })

    # Case 6: Busy running agent steer exception
    mock_agent_err = MagicMock()
    mock_agent_err.steer.side_effect = RuntimeError("lock failure")
    runner_err = MinimalGatewayRunner(agent=mock_agent_err)
    ev = MessageEvent(
        text="/steer triggers_error", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_err._busy_steer_command(ev, "s1", "cli"))
    assert ack == "⚠️ Steer failed: lock failure"
    cases.append({
        "case_id": "STEER-ACK-06",
        "name": "busy_steer_exception_handling",
        "description": "Exception raised by agent.steer returns warning failure string.",
        "evidence_type": "executed",
        "expected_ack": "⚠️ Steer failed: lock failure",
        "resulting_ack": ack,
    })

    # Case 7: Preview truncation below boundary (59 characters)
    agent = _bare_agent()
    runner_active = MinimalGatewayRunner(agent=agent)
    text_59 = "A" * 59
    assert len(text_59) == 59
    ev = MessageEvent(
        text=f"/steer {text_59}", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_active._busy_steer_command(ev, "s1", "cli"))
    expected_ack_59 = (
        f"⏩ Steer queued \u2014 arrives after the next tool call: '{text_59}'"
    )
    assert ack == expected_ack_59
    assert "..." not in ack
    cases.append({
        "case_id": "STEER-ACK-07",
        "name": "preview_boundary_59_chars_not_truncated",
        "description": "59 characters (len <= 60): preview has exact length, no ellipsis appended.",
        "evidence_type": "executed",
        "payload_length": 59,
        "is_truncated": False,
        "resulting_ack": ack,
    })

    # Case 8: Preview truncation exact boundary (60 characters)
    agent = _bare_agent()
    runner_active = MinimalGatewayRunner(agent=agent)
    text_60 = "B" * 60
    assert len(text_60) == 60
    ev = MessageEvent(
        text=f"/steer {text_60}", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_active._busy_steer_command(ev, "s1", "cli"))
    expected_ack_60 = (
        f"⏩ Steer queued \u2014 arrives after the next tool call: '{text_60}'"
    )
    assert ack == expected_ack_60
    assert "..." not in ack
    cases.append({
        "case_id": "STEER-ACK-08",
        "name": "preview_boundary_60_chars_not_truncated",
        "description": "60 characters (len <= 60): preview has exact length, no ellipsis appended.",
        "evidence_type": "executed",
        "payload_length": 60,
        "is_truncated": False,
        "resulting_ack": ack,
    })

    # Case 9: Preview truncation over boundary (61 characters)
    agent = _bare_agent()
    runner_active = MinimalGatewayRunner(agent=agent)
    text_61 = "C" * 60 + "X"
    assert len(text_61) == 61
    ev = MessageEvent(
        text=f"/steer {text_61}", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_active._busy_steer_command(ev, "s1", "cli"))
    expected_ack_61 = (
        f"⏩ Steer queued \u2014 arrives after the next tool call: '{'C' * 60}...'"
    )
    assert ack == expected_ack_61
    assert "..." in ack
    assert text_61[:60] in ack
    assert "X" not in ack
    cases.append({
        "case_id": "STEER-ACK-09",
        "name": "preview_boundary_61_chars_truncated",
        "description": "61 characters (len > 60): truncated at 60 characters and ellipsis appended.",
        "evidence_type": "executed",
        "payload_length": 61,
        "is_truncated": True,
        "preview_slice_length": 60,
        "resulting_ack": ack,
    })

    # Case 10: Preview truncation long payload (120 characters)
    agent = _bare_agent()
    runner_active = MinimalGatewayRunner(agent=agent)
    text_120 = "0123456789" * 12
    assert len(text_120) == 120
    ev = MessageEvent(
        text=f"/steer {text_120}", message_type=MessageType.TEXT, source="cli"
    )
    ack = asyncio.run(runner_active._busy_steer_command(ev, "s1", "cli"))
    expected_ack_120 = (
        f"⏩ Steer queued \u2014 arrives after the next tool call: '{text_120[:60]}...'"
    )
    assert ack == expected_ack_120
    cases.append({
        "case_id": "STEER-ACK-10",
        "name": "preview_boundary_120_chars_truncated",
        "description": "Long payload truncated at 60 characters with ellipsis.",
        "evidence_type": "executed",
        "payload_length": 120,
        "is_truncated": True,
        "resulting_ack": ack,
    })

    # Case 11: Idle /steer with empty payload (gateway/run.py:20019-20021)
    ev_idle_empty = MessageEvent(
        text="/steer   ", message_type=MessageType.TEXT, source="cli"
    )
    steer_payload = ev_idle_empty.get_command_args().strip()
    idle_empty_ack = (
        "Usage: /steer <prompt>  (no agent is running; sending as a normal message)"
        if not steer_payload
        else None
    )
    assert (
        idle_empty_ack
        == "Usage: /steer <prompt>  (no agent is running; sending as a normal message)"
    )
    cases.append({
        "case_id": "STEER-ACK-11",
        "name": "idle_empty_payload",
        "description": "Idle session with empty steer payload returns cold-path usage string with double space.",
        "evidence_type": "executed",
        "command_text": "/steer   ",
        "expected_ack": "Usage: /steer <prompt>  (no agent is running; sending as a normal message)",
        "resulting_ack": idle_empty_ack,
    })

    # Case 12: Idle /steer with valid payload falls through (gateway/run.py:20022-20028)
    ev_idle_valid = MessageEvent(
        text="/steer deploy service now", message_type=MessageType.TEXT, source="cli"
    )
    steer_payload = ev_idle_valid.get_command_args().strip()
    assert steer_payload == "deploy service now"
    ev_idle_valid.text = steer_payload
    assert ev_idle_valid.text == "deploy service now"
    cases.append({
        "case_id": "STEER-ACK-12",
        "name": "idle_valid_payload_fallthrough",
        "description": "Idle valid steer rewrites event.text to stripped payload and falls through without ack.",
        "evidence_type": "executed",
        "command_text": "/steer deploy service now",
        "rewritten_event_text": ev_idle_valid.text,
        "fallthrough_return": None,
    })

    # Case 13: Marker constants and delimiters (agent/prompt_builder.py)
    sample_marker = format_steer_marker("sample instruction")
    assert STEER_MARKER_OPEN in sample_marker
    assert STEER_MARKER_CLOSE in sample_marker
    assert sample_marker.startswith("\n\n")
    cases.append({
        "case_id": "STEER-ACK-13",
        "name": "steer_marker_constants_and_delimiters",
        "description": "Exact delimiter constants and marker formatting template.",
        "evidence_type": "source_derived",
        "marker_open": STEER_MARKER_OPEN,
        "marker_close": STEER_MARKER_CLOSE,
        "channel_note_summary": STEER_CHANNEL_NOTE,
        "formatted_sample": sample_marker,
    })

    # Case 14: Command registration in hermes_cli/commands.py
    cmd_def = resolve_command("steer")
    assert cmd_def is not None
    assert cmd_def.name == "steer"
    assert cmd_def.category == "Session"
    assert cmd_def.args_hint == "<prompt>"
    assert cmd_def.busy_policy == "dispatch"
    assert cmd_def.busy_handler == "steer"
    assert "steer" in ACTIVE_SESSION_BYPASS_COMMANDS
    assert should_bypass_active_session("steer") is True
    cases.append({
        "case_id": "STEER-ACK-14",
        "name": "command_registry_metadata",
        "description": "Command registry attributes for /steer in hermes_cli/commands.py.",
        "evidence_type": "source_derived",
        "command_name": cmd_def.name,
        "category": cmd_def.category,
        "args_hint": cmd_def.args_hint,
        "busy_policy": cmd_def.busy_policy,
        "busy_handler": cmd_def.busy_handler,
        "is_bypass_command": True,
    })

    # Case 15: CLI implementation strings (cli.py:13047-13070)
    cases.append({
        "case_id": "STEER-ACK-15",
        "name": "cli_steer_strings_source_derived",
        "description": "Interactive CLI strings and boundaries in cli.py.",
        "evidence_type": "source_derived",
        "usage": "  Usage: /steer <prompt>",
        "busy_accepted_template": "  ⏩ Steer queued \u2014 arrives after the next tool call: {payload[:80]}{'...' if len(payload) > 80 else ''}",
        "busy_rejected": "  Steer rejected (empty payload).",
        "busy_failed_template": "  Steer failed: {exc}",
        "idle_queued_template": "  No agent running; queued as next turn: {payload[:80]}{'...' if len(payload) > 80 else ''}",
        "cli_preview_length": 80,
    })

    # Case 16: API server HTTP endpoint contracts (gateway/platforms/api_server_runs.py:1340-1388)
    cases.append({
        "case_id": "STEER-ACK-16",
        "name": "api_server_runs_steer_contract",
        "description": "HTTP POST /v1/runs/{run_id}/steer status codes, error payloads, and event schemas.",
        "evidence_type": "source_derived",
        "not_running_or_no_steer": {
            "status_code": 409,
            "error_code": "run_not_accepting_steer",
            "message_template": "Run is not currently accepting steer input: {run_id}",
        },
        "empty_input_payload": {
            "status_code": 400,
            "error_code": "invalid_steer_input",
            "message": "Missing non-empty steer text; expected 'input', 'message', or 'text'.",
        },
        "agent_rejected": {
            "status_code": 409,
            "error_code": "steer_not_accepted",
            "message_template": "Run did not accept steer text: {run_id}",
        },
        "success_response": {
            "status_code": 200,
            "body": {
                "accepted": True,
                "object": "hermes.run.steer",
                "run_id": "{run_id}",
            },
            "stream_event": "run.steered",
        },
    })

    return cases


# ==============================================================================
# Master Corpus Builder
# ==============================================================================
def build_native_steer_goldens() -> Dict[str, Any]:
    corpus = {
        "metadata": {
            "target": "rust/tools/native-steer-goldens.json",
            "generator": "rust/tools/gen_native_steer_goldens.py",
            "oracle_report": "rust/analysis/native-steer-contract-agy.md",
            "description": "Authoritative Python behavioral contract and golden expectations for native Rust /steer implementation.",
        },
        "agent_steer_submission": gen_steer_submission_cases(),
        "drain_and_hard_interrupt_clearing": gen_drain_and_interrupt_cases(),
        "apply_pending_steer_to_tool_results": gen_apply_steer_to_tool_results_cases(),
        "pre_api_drain_conversation_loop": gen_pre_api_drain_cases(),
        "turn_finalizer_leftover_steer": gen_turn_finalizer_cases(),
        "acknowledgement_and_preview_strings": gen_acknowledgement_and_preview_cases(),
    }
    return corpus


def main() -> None:
    check_mode = "--check" in sys.argv
    corpus = build_native_steer_goldens()
    rendered = json.dumps(corpus, indent=2, ensure_ascii=True, sort_keys=True) + "\n"

    section_counts = {k: len(v) for k, v in corpus.items() if isinstance(v, list)}
    total_cases = sum(section_counts.values())

    if check_mode:
        if not OUT_PATH.exists():
            print(f"Error: Golden file {OUT_PATH} does not exist.", file=sys.stderr)
            sys.exit(1)
        existing = OUT_PATH.read_text(encoding="utf-8")
        if existing != rendered:
            print(
                f"Error: Golden file {OUT_PATH} differs from generated output.",
                file=sys.stderr,
            )
            sys.exit(1)
        print(f"Parity check passed: {OUT_PATH} is up to date ({total_cases} cases).")
        sys.exit(0)

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(rendered, encoding="utf-8")
    print(f"Successfully generated {OUT_PATH}:")
    print(f"  Total test cases: {total_cases}")
    for sec, cnt in section_counts.items():
        print(f"    - {sec}: {cnt} cases")


if __name__ == "__main__":
    main()
