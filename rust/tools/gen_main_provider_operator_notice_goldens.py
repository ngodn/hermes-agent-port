#!/usr/bin/env python3
"""Deterministic source-executed oracle for main chat provider retry and fallback notice buffering contract.

This generator audits and executes live Python decision functions and runtime transitions
governing operator notice buffering, retry suppression, and fallback notification:
1. Buffering primitives: _buffer_status, _buffer_vprint, and lazy buffer initialization.
2. Recovered success lifecycle: one-shot fallback notice emission via _emit_pending_fallback_notice
   and silent retry chatter suppression via _clear_status_buffer.
3. Terminal failure lifecycle: buffer replay in exact FIFO order via _flush_status_buffer and
   proactive discard of pending fallback notice to prevent stale leaks.
4. Multiple switches: chained failovers (primary -> fb1 -> fb2), order preservation, multi-notice
   emission on success, and interleaved trace playback on terminal failure.
5. Callback failure resilience: swallowed surface exceptions, loop continuation across pending notices,
   and pre-dispatch buffer draining to prevent double-emission.
6. Clear and flush idempotence: empty buffers, missing attributes on test doubles, repeated calls,
   and clean cross-method transitions.
7. Representative fallback reason strings: exact live source formatting from _fallback_reason_text
   and try_activate_fallback for auth, rate limit, transport, and policy failovers.

Usage:
    python3 rust/tools/gen_main_provider_operator_notice_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_operator_notice_goldens.py --check  # check parity
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import time
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-operator-notice-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies by re-execing with repository virtualenv if needed.
if "dotenv" not in sys.modules:
    try:
        import dotenv  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

from agent.chat_completion_helpers import (
    _fallback_reason_text,
    try_activate_fallback,
)
from agent.error_classifier import FailoverReason
from run_agent import AIAgent


def _escape_em_dash(val: Any) -> Any:
    """Sanitize strings so that any unicode em dash (\\u2014) is escaped as '\\\\u2014'."""
    if isinstance(val, str):
        return val.replace("\u2014", "\\u2014")
    if isinstance(val, list):
        return [_escape_em_dash(v) for v in val]
    if isinstance(val, dict):
        return {k: _escape_em_dash(v) for k, v in val.items()}
    return val


def _make_bare_agent_double(
    model: str = "claude-3-5-sonnet",
    provider: str = "anthropic",
    base_url: str = "https://api.anthropic.com/v1",
    fallback_chain: Optional[List[Dict[str, Any]]] = None,
    log_prefix: str = "",
) -> AIAgent:
    """Construct a minimal AIAgent test double using AIAgent.__new__.

    Never runs AIAgent.__init__, isolating the pure-Python notice buffering
    and failover helpers.
    """
    agent = AIAgent.__new__(AIAgent)
    agent.model = model
    agent.provider = provider
    agent.requested_provider = provider
    agent.base_url = base_url
    agent.api_mode = "chat_completions"
    agent._primary_runtime = {
        "model": model,
        "provider": provider,
        "base_url": base_url,
    }
    agent._fallback_chain = list(fallback_chain or [])
    agent._fallback_index = 0
    agent._unavailable_fallback_keys = set()
    agent._retry_status_buffer = []
    agent._pending_fallback_notice = None
    agent._fallback_activated = False
    agent.log_prefix = log_prefix
    agent.status_callback = None
    agent.suppress_status_output = False
    agent._mute_post_response = False
    agent._executing_tools = False
    agent._print_fn = None
    agent.system_prompt = "You are a helpful assistant."
    agent._is_azure_openai_url = lambda url: False
    agent._is_direct_openai_url = lambda url: False
    agent._provider_model_requires_responses_api = lambda m, provider=None: False
    agent._credential_pool = None
    agent._try_activate_fallback = lambda reason=None: try_activate_fallback(
        agent, reason
    )
    agent._emit_status = lambda msg: None
    agent._vprint = lambda msg, force=False, **kw: None
    agent._emit_warning = lambda msg: None
    return agent


def _build_mock_client(base_url: str = "https://mock.example.com/v1") -> MagicMock:
    """Build a mock OpenAI-like client for resolve_provider_client."""
    mock = MagicMock()
    mock.base_url = base_url
    return mock


def generate_buffering_primitives() -> List[Dict[str, Any]]:
    """Test primitive accumulation and buffering methods on AIAgent."""
    rows: List[Dict[str, Any]] = []

    # Case 1: _buffer_status records a ("status", message) tuple
    agent1 = _make_bare_agent_double()
    msg1 = "⏳ Retrying in 1.0s (attempt 1/3)..."
    agent1._buffer_status(msg1)
    assert agent1._retry_status_buffer == [("status", msg1)]
    rows.append({
        "case_name": "buffer_status_appends_tuple",
        "category": "buffering_primitives",
        "description": "agent._buffer_status records (status, text) in _retry_status_buffer",
        "buffered_record": ["status", msg1],
        "buffer_len": len(agent1._retry_status_buffer),
    })

    # Case 2: _buffer_vprint records a ("vprint", message) tuple
    agent2 = _make_bare_agent_double()
    msg2 = "API call failed: 500 Internal Server Error"
    agent2._buffer_vprint(msg2)
    assert agent2._retry_status_buffer == [("vprint", msg2)]
    rows.append({
        "case_name": "buffer_vprint_appends_tuple",
        "category": "buffering_primitives",
        "description": "agent._buffer_vprint records (vprint, text) in _retry_status_buffer",
        "buffered_record": ["vprint", msg2],
        "buffer_len": len(agent2._retry_status_buffer),
    })

    # Case 3: Lazy initialization of buffer if attribute is None
    agent3 = _make_bare_agent_double()
    agent3._retry_status_buffer = None
    agent3._buffer_status("lazy init message")
    assert agent3._retry_status_buffer == [("status", "lazy init message")]
    rows.append({
        "case_name": "buffer_lazy_initialization_from_none",
        "category": "buffering_primitives",
        "description": "buffer methods lazily initialize _retry_status_buffer if set to None",
        "initial_buffer_is_none": True,
        "final_buffer_len": 1,
    })

    # Case 4: Preserves mixed kind FIFO sequence
    agent4 = _make_bare_agent_double()
    agent4._buffer_status("status-1")
    agent4._buffer_vprint("vprint-1")
    agent4._retry_status_buffer.append(("warn", "warn-1"))
    agent4._buffer_status("status-2")
    expected_tuples = [
        ["status", "status-1"],
        ["vprint", "vprint-1"],
        ["warn", "warn-1"],
        ["status", "status-2"],
    ]
    assert [list(t) for t in agent4._retry_status_buffer] == expected_tuples
    rows.append({
        "case_name": "buffer_preserves_mixed_sequence_order",
        "category": "buffering_primitives",
        "description": "interleaved status, vprint, and warn items retain exact FIFO arrival order",
        "expected_sequence": expected_tuples,
        "buffer_len": len(agent4._retry_status_buffer),
    })

    return rows


def generate_recovered_success() -> List[Dict[str, Any]]:
    """Test the recovered success lifecycle: emit pending fallback notice, drop retry buffer."""
    rows: List[Dict[str, Any]] = []

    # Case 1: Single fallback with recovery
    agent1 = _make_bare_agent_double(
        model="claude-3-5-sonnet",
        provider="anthropic",
        fallback_chain=[{"model": "gpt-4o", "provider": "openai"}],
    )
    emitted1: List[Dict[str, Any]] = []
    agent1._emit_status = lambda msg: emitted1.append({
        "channel": "status",
        "text": msg,
    })
    agent1._vprint = lambda msg, force=False, **kw: emitted1.append({
        "channel": "vprint",
        "text": msg,
        "force": force,
    })

    # 1a: Primary fails, retry chatter buffered
    retry_msg = "⏳ Retrying in 1.0s (attempt 1/3)..."
    pre_switch_msg = "⚠️ Rate limited \u2014 switching to fallback provider..."
    agent1._buffer_status(retry_msg)
    agent1._buffer_status(pre_switch_msg)

    # 1b: try_activate_fallback called from live source
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(_build_mock_client(), "gpt-4o"),
    ):
        activated = agent1._try_activate_fallback(FailoverReason.rate_limit)

    assert activated is True
    assert len(agent1._pending_fallback_notice) == 1
    fallback_notice = agent1._pending_fallback_notice[0]
    assert fallback_notice == (
        "⚠️ Model fallback: claude-3-5-sonnet via anthropic unavailable "
        "(rate limit); using gpt-4o via openai."
    )
    # Nothing emitted to operator yet
    assert emitted1 == []
    assert len(agent1._retry_status_buffer) == 3

    # 1c: Fallback succeeds! Success path executes:
    # 1. _emit_pending_fallback_notice()
    # 2. _clear_status_buffer()
    agent1._emit_pending_fallback_notice()
    agent1._clear_status_buffer()

    # Operator saw ONLY the one-shot fallback notice; chatter was silenced
    assert emitted1 == [{"channel": "status", "text": fallback_notice}]
    assert agent1._retry_status_buffer == []
    assert agent1._pending_fallback_notice is None

    rows.append({
        "case_name": "single_switch_recovered_success",
        "category": "recovered_success",
        "description": "On recovery, one-shot fallback notice is emitted once while noisy retries are dropped",
        "operations": [
            "_buffer_status(retry_msg)",
            "_buffer_status(pre_switch_msg)",
            "try_activate_fallback(rate_limit)",
            "_emit_pending_fallback_notice()",
            "_clear_status_buffer()",
        ],
        "emitted_events": emitted1,
        "final_state": {
            "retry_status_buffer": [],
            "pending_fallback_notice": None,
        },
        "invariants": {
            "only_durable_notice_emitted": len(emitted1) == 1,
            "retry_chatter_silenced": True,
            "buffer_drained": True,
            "pending_cleared": True,
        },
    })

    # Case 2: Pure primary recovery (retries then recovers without fallback)
    agent2 = _make_bare_agent_double()
    emitted2: List[Dict[str, Any]] = []
    agent2._emit_status = lambda msg: emitted2.append({
        "channel": "status",
        "text": msg,
    })

    agent2._buffer_status("⏳ Retrying in 1.0s (attempt 1/3)...")
    agent2._buffer_vprint("Temporary socket reset")
    assert agent2._pending_fallback_notice is None

    # Primary succeeds on attempt 2
    agent2._emit_pending_fallback_notice()
    agent2._clear_status_buffer()

    assert emitted2 == []
    assert agent2._retry_status_buffer == []
    rows.append({
        "case_name": "primary_recovery_without_fallback_silent",
        "category": "recovered_success",
        "description": "If primary recovers without activating fallback, zero operator notices are emitted",
        "operations": [
            "_buffer_status(retry)",
            "_buffer_vprint(socket_reset)",
            "_emit_pending_fallback_notice()",
            "_clear_status_buffer()",
        ],
        "emitted_events": emitted2,
        "final_state": {
            "retry_status_buffer": [],
            "pending_fallback_notice": None,
        },
        "invariants": {
            "zero_emissions": len(emitted2) == 0,
            "buffer_drained": True,
        },
    })

    # Case 3: Post-recovery idempotence (calling emit_pending or flush afterward is no-op)
    agent3 = _make_bare_agent_double()
    emitted3: List[Dict[str, Any]] = []
    agent3._emit_status = lambda msg: emitted3.append({
        "channel": "status",
        "text": msg,
    })
    agent3._pending_fallback_notice = "Fallback notice once"
    agent3._buffer_status("chatter")

    agent3._emit_pending_fallback_notice()
    agent3._clear_status_buffer()
    assert len(emitted3) == 1

    # Subsequent calls on a clean state
    agent3._emit_pending_fallback_notice()
    agent3._flush_status_buffer()
    assert len(emitted3) == 1  # No additional emissions

    rows.append({
        "case_name": "post_recovery_subsequent_calls_noop",
        "category": "recovered_success",
        "description": "Subsequent calls to _emit_pending_fallback_notice and _flush_status_buffer produce zero extra events",
        "emitted_count_after_first": 1,
        "emitted_count_after_second": 1,
        "invariants": {
            "no_duplicate_replays": True,
        },
    })

    return rows


def generate_terminal_failure() -> List[Dict[str, Any]]:
    """Test the terminal failure lifecycle: flush status buffer in FIFO order, drop pending notice."""
    rows: List[Dict[str, Any]] = []

    # Case 1: Terminal failure after fallback switch
    agent1 = _make_bare_agent_double(
        model="claude-3-5-sonnet",
        provider="anthropic",
        fallback_chain=[{"model": "gpt-4o", "provider": "openai"}],
        log_prefix="[hermes] ",
    )
    emitted1: List[Dict[str, Any]] = []
    agent1._emit_status = lambda msg: emitted1.append({
        "channel": "status",
        "text": msg,
    })
    agent1._vprint = lambda msg, force=False, **kw: emitted1.append({
        "channel": "vprint",
        "text": msg,
        "force": force,
    })
    agent1._emit_warning = lambda msg: emitted1.append({"channel": "warn", "text": msg})

    agent1._buffer_status("⏳ Primary attempt 1 failed")
    agent1._buffer_vprint("Connection refused by peer")
    agent1._buffer_status(
        "⚠️ Provider unreachable \u2014 switching to fallback provider..."
    )

    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(_build_mock_client(), "gpt-4o"),
    ):
        agent1._try_activate_fallback(FailoverReason.timeout)

    agent1._buffer_status("⏳ Fallback attempt 1 failed")
    agent1._buffer_vprint("Gateway timeout 504")

    assert len(agent1._retry_status_buffer) == 6
    assert agent1._pending_fallback_notice is not None

    # Terminal failure triggers _flush_status_buffer()
    agent1._flush_status_buffer()

    expected_emitted = [
        {"channel": "status", "text": "⏳ Primary attempt 1 failed"},
        {
            "channel": "vprint",
            "text": "[hermes] Connection refused by peer",
            "force": True,
        },
        {
            "channel": "status",
            "text": "⚠️ Provider unreachable \u2014 switching to fallback provider...",
        },
        {
            "channel": "status",
            "text": (
                "⚠️ Model fallback: claude-3-5-sonnet via anthropic unavailable "
                "(request timeout); using gpt-4o via openai."
            ),
        },
        {"channel": "status", "text": "⏳ Fallback attempt 1 failed"},
        {"channel": "vprint", "text": "[hermes] Gateway timeout 504", "force": True},
    ]

    assert emitted1 == expected_emitted
    assert agent1._retry_status_buffer == []
    assert agent1._pending_fallback_notice is None

    # Subsequent emit_pending_fallback_notice emits nothing
    post_emitted: List[Any] = []
    agent1._emit_status = lambda msg: post_emitted.append(msg)
    agent1._emit_pending_fallback_notice()
    assert post_emitted == []

    rows.append({
        "case_name": "terminal_failure_with_fallback_switch",
        "category": "terminal_failure",
        "description": "On terminal failure, full trace is replayed in FIFO order and pending notice is discarded",
        "operations": [
            "_buffer_status(primary_err)",
            "_buffer_vprint(conn_refused)",
            "_buffer_status(pre_switch)",
            "try_activate_fallback(timeout)",
            "_buffer_status(fallback_err)",
            "_buffer_vprint(timeout_504)",
            "_flush_status_buffer()",
        ],
        "emitted_events": emitted1,
        "final_state": {
            "retry_status_buffer": [],
            "pending_fallback_notice": None,
        },
        "invariants": {
            "exact_fifo_order": True,
            "vprint_prepends_log_prefix": True,
            "vprint_force_flag_true": True,
            "pending_notice_cleared_by_flush": True,
            "buffer_drained": True,
        },
    })

    # Case 2: Terminal failure with primary retries only (no fallback configured)
    agent2 = _make_bare_agent_double()
    emitted2: List[Dict[str, Any]] = []
    agent2._emit_status = lambda msg: emitted2.append({
        "channel": "status",
        "text": msg,
    })
    agent2._vprint = lambda msg, force=False, **kw: emitted2.append({
        "channel": "vprint",
        "text": msg,
        "force": force,
    })

    agent2._buffer_status("⏳ Attempt 1 failed")
    agent2._buffer_status("⏳ Attempt 2 failed")
    agent2._buffer_status("⏳ Attempt 3 failed")

    agent2._flush_status_buffer()

    assert len(emitted2) == 3
    assert agent2._retry_status_buffer == []
    rows.append({
        "case_name": "terminal_failure_primary_only_no_fallback",
        "category": "terminal_failure",
        "description": "When no fallback is configured, exhausted retries are flushed to operator in FIFO order",
        "emitted_events": emitted2,
        "final_state": {
            "retry_status_buffer": [],
            "pending_fallback_notice": None,
        },
        "invariants": {
            "count_matches_retries": len(emitted2) == 3,
            "buffer_drained": True,
        },
    })

    # Case 3: Flush discards pending notice to prevent cross-turn leak
    agent3 = _make_bare_agent_double()
    agent3._pending_fallback_notice = "Pending switch"
    agent3._buffer_status("failed attempt")
    agent3._flush_status_buffer()
    assert agent3._pending_fallback_notice is None

    # Next turn succeeds without fallback
    emitted3: List[Any] = []
    agent3._emit_status = lambda msg: emitted3.append(msg)
    agent3._emit_pending_fallback_notice()
    assert emitted3 == []

    rows.append({
        "case_name": "flush_discards_pending_notice_preventing_leak",
        "category": "terminal_failure",
        "description": "Flushing discards pending notice so it cannot leak into a subsequent successful turn",
        "invariants": {
            "pending_notice_discarded": True,
            "subsequent_turn_emits_nothing": len(emitted3) == 0,
        },
    })

    return rows


def generate_multiple_switches() -> List[Dict[str, Any]]:
    """Test multiple sequential fallback switches in a chain."""
    rows: List[Dict[str, Any]] = []

    # Case 1: Two switches recovering on second fallback
    agent1 = _make_bare_agent_double(
        model="primary-m",
        provider="primary-p",
        fallback_chain=[
            {"model": "fb1-m", "provider": "fb1-p"},
            {"model": "fb2-m", "provider": "fb2-p"},
        ],
    )
    emitted1: List[Dict[str, Any]] = []
    agent1._emit_status = lambda msg: emitted1.append({
        "channel": "status",
        "text": msg,
    })

    # Switch 1: Primary fails on 429
    agent1._buffer_status("⏳ Retrying primary...")
    agent1._buffer_status("⚠️ Rate limited \u2014 switching to fallback provider...")
    mock_c1 = _build_mock_client("https://fb1.example.com/v1")
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(mock_c1, "fb1-m"),
    ):
        agent1._try_activate_fallback(FailoverReason.rate_limit)

    # Switch 2: FB1 fails on 401
    agent1._buffer_status("⏳ Retrying fb1...")
    agent1._buffer_status(
        "🔐 Authentication failed and could not be refreshed \u2014 switching to fallback provider..."
    )
    mock_c2 = _build_mock_client("https://fb2.example.com/v1")
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(mock_c2, "fb2-m"),
    ):
        agent1._try_activate_fallback(FailoverReason.auth)

    expected_notice_1 = "⚠️ Model fallback: primary-m via primary-p unavailable (rate limit); using fb1-m via fb1-p."
    expected_notice_2 = "⚠️ Model fallback: fb1-m via fb1-p unavailable (authentication failed); using fb2-m via fb2-p."
    assert agent1._pending_fallback_notice == [expected_notice_1, expected_notice_2]

    # Recovers on FB2!
    agent1._emit_pending_fallback_notice()
    agent1._clear_status_buffer()

    assert emitted1 == [
        {"channel": "status", "text": expected_notice_1},
        {"channel": "status", "text": expected_notice_2},
    ]
    assert agent1._retry_status_buffer == []
    assert agent1._pending_fallback_notice is None

    rows.append({
        "case_name": "two_switches_recovered_on_second_fallback",
        "category": "multiple_switches",
        "description": "Both fallback switch notices are preserved and emitted in sequential order on recovery",
        "pending_notices_before_emit": [expected_notice_1, expected_notice_2],
        "emitted_events": emitted1,
        "final_state": {
            "retry_status_buffer": [],
            "pending_fallback_notice": None,
        },
        "invariants": {
            "all_switches_emitted_in_order": True,
            "retry_chatter_dropped": True,
            "buffer_drained": True,
            "pending_cleared": True,
        },
    })

    # Case 2: Two switches failing terminally
    agent2 = _make_bare_agent_double(
        model="primary-m",
        provider="primary-p",
        fallback_chain=[
            {"model": "fb1-m", "provider": "fb1-p"},
            {"model": "fb2-m", "provider": "fb2-p"},
        ],
    )
    emitted2: List[Dict[str, Any]] = []
    agent2._emit_status = lambda msg: emitted2.append({
        "channel": "status",
        "text": msg,
    })
    agent2._vprint = lambda msg, force=False, **kw: emitted2.append({
        "channel": "vprint",
        "text": msg,
        "force": force,
    })

    agent2._buffer_status("⏳ Primary 429")
    agent2._buffer_status("⚠️ Rate limited \u2014 switching to fallback provider...")
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(mock_c1, "fb1-m"),
    ):
        agent2._try_activate_fallback(FailoverReason.rate_limit)

    agent2._buffer_status("⏳ FB1 401")
    agent2._buffer_status(
        "🔐 Authentication failed and could not be refreshed \u2014 switching to fallback provider..."
    )
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(mock_c2, "fb2-m"),
    ):
        agent2._try_activate_fallback(FailoverReason.auth)

    agent2._buffer_status("⏳ FB2 500 server error")

    # Terminal flush
    agent2._flush_status_buffer()

    expected_terminal_events = [
        {"channel": "status", "text": "⏳ Primary 429"},
        {
            "channel": "status",
            "text": "⚠️ Rate limited \u2014 switching to fallback provider...",
        },
        {"channel": "status", "text": expected_notice_1},
        {"channel": "status", "text": "⏳ FB1 401"},
        {
            "channel": "status",
            "text": "🔐 Authentication failed and could not be refreshed \u2014 switching to fallback provider...",
        },
        {"channel": "status", "text": expected_notice_2},
        {"channel": "status", "text": "⏳ FB2 500 server error"},
    ]
    assert emitted2 == expected_terminal_events
    assert agent2._retry_status_buffer == []
    assert agent2._pending_fallback_notice is None

    rows.append({
        "case_name": "two_switches_terminal_failure_exhausted",
        "category": "multiple_switches",
        "description": "Terminal failure outputs all interleaved retries and switch notices in exact FIFO order",
        "emitted_events": emitted2,
        "final_state": {
            "retry_status_buffer": [],
            "pending_fallback_notice": None,
        },
        "invariants": {
            "exact_interleaved_fifo_order": True,
            "buffer_drained": True,
            "pending_cleared": True,
        },
    })

    # Case 3: Three switches recovering on third fallback
    agent3 = _make_bare_agent_double(
        model="m0",
        provider="p0",
        fallback_chain=[
            {"model": "m1", "provider": "p1"},
            {"model": "m2", "provider": "p2"},
            {"model": "m3", "provider": "p3"},
        ],
    )
    emitted3: List[Dict[str, Any]] = []
    agent3._emit_status = lambda msg: emitted3.append({
        "channel": "status",
        "text": msg,
    })

    mock_client = _build_mock_client()
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(mock_client, "m1"),
    ):
        agent3._try_activate_fallback(FailoverReason.rate_limit)
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(mock_client, "m2"),
    ):
        agent3._try_activate_fallback(FailoverReason.timeout)
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(mock_client, "m3"),
    ):
        agent3._try_activate_fallback(FailoverReason.server_error)

    assert len(agent3._pending_fallback_notice) == 3
    agent3._emit_pending_fallback_notice()
    agent3._clear_status_buffer()

    assert len(emitted3) == 3
    assert "m0 via p0 unavailable (rate limit); using m1 via p1" in emitted3[0]["text"]
    assert (
        "m1 via p1 unavailable (request timeout); using m2 via p2"
        in emitted3[1]["text"]
    )
    assert (
        "m2 via p2 unavailable (provider server error); using m3 via p3"
        in emitted3[2]["text"]
    )

    rows.append({
        "case_name": "three_switches_recovered_on_third_fallback",
        "category": "multiple_switches",
        "description": "Three sequential fallback notices emit in order 1 -> 2 -> 3 upon successful recovery",
        "emitted_events": emitted3,
        "invariants": {
            "notice_count": 3,
            "buffer_drained": True,
            "pending_cleared": True,
        },
    })

    return rows


def generate_callback_failure_resilience() -> List[Dict[str, Any]]:
    """Test robustness against surface exceptions raised by emit callbacks."""
    rows: List[Dict[str, Any]] = []

    # Case 1: Callback error in _emit_pending_fallback_notice continues across notices
    agent1 = _make_bare_agent_double()
    notices1 = ["first-notice", "second-notice", "third-notice"]
    agent1._pending_fallback_notice = list(notices1)

    attempted1: List[str] = []
    succeeded1: List[str] = []

    def failing_emit_first(msg: str) -> None:
        attempted1.append(msg)
        if msg == "first-notice":
            raise RuntimeError("simulated surface socket failure")
        succeeded1.append(msg)

    agent1._emit_status = failing_emit_first
    agent1._emit_pending_fallback_notice()

    assert attempted1 == notices1
    assert succeeded1 == ["second-notice", "third-notice"]
    # Notice cleared before dispatch so no stale re-emit later
    assert agent1._pending_fallback_notice is None

    rows.append({
        "case_name": "emit_pending_notice_continues_after_first_callback_error",
        "category": "callback_failure_resilience",
        "description": "Exception on first notice does not stop remaining notices from being emitted",
        "attempted_notices": attempted1,
        "succeeded_notices": succeeded1,
        "final_state": {
            "pending_fallback_notice": None,
        },
        "invariants": {
            "attempted_all_items": True,
            "pending_notice_cleared_despite_error": True,
        },
    })

    # Case 2: Callback error on intermediate notice
    agent2 = _make_bare_agent_double()
    notices2 = ["item-1", "item-2", "item-3"]
    agent2._pending_fallback_notice = list(notices2)

    attempted2: List[str] = []
    succeeded2: List[str] = []

    def failing_emit_middle(msg: str) -> None:
        attempted2.append(msg)
        if msg == "item-2":
            raise ValueError("simulated broken pipe")
        succeeded2.append(msg)

    agent2._emit_status = failing_emit_middle
    agent2._emit_pending_fallback_notice()

    assert attempted2 == notices2
    assert succeeded2 == ["item-1", "item-3"]
    assert agent2._pending_fallback_notice is None

    rows.append({
        "case_name": "emit_pending_notice_continues_after_middle_callback_error",
        "category": "callback_failure_resilience",
        "description": "Exception on middle notice is swallowed and remaining notices dispatch",
        "attempted_notices": attempted2,
        "succeeded_notices": succeeded2,
        "invariants": {
            "pending_notice_cleared": True,
        },
    })

    # Case 3: Callback error during _flush_status_buffer drains buffer first
    agent3 = _make_bare_agent_double()
    agent3.log_prefix = ""
    agent3._retry_status_buffer = [
        ("status", "status-1"),
        ("vprint", "vprint-1"),
        ("status", "status-2"),
    ]

    attempted3: List[str] = []

    def failing_emit_status(msg: str) -> None:
        attempted3.append(msg)
        if msg == "status-1":
            raise RuntimeError("IPC channel crashed")

    agent3._emit_status = failing_emit_status
    agent3._vprint = lambda msg, force=False, **kw: attempted3.append(msg)

    agent3._flush_status_buffer()

    assert attempted3 == ["status-1", "vprint-1", "status-2"]
    assert agent3._retry_status_buffer == []

    rows.append({
        "case_name": "flush_status_buffer_swallows_callback_exceptions_and_drains",
        "category": "callback_failure_resilience",
        "description": "_flush_status_buffer drains buffer first and continues dispatching remaining messages if one fails",
        "attempted_records": attempted3,
        "final_state": {
            "retry_status_buffer": [],
        },
        "invariants": {
            "buffer_drained_completely": True,
            "all_items_attempted": True,
        },
    })

    # Case 4: _vprint callback throws during flush
    agent4 = _make_bare_agent_double()
    agent4.log_prefix = ""
    agent4._retry_status_buffer = [
        ("vprint", "vprint-error"),
        ("status", "status-ok"),
    ]
    seen4: List[str] = []

    def boom_vprint(msg: str, force: bool = False) -> None:
        seen4.append(msg)
        raise IOError("terminal write error")

    agent4._vprint = boom_vprint
    agent4._emit_status = lambda msg: seen4.append(msg)

    agent4._flush_status_buffer()
    assert seen4 == ["vprint-error", "status-ok"]
    assert agent4._retry_status_buffer == []

    rows.append({
        "case_name": "flush_vprint_callback_exception_swallowed",
        "category": "callback_failure_resilience",
        "description": "_flush_status_buffer swallows exceptions from _vprint and continues to next item",
        "attempted_records": seen4,
        "invariants": {
            "buffer_drained": True,
        },
    })

    return rows


def generate_clear_flush_idempotence() -> List[Dict[str, Any]]:
    """Test idempotence, uninitialized attributes, and repeated calls."""
    rows: List[Dict[str, Any]] = []

    # Case 1: Uninitialized agent double (missing buffer and pending notice attributes)
    agent1 = AIAgent.__new__(AIAgent)
    # Must not raise when called on bare instance
    agent1._clear_status_buffer()
    agent1._flush_status_buffer()
    agent1._emit_pending_fallback_notice()

    rows.append({
        "case_name": "uninitialized_agent_attributes_safe",
        "category": "clear_flush_idempotence",
        "description": "Calling _clear_status_buffer, _flush_status_buffer, or _emit_pending_fallback_notice on bare double does not raise",
        "invariants": {
            "no_exceptions_raised": True,
        },
    })

    # Case 2: Repeated _clear_status_buffer calls are idempotent
    agent2 = _make_bare_agent_double()
    agent2._buffer_status("transient chatter")
    agent2._clear_status_buffer()
    assert agent2._retry_status_buffer == []
    agent2._clear_status_buffer()
    agent2._clear_status_buffer()
    assert agent2._retry_status_buffer == []

    rows.append({
        "case_name": "repeated_clear_status_buffer_idempotent",
        "category": "clear_flush_idempotence",
        "description": "Repeated calls to _clear_status_buffer remain quiet no-ops",
        "invariants": {
            "buffer_empty": True,
        },
    })

    # Case 3: Repeated _flush_status_buffer calls (first emits, later no-op)
    agent3 = _make_bare_agent_double()
    emitted3: List[str] = []
    agent3._emit_status = emitted3.append
    agent3._buffer_status("single message")

    agent3._flush_status_buffer()
    assert emitted3 == ["single message"]
    assert agent3._retry_status_buffer == []

    agent3._flush_status_buffer()
    agent3._flush_status_buffer()
    assert emitted3 == ["single message"]

    rows.append({
        "case_name": "repeated_flush_status_buffer_idempotent",
        "category": "clear_flush_idempotence",
        "description": "First flush drains buffer; second and third flushes emit zero messages",
        "emissions_after_first": 1,
        "emissions_after_third": 1,
        "invariants": {
            "zero_duplicate_emissions": True,
        },
    })

    # Case 4: Repeated _emit_pending_fallback_notice calls (first emits, later no-op)
    agent4 = _make_bare_agent_double()
    emitted4: List[str] = []
    agent4._emit_status = emitted4.append
    agent4._pending_fallback_notice = ["notice 1"]

    agent4._emit_pending_fallback_notice()
    assert emitted4 == ["notice 1"]
    assert agent4._pending_fallback_notice is None

    agent4._emit_pending_fallback_notice()
    agent4._emit_pending_fallback_notice()
    assert emitted4 == ["notice 1"]

    rows.append({
        "case_name": "repeated_emit_pending_fallback_notice_idempotent",
        "category": "clear_flush_idempotence",
        "description": "First call emits notice; subsequent calls find pending notice None and emit zero",
        "emissions_after_first": 1,
        "emissions_after_third": 1,
        "invariants": {
            "notice_cleared_to_none": True,
        },
    })

    # Case 5: _clear_status_buffer followed by _flush_status_buffer
    agent5 = _make_bare_agent_double()
    emitted5: List[str] = []
    agent5._emit_status = emitted5.append
    agent5._buffer_status("will be cleared")

    agent5._clear_status_buffer()
    agent5._flush_status_buffer()
    assert emitted5 == []

    rows.append({
        "case_name": "clear_then_flush_produces_zero_emissions",
        "category": "clear_flush_idempotence",
        "description": "Clearing the buffer beforehand guarantees subsequent flush produces zero emissions",
        "emitted_events": emitted5,
        "invariants": {
            "zero_emissions": True,
        },
    })

    # Case 6: Single string pending notice coercion and cleanup
    agent6 = _make_bare_agent_double()
    emitted6: List[str] = []
    agent6._emit_status = emitted6.append
    agent6._pending_fallback_notice = "legacy single string notice"

    agent6._emit_pending_fallback_notice()
    assert emitted6 == ["legacy single string notice"]
    assert agent6._pending_fallback_notice is None

    rows.append({
        "case_name": "pending_notice_single_string_coercion",
        "category": "clear_flush_idempotence",
        "description": "_emit_pending_fallback_notice correctly coerces a single str notice to a list and clears it",
        "emitted_events": emitted6,
        "invariants": {
            "coerced_and_emitted": True,
            "cleared_to_none": True,
        },
    })

    return rows


def generate_representative_fallback_reasons() -> List[Dict[str, Any]]:
    """Test live source generation of fallback switch notices across representative failover reasons.

    Executes try_activate_fallback on AIAgent.__new__ test doubles with mock clients.
    """
    rows: List[Dict[str, Any]] = []

    # Matrix of representative reasons and pre-switch operator notices from conversation_loop.py
    specs = [
        # Auth reasons
        {
            "case_name": "auth_failure_standard",
            "reason": FailoverReason.auth,
            "category": "auth",
            "pre_switch_notice": (
                "🔐 Authentication failed and could not be refreshed \u2014 "
                "switching to fallback provider..."
            ),
        },
        {
            "case_name": "auth_failure_permanent",
            "reason": FailoverReason.auth_permanent,
            "category": "auth",
            "pre_switch_notice": (
                "🔐 Authentication failed and could not be refreshed \u2014 "
                "switching to fallback provider..."
            ),
        },
        # Rate reasons
        {
            "case_name": "rate_limit_standard",
            "reason": FailoverReason.rate_limit,
            "category": "rate",
            "pre_switch_notice": "⚠️ Rate limited \u2014 switching to fallback provider...",
        },
        {
            "case_name": "rate_limit_upstream_aggregator",
            "reason": FailoverReason.upstream_rate_limit,
            "category": "rate",
            "pre_switch_notice": "⚠️ Upstream aggregator rate-limited \u2014 switching to fallback model...",
        },
        {
            "case_name": "billing_quota_exhausted_verified",
            "reason": FailoverReason.billing,
            "category": "rate",
            "pre_switch_notice": "⚠️ Billing or credits exhausted \u2014 switching to fallback provider...",
        },
        {
            "case_name": "billing_quota_exhausted_unverified",
            "reason": FailoverReason.billing,
            "category": "rate",
            "pre_switch_notice": (
                "⚠️ Provider reported usage/credit exhaustion "
                "(unverified \u2014 may be a content-filter rejection) "
                "\u2014 switching to fallback provider..."
            ),
        },
        {
            "case_name": "provider_overloaded",
            "reason": FailoverReason.overloaded,
            "category": "rate",
            "pre_switch_notice": "⚠️ Rate limited \u2014 switching to fallback provider...",
        },
        # Transport reasons
        {
            "case_name": "transport_timeout",
            "reason": FailoverReason.timeout,
            "category": "transport",
            "pre_switch_notice": "⚠️ Provider unreachable \u2014 switching to fallback provider...",
        },
        {
            "case_name": "transport_ssl_cert_verification",
            "reason": FailoverReason.ssl_cert_verification,
            "category": "transport",
            "pre_switch_notice": "⚠️ Provider unreachable \u2014 switching to fallback provider...",
        },
        {
            "case_name": "transport_server_error",
            "reason": FailoverReason.server_error,
            "category": "transport",
            "pre_switch_notice": "⚠️ Provider unreachable \u2014 switching to fallback provider...",
        },
        # Policy & Content reasons
        {
            "case_name": "policy_content_blocked",
            "reason": FailoverReason.content_policy_blocked,
            "category": "policy",
            "pre_switch_notice": None,
        },
        {
            "case_name": "context_window_overflow",
            "reason": FailoverReason.context_overflow,
            "category": "policy",
            "pre_switch_notice": None,
        },
        {
            "case_name": "model_not_found",
            "reason": FailoverReason.model_not_found,
            "category": "policy",
            "pre_switch_notice": None,
        },
        # Default / unknown reason
        {
            "case_name": "unknown_provider_failure",
            "reason": None,
            "category": "default",
            "pre_switch_notice": None,
        },
    ]

    mock_client = _build_mock_client("https://api.openai.com/v1")

    for spec in specs:
        case_name = spec["case_name"]
        reason = spec["reason"]
        category = spec["category"]
        pre_switch = spec["pre_switch_notice"]

        # Run live reason text producer
        live_reason_label = _fallback_reason_text(reason)

        # Run live fallback activator
        agent = _make_bare_agent_double(
            model="primary-model",
            provider="primary-provider",
            fallback_chain=[{"model": "fb-model", "provider": "fb-provider"}],
        )

        with patch(
            "agent.auxiliary_client.resolve_provider_client",
            return_value=(mock_client, "fb-model"),
        ):
            ok = agent._try_activate_fallback(reason)

        assert ok is True
        assert len(agent._pending_fallback_notice) == 1
        live_switch_notice = agent._pending_fallback_notice[0]

        expected_switch_notice = (
            f"⚠️ Model fallback: primary-model via primary-provider unavailable "
            f"({live_reason_label}); using fb-model via fb-provider."
        )
        assert live_switch_notice == expected_switch_notice
        assert agent._retry_status_buffer == [("status", live_switch_notice)]

        rows.append({
            "case_name": case_name,
            "category": category,
            "reason_enum": reason.value if reason is not None else None,
            "live_reason_label": live_reason_label,
            "live_switch_notice": live_switch_notice,
            "conversation_loop_pre_switch_notice": pre_switch,
            "buffered_tuple": ["status", live_switch_notice],
            "pending_notice_stored": [live_switch_notice],
            "verified_live_source": True,
        })

    return rows


def build_corpus() -> Dict[str, Any]:
    """Execute all oracle tests and package the deterministic golden corpus."""
    primitives = generate_buffering_primitives()
    recovered = generate_recovered_success()
    terminal = generate_terminal_failure()
    multiple = generate_multiple_switches()
    resilience = generate_callback_failure_resilience()
    idempotence = generate_clear_flush_idempotence()
    reasons = generate_representative_fallback_reasons()

    total_cases = (
        len(primitives)
        + len(recovered)
        + len(terminal)
        + len(multiple)
        + len(resilience)
        + len(idempotence)
        + len(reasons)
    )

    corpus = {
        "contract_metadata": {
            "target": "rust/tools/main-provider-operator-notice-goldens.json",
            "lane": "Python Source-Executed Main Provider Retry & Fallback Notice Buffering Contract",
            "generator": "rust/tools/gen_main_provider_operator_notice_goldens.py",
            "total_cases": total_cases,
            "sections": [
                "buffering_primitives",
                "recovered_success",
                "terminal_failure",
                "multiple_switches",
                "callback_failure_resilience",
                "clear_flush_idempotence",
                "representative_fallback_reasons",
            ],
        },
        "buffering_primitives": primitives,
        "recovered_success": recovered,
        "terminal_failure": terminal,
        "multiple_switches": multiple,
        "callback_failure_resilience": resilience,
        "clear_flush_idempotence": idempotence,
        "representative_fallback_reasons": reasons,
    }

    # Assert every expected field before serialization
    for section_name in [
        "buffering_primitives",
        "recovered_success",
        "terminal_failure",
        "multiple_switches",
        "callback_failure_resilience",
        "clear_flush_idempotence",
        "representative_fallback_reasons",
    ]:
        for case in corpus[section_name]:
            assert "case_name" in case, f"Missing case_name in {section_name}"
            assert "category" in case, f"Missing category in {case['case_name']}"
            assert isinstance(case["case_name"], str)

    return corpus


def main() -> None:
    t0 = time.monotonic()
    raw_corpus = build_corpus()
    t1 = time.monotonic()

    # Verify execution took under 1 second
    elapsed = t1 - t0
    assert elapsed < 1.0, f"Corpus generation exceeded 1.0s budget: {elapsed:.3f}s"

    # Sanitize and escape all unicode em dashes
    clean_corpus = _escape_em_dash(raw_corpus)
    rendered_json = json.dumps(clean_corpus, indent=2, sort_keys=True) + "\n"

    # Strict assertion: no literal em dash characters in rendered json!
    assert "\u2014" not in rendered_json, (
        "Literal unicode em dash character (\\u2014) detected in output JSON!"
    )

    if "--check" in sys.argv:
        if not OUT.exists():
            sys.stderr.write(
                f"Error: {OUT} does not exist. Run without --check to generate it.\n"
            )
            sys.exit(1)
        existing = OUT.read_text(encoding="utf-8")
        if existing != rendered_json:
            sys.stderr.write(
                f"Error: {OUT} is out of sync with generator output. Run without --check to regenerate.\n"
            )
            sys.exit(1)
        print(
            f"Parity check passed: {OUT} matches oracle output ({clean_corpus['contract_metadata']['total_cases']} cases, {elapsed:.4f}s)."
        )
        return

    # Write output
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(rendered_json, encoding="utf-8")
    print(
        f"Wrote {clean_corpus['contract_metadata']['total_cases']} golden cases to {OUT} ({elapsed:.4f}s)."
    )


if __name__ == "__main__":
    main()
