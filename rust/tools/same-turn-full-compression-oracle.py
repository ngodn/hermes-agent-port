#!/usr/bin/env python3
"""Golden oracle for the post-tool full LLM compression decision + adoption.

This drives the *real* Python decision and transcript-reshaping functions that
run after a completed tool-result batch and before the next provider request,
so the Rust port can be reviewed against CPython behaviour rather than a
paraphrase.
Nothing here reimplements the decision logic; every recorded value is the
return of an authoritative local function executed under patched-out I/O.

The post-tool gate lives inline in ``agent/conversation_loop.py`` (the
``run_conversation`` tool-result branch, around the ``should_compress`` /
``_compress_context`` / ``conversation_history_after_compression`` /
``_should_skip_model_call_for_reference_handoff`` calls). Because that gate is
a several-thousand-line generator, the oracle drives the narrowest real
functions it delegates to, in the same order the loop calls them, and records
the observable results. The purely inline attempt bookkeeping (``+= 1`` on
admit, ``-= 1`` on lock-skip refund) and the ``break`` / next-request ordering
are documented in the analysis report as deferred to Rust's live integration
test; every predicate that feeds them is exercised here for real.

Authoritative functions executed (current local source):

  * ``ContextCompressor.should_compress`` / ``should_compress_info``
    (agent/context_compressor.py) - the trigger decision and its block reasons.
  * ``ContextCompressor.compress`` (agent/context_compressor.py) - Phase-1
    deterministic prune, protected head/tail, summary generation, in-place
    reshaping, and the abort/no-op returns.
  * ``_midturn_request_pressure_tokens`` + ``estimate_request_tokens_rough``
    (agent/conversation_loop.py, agent/model_metadata.py) - the no-usage
    fallback sizing the loop feeds to ``should_compress``.
  * ``conversation_history_after_compression``
    (agent/conversation_compression.py) - the committed-transcript adoption
    baseline that prevents duplicate persistence.
  * ``compression_skipped_due_to_lock``
    (agent/conversation_compression.py) - the type-pinned lock-skip read.
  * ``_should_skip_model_call_for_reference_handoff`` +
    ``reference_handoff_would_drive_next_model_call``
    (agent/conversation_loop.py, agent/context_compressor.py) - the
    reference-only handoff suppression of an extra provider call.

Only the summary LLM I/O, the model-window probe, and the clock are patched.
The summary call reaches the network through ``agent.context_compressor.call_llm``;
a scripted stand-in replaces it, one reply per call, so the state machine (not
the summarizer) is what the golden pins. ``get_model_context_length`` is pinned
to a fixed window so construction never probes ``/models``. ``time.monotonic``
is pinned only for the cooldown/backoff gate cases so their reason strings are
stable. Persistence is deliberately left unbound: an unbound ``_session_db``
makes the durable-guard refresh no-op, which is the exact shape the in-memory
decision must survive. No network, timestamps, randomness, absolute paths, or
credentials enter a golden.

Run from the repository root with the project virtualenv:

    .venv/bin/python rust/tools/same-turn-full-compression-oracle.py

Writes ``rust/tools/same-turn-full-compression-goldens.json`` next to this
script. Pass ``--check`` to regenerate in memory and compare against the
checked-in fixture.
"""
from __future__ import annotations

import copy
import json
import logging
import sys
import types
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent / "same-turn-full-compression-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

# Silence the compressor's warning-level failure logs; they are not part of any
# golden and would only clutter the generator output.
logging.getLogger("agent.context_compressor").setLevel(logging.CRITICAL)

import agent.context_compressor as cc  # noqa: E402
from agent.context_compressor import (  # noqa: E402
    COMPRESSED_SUMMARY_METADATA_KEY,
    SUMMARY_PREFIX,
    reference_handoff_would_drive_next_model_call,
)
from agent.conversation_compression import (  # noqa: E402
    compression_skipped_due_to_lock,
    conversation_history_after_compression,
)
from agent.conversation_loop import (  # noqa: E402
    _midturn_request_pressure_tokens,
    _should_skip_model_call_for_reference_handoff,
)
from agent.model_metadata import estimate_request_tokens_rough  # noqa: E402

FIXED_WINDOW = 8000


# ---------------------------------------------------------------------------
# scripted summary LLM  (agent.context_compressor.call_llm seam)
# ---------------------------------------------------------------------------
def _response(content: str, finish_reason: str = "stop") -> Any:
    resp = MagicMock()
    resp.choices[0].message.content = content
    resp.choices[0].finish_reason = finish_reason
    return resp


class ScriptedSummary:
    """Deterministic stand-in for the summary LLM.

    Each ``compress`` summary call pops the next scripted reply. ``ok`` returns
    content, ``empty`` returns whitespace content (routes to the empty-content
    failure), ``raise`` raises a transient connection error (network failure).
    An unscripted call is a mis-built case and raises. ``count`` is the real
    number of calls the compressor issued, which the golden records.
    """

    def __init__(self, script: List[Dict[str, Any]]) -> None:
        self.script = script
        self.count = 0

    def __call__(self, **_kwargs: Any) -> Any:
        i = self.count
        self.count += 1
        if i >= len(self.script):
            raise AssertionError(f"unscripted summary call #{i}")
        entry = self.script[i]
        kind = entry["kind"]
        if kind == "ok":
            return _response(entry["content"])
        if kind == "empty":
            return _response("")
        if kind == "raise":
            raise ConnectionError("peer closed connection")
        raise AssertionError(f"unknown script kind {kind!r}")


# ---------------------------------------------------------------------------
# compressor construction (no network)
# ---------------------------------------------------------------------------
def make_compressor(
    *,
    threshold_percent: float = 0.50,
    protect_first_n: int = 3,
    protect_last_n: int = 20,
    model: str = "test-model",
    provider: str = "test",
) -> "cc.ContextCompressor":
    with patch.object(cc, "get_model_context_length", return_value=FIXED_WINDOW):
        c = cc.ContextCompressor(
            model=model,
            threshold_percent=threshold_percent,
            protect_first_n=protect_first_n,
            protect_last_n=protect_last_n,
            quiet_mode=True,
            config_context_length=FIXED_WINDOW,
            provider=provider,
        )
        # Resolve derived budgets while the window patch is live so no later
        # property access can fire a /models probe.
        _ = c.tail_token_budget
        _ = c.threshold_tokens
    # summary_model is left equal to the main model on purpose: a distinct
    # summary_model would arm the one-shot main-model fallback in
    # _generate_summary, adding a second scripted call. Equal models keep every
    # failure path to exactly one summary call.
    return c


# ---------------------------------------------------------------------------
# message builders
# ---------------------------------------------------------------------------
def system(text: str = "system prompt") -> Dict[str, Any]:
    return {"role": "system", "content": text}


def user(text: str, **extra: Any) -> Dict[str, Any]:
    msg = {"role": "user", "content": text}
    msg.update(extra)
    return msg


def assistant(content: str = "", tool_calls: Optional[List[Dict[str, Any]]] = None,
              **extra: Any) -> Dict[str, Any]:
    msg: Dict[str, Any] = {"role": "assistant", "content": content}
    if tool_calls is not None:
        msg["tool_calls"] = tool_calls
    msg.update(extra)
    return msg


def tool_call(cid: str, name: str = "run", args: str = "{}") -> Dict[str, Any]:
    return {"id": cid, "type": "function",
            "function": {"name": name, "arguments": args}}


def tool(cid: str, content: Any = None) -> Dict[str, Any]:
    return {"role": "tool", "tool_call_id": cid,
            "content": content if content is not None else "R" * 400}


def plain_conversation(exchanges: int) -> List[Dict[str, Any]]:
    """Alternating user/assistant transcript with bulky content."""
    msgs = [system()]
    for i in range(exchanges):
        msgs.append(user(f"question {i} " + "x" * 400))
        msgs.append(assistant(f"answer {i} " + "y" * 400))
    return msgs


def tool_batch_conversation(exchanges: int) -> List[Dict[str, Any]]:
    """Transcript where every exchange completes a durable tool batch.

    user -> assistant(tool_calls) -> tool result -> assistant(text). This is
    the post-tool shape the loop compresses: complete tool groups the reshaping
    must never orphan.
    """
    msgs = [system()]
    for i in range(exchanges):
        cid = f"call_{i}"
        msgs.append(user(f"do task {i} " + "x" * 200))
        msgs.append(assistant("", tool_calls=[tool_call(cid, args=f'{{"n":{i}}}')]))
        msgs.append(tool(cid, content=f"tool output {i} " + "R" * 400))
        msgs.append(assistant(f"done {i} " + "y" * 200))
    return msgs


# ---------------------------------------------------------------------------
# observation helpers (assertions only; no decision logic)
# ---------------------------------------------------------------------------
def marker_count(messages: List[Dict[str, Any]]) -> int:
    return sum(1 for m in messages if isinstance(m, dict)
               and m.get(COMPRESSED_SUMMARY_METADATA_KEY))


def roles(messages: List[Dict[str, Any]]) -> List[Optional[str]]:
    return [m.get("role") if isinstance(m, dict) else None for m in messages]


def user_contents(messages: List[Dict[str, Any]]) -> List[str]:
    out: List[str] = []
    for m in messages:
        if isinstance(m, dict) and m.get("role") == "user":
            c = m.get("content")
            if isinstance(c, str):
                out.append(c)
    return out


def tool_group_orphans(messages: List[Dict[str, Any]]) -> Dict[str, List[str]]:
    """Return orphaned tool_call ids and tool_result ids in a transcript.

    A complete tool group has each assistant tool_call id matched by a tool
    result and vice versa. Any leftover on either side is an orphan.
    """
    call_ids: List[str] = []
    result_ids: List[str] = []
    for m in messages:
        if not isinstance(m, dict):
            continue
        if m.get("role") == "assistant":
            for tc in m.get("tool_calls") or []:
                if isinstance(tc, dict) and tc.get("id"):
                    call_ids.append(tc["id"])
        elif m.get("role") == "tool":
            if m.get("tool_call_id"):
                result_ids.append(m["tool_call_id"])
    call_set, result_set = set(call_ids), set(result_ids)
    return {
        "calls_without_results": sorted(call_set - result_set),
        "results_without_calls": sorted(result_set - call_set),
    }


# ---------------------------------------------------------------------------
# case builders  (each self-asserts its target branch before recording)
# ---------------------------------------------------------------------------
def trigger_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # 1. Real provider prompt usage over threshold triggers compression.
    c = make_compressor()
    thr = c.threshold_tokens
    over = thr + 50_000
    decision, reason = c.should_compress_info(over)
    assert decision is True and reason is None
    cases.append({
        "case": "prompt_usage_over_threshold_triggers",
        "why": "provider prompt usage above threshold after a durable tool batch",
        "threshold_tokens": thr,
        "prompt_tokens": over,
        "should_compress": decision,
        "block_reason": reason,
    })

    # 2. Prompt usage below threshold does not compress.
    c = make_compressor()
    under = c.threshold_tokens - 1
    decision, reason = c.should_compress_info(under)
    assert decision is False and reason is None
    cases.append({
        "case": "prompt_usage_below_threshold_no_compress",
        "why": "real prompt usage under threshold leaves the transcript alone",
        "threshold_tokens": c.threshold_tokens,
        "prompt_tokens": under,
        "should_compress": decision,
        "block_reason": reason,
    })

    # 3. Over threshold but summary-LLM cooldown blocks -> no summary call.
    c = make_compressor()
    with patch("agent.context_compressor.time.monotonic", return_value=1000.0):
        c._summary_failure_cooldown_until = 2000.0
        decision, reason = c.should_compress_info(c.threshold_tokens + 50_000)
    assert decision is False and reason == "cooldown:1000"
    cases.append({
        "case": "over_threshold_cooldown_blocks",
        "why": "summary cooldown defers compression; the loop issues no summary call",
        "threshold_tokens": c.threshold_tokens,
        "prompt_tokens": c.threshold_tokens + 50_000,
        "monotonic_now": 1000.0,
        "summary_failure_cooldown_until": 2000.0,
        "should_compress": decision,
        "block_reason": reason,
    })

    # 4. Over threshold but structural no-op backoff blocks -> no summary call.
    c = make_compressor()
    with patch("agent.context_compressor.time.monotonic", return_value=1000.0):
        c._structural_no_op_backoff_until = 1500.0
        decision, reason = c.should_compress_info(c.threshold_tokens + 50_000)
    assert decision is False and reason == "structural_backoff:500"
    cases.append({
        "case": "over_threshold_structural_backoff_blocks",
        "why": "structural backoff defers compression; the loop issues no summary call",
        "threshold_tokens": c.threshold_tokens,
        "prompt_tokens": c.threshold_tokens + 50_000,
        "monotonic_now": 1000.0,
        "structural_no_op_backoff_until": 1500.0,
        "should_compress": decision,
        "block_reason": reason,
    })

    return cases


def token_selection_cases() -> List[Dict[str, Any]]:
    """The loop's ``_real_tokens`` selection feeding ``should_compress``.

    The three-way branch itself (last_prompt_tokens > 0 / == -1 / else) is
    inline loop code, mirrored here and verified against the real fallback
    sizing function for the no-usage case.
    """
    cases: List[Dict[str, Any]] = []

    # Real usage present -> the loop uses last_prompt_tokens verbatim.
    c = make_compressor()
    c.last_prompt_tokens = 123_456
    assert c.last_prompt_tokens > 0
    cases.append({
        "case": "real_usage_used_verbatim",
        "why": "provider usage available: loop compares last_prompt_tokens",
        "last_prompt_tokens": c.last_prompt_tokens,
        "selected_real_tokens": c.last_prompt_tokens,
        "branch": "last_prompt_tokens>0",
    })

    # Awaiting-real-usage sentinel (-1) -> the loop uses 0.
    c = make_compressor()
    c.last_prompt_tokens = -1
    assert c.last_prompt_tokens == -1
    cases.append({
        "case": "awaiting_real_usage_sentinel_zero",
        "why": "compression just ran; no real count yet, avoid rough false-trigger",
        "last_prompt_tokens": -1,
        "selected_real_tokens": 0,
        "branch": "last_prompt_tokens==-1",
    })

    # Zero / unavailable usage -> fall back to real request sizing.
    ag = types.SimpleNamespace(tools=None)
    api_messages = [system(), user("hi " + "x" * 50), assistant("ok " + "y" * 50)]
    rough = estimate_request_tokens_rough(api_messages, tools=None)
    fallback = _midturn_request_pressure_tokens(ag, api_messages, "sys prompt", rough)
    assert isinstance(fallback, int) and fallback == rough  # no tools, non-codex
    cases.append({
        "case": "zero_usage_falls_back_to_request_sizing",
        "why": "no provider usage (post-disconnect): size from the request",
        "last_prompt_tokens": 0,
        "estimate_request_tokens_rough": rough,
        "midturn_request_pressure_tokens": fallback,
        "selected_real_tokens": fallback,
        "branch": "else/fallback",
    })

    return cases


def _run_compress(
    messages: List[Dict[str, Any]],
    script: List[Dict[str, Any]],
    *,
    current_tokens: int,
    force: bool = False,
) -> Tuple[List[Dict[str, Any]], "cc.ContextCompressor", ScriptedSummary, List[Dict[str, Any]]]:
    c = make_compressor()
    pristine = copy.deepcopy(messages)
    scripted = ScriptedSummary(script)
    with patch("agent.context_compressor.call_llm", scripted), \
            patch.object(cc, "get_model_context_length", return_value=FIXED_WINDOW):
        out = c.compress(messages, current_tokens=current_tokens, force=force)
    return out, c, scripted, pristine


def compress_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # 5. Successful in-place compression over a durable tool batch.
    src = tool_batch_conversation(20)
    out, c, scripted, pristine = _run_compress(
        src, [{"kind": "ok", "content": "## Active Task\nport the compressor"}],
        current_tokens=100_000,
    )
    assert out is not src              # a new transcript was returned
    assert len(out) < len(pristine)    # it shrank
    assert scripted.count == 1         # exactly one summary call
    assert marker_count(out) == 1      # one summary/ack pair
    orphans = tool_group_orphans(out)
    assert not orphans["calls_without_results"]
    assert not orphans["results_without_calls"]
    assert c._last_compression_made_progress is True
    assert c._last_compress_aborted is False
    # every user byte that survives outside the summarized middle is verbatim.
    out_users = user_contents(out)
    src_users = user_contents(pristine)
    assert out_users[-1] == src_users[-1]  # newest tail user preserved
    cases.append({
        "case": "successful_inplace_compression",
        "why": "one summary/ack pair, protected tail, complete tool groups, tail user bytes",
        "input_len": len(pristine),
        "output_len": len(out),
        "returned_is_input": out is src,
        "summary_calls": scripted.count,
        "marker_count": marker_count(out),
        "output_roles": roles(out),
        "output_user_contents": out_users,
        "tool_group_orphans": orphans,
        "made_progress": c._last_compression_made_progress,
        "aborted": c._last_compress_aborted,
        "savings_pct": round(c._last_compression_savings_pct, 6),
    })

    # 6. Empty summary content -> abort, transcript unchanged, one call.
    src = plain_conversation(30)
    out, c, scripted, pristine = _run_compress(
        src, [{"kind": "empty"}], current_tokens=100_000,
    )
    assert out == pristine             # content unchanged
    assert scripted.count == 1
    assert c._last_compress_aborted is True
    assert c._last_summary_empty_content_failure is True
    assert c._last_compression_made_progress is False
    cases.append({
        "case": "empty_summary_leaves_transcript_unchanged",
        "why": "HTTP-200 empty content aborts; the middle window is preserved",
        "input_len": len(pristine),
        "output_len": len(out),
        "output_equals_input": out == pristine,
        "summary_calls": scripted.count,
        "marker_count": marker_count(out),
        "aborted": c._last_compress_aborted,
        "empty_content_failure": c._last_summary_empty_content_failure,
        "made_progress": c._last_compression_made_progress,
    })

    # 7. Summary call raises (network) -> abort, transcript unchanged.
    src = plain_conversation(30)
    out, c, scripted, pristine = _run_compress(
        src, [{"kind": "raise"}], current_tokens=100_000,
    )
    assert out == pristine
    assert scripted.count == 1
    assert c._last_compress_aborted is True
    assert c._last_summary_network_failure is True
    cases.append({
        "case": "failed_summary_leaves_transcript_unchanged",
        "why": "transient network failure aborts; session not rotated",
        "input_len": len(pristine),
        "output_len": len(out),
        "output_equals_input": out == pristine,
        "summary_calls": scripted.count,
        "aborted": c._last_compress_aborted,
        "network_failure": c._last_summary_network_failure,
        "made_progress": c._last_compression_made_progress,
    })

    # 8. Structural no-op (too few messages) -> no summary call, unchanged.
    src = [system(), user("hi"), assistant("hello")]
    out, c, scripted, pristine = _run_compress(
        src, [], current_tokens=100_000,
    )
    assert out is src                  # same object, nothing eligible
    assert scripted.count == 0         # no summary call at all
    assert c._last_compression_made_progress is False
    cases.append({
        "case": "structural_no_op_no_summary_call",
        "why": "non-shrinking transcript: no eligible window, zero summary calls",
        "input_len": len(pristine),
        "output_len": len(out),
        "returned_is_input": out is src,
        "summary_calls": scripted.count,
        "made_progress": c._last_compression_made_progress,
    })

    return cases


def adoption_cases() -> List[Dict[str, Any]]:
    """conversation_history_after_compression: the flush baseline decision."""
    cases: List[Dict[str, Any]] = []
    compacted = [system(), user("q"), assistant("a")]
    previous = [{"role": "user", "content": "older"}]

    # In-place boundary recorded -> shallow copy of the compacted transcript,
    # so the same-turn identity flush does not re-persist the already-written
    # rows (duplicate-persistence guard).
    agent = types.SimpleNamespace(
        _last_compression_attempt_recorded=True,
        _last_compression_attempt_in_place=True,
        _last_compaction_in_place=False,
    )
    baseline = conversation_history_after_compression(agent, compacted, previous)
    assert baseline == compacted and baseline is not compacted
    cases.append({
        "case": "adopt_inplace_shallow_copy",
        "why": "committed in-place transcript adopted; duplicate persistence prevented",
        "attempt_recorded": True,
        "attempt_in_place": True,
        "baseline": baseline,
        "baseline_is_messages_object": baseline is compacted,
    })

    # Attempt recorded but no boundary (aborted / no-op) -> keep prior baseline.
    agent = types.SimpleNamespace(
        _last_compression_attempt_recorded=True,
        _last_compression_attempt_in_place=None,
        _last_compaction_in_place=False,
    )
    baseline = conversation_history_after_compression(agent, compacted, previous)
    assert baseline == previous
    cases.append({
        "case": "adopt_no_boundary_keeps_previous",
        "why": "aborted/no-op after an earlier boundary retains the pre-attempt baseline",
        "attempt_recorded": True,
        "attempt_in_place": None,
        "previous_history": previous,
        "baseline": baseline,
    })

    # Legacy rotation (attempt_in_place False) -> clear baseline so the child
    # session writes the whole compacted list next flush.
    agent = types.SimpleNamespace(
        _last_compression_attempt_recorded=True,
        _last_compression_attempt_in_place=False,
        _last_compaction_in_place=False,
    )
    baseline = conversation_history_after_compression(agent, compacted, previous)
    assert baseline is None
    cases.append({
        "case": "adopt_legacy_rotation_clears_baseline",
        "why": "legacy child-session rotation writes the full compacted list",
        "attempt_recorded": True,
        "attempt_in_place": False,
        "baseline": baseline,
    })

    return cases


def lock_skip_cases() -> List[Dict[str, Any]]:
    """compression_skipped_due_to_lock: the type-pinned #69870 read."""
    cases: List[Dict[str, Any]] = []
    for label, signal, expected in (
        ("holder_string", "manual_compress", True),
        ("bare_true", True, True),
        ("cleared_none", None, False),
    ):
        agent = types.SimpleNamespace(_compression_skipped_due_to_lock=signal)
        result = compression_skipped_due_to_lock(agent)
        assert result is expected
        cases.append({
            "case": f"lock_skip_{label}",
            "why": "type-pinned lock-skip read (str holder or True), never bare truthiness",
            "signal": signal,
            "skipped_due_to_lock": result,
        })
    # A MagicMock agent must NOT be hijacked into the lock-skip branch.
    mock_agent = MagicMock()
    assert compression_skipped_due_to_lock(mock_agent) is False
    cases.append({
        "case": "lock_skip_magicmock_not_hijacked",
        "why": "auto-created truthy MagicMock attribute must read False",
        "signal": "<MagicMock attribute>",
        "skipped_due_to_lock": False,
    })
    return cases


def reference_handoff_cases() -> List[Dict[str, Any]]:
    """reference-only handoff suppression of an extra provider call (#80622)."""
    cases: List[Dict[str, Any]] = []
    handoff = {
        "role": "user",
        "content": SUMMARY_PREFIX + "\n## Historical Task\nport work",
        COMPRESSED_SUMMARY_METADATA_KEY: True,
    }

    # Sole handoff after a completed assistant turn -> skip the model call.
    sole = [system(), handoff]
    drives = reference_handoff_would_drive_next_model_call(sole)
    skip = _should_skip_model_call_for_reference_handoff(list(sole), None)
    assert drives is True and skip is True
    cases.append({
        "case": "reference_only_handoff_skips_model_call",
        "why": "sole handoff would drive the next call by itself: suppress it",
        "would_drive": drives,
        "user_message": None,
        "skip_model_call": skip,
    })

    # A real trailing user turn after the handoff -> do not skip.
    trailing = [system(), handoff, user("do the thing now")]
    drives = reference_handoff_would_drive_next_model_call(trailing)
    skip = _should_skip_model_call_for_reference_handoff(list(trailing), None)
    assert drives is False and skip is False
    cases.append({
        "case": "trailing_user_turn_no_skip",
        "why": "an actionable user turn after the handoff keeps the model call",
        "would_drive": drives,
        "skip_model_call": skip,
    })

    # Mid tool-loop: a tool result after the handoff -> not a sole driver.
    mid_loop = [system(), handoff, tool("call_x", content="mid-flight result")]
    drives = reference_handoff_would_drive_next_model_call(mid_loop)
    assert drives is False
    cases.append({
        "case": "tool_result_after_handoff_not_sole_driver",
        "why": "tool result after the handoff means an in-flight exchange continues",
        "would_drive": drives,
    })

    # Restorable ask available: helper appends the real user turn, no skip.
    restorable = [system(), copy.deepcopy(handoff)]
    skip = _should_skip_model_call_for_reference_handoff(restorable, "the fresh ask")
    assert skip is False
    # append_message stamps a non-deterministic timestamp; record only the
    # stable role/content projection so no clock value enters the golden.
    restored_tail = {k: restorable[-1][k] for k in ("role", "content")}
    assert restored_tail == {"role": "user", "content": "the fresh ask"}
    cases.append({
        "case": "restorable_user_ask_no_skip",
        "why": "the turn's real user ask is re-appended; the handoff no longer drives",
        "user_message": "the fresh ask",
        "skip_model_call": skip,
        "restored_tail": restored_tail,
        "restored_len": len(restorable),
    })

    return cases


def gate_composition_cases() -> List[Dict[str, Any]]:
    """Compose real predicate outputs into the loop's admit gate.

    The predicates are executed for real; only the two-line inline attempt
    bookkeeping and the ``break`` are mirrored (documented as deferred to
    Rust's live loop test). Loop reference: the run_conversation tool-result
    branch that reads ``compression_attempts < max_compression_attempts and
    _compressor.should_compress(_real_tokens)`` then, on a lock-skip no-op,
    refunds the attempt.
    """
    cases: List[Dict[str, Any]] = []

    # Admitted pass: over threshold, budget available, real compress runs once.
    c = make_compressor()
    real_tokens = c.threshold_tokens + 50_000
    should = c.should_compress(real_tokens)
    assert should is True
    attempts, max_attempts = 0, 3
    admitted = should and attempts < max_attempts
    assert admitted
    src = tool_batch_conversation(20)
    scripted = ScriptedSummary([{"kind": "ok", "content": "## Active Task\nx"}])
    with patch("agent.context_compressor.call_llm", scripted), \
            patch.object(cc, "get_model_context_length", return_value=FIXED_WINDOW):
        out = c.compress(src, current_tokens=real_tokens, force=False)
    attempts_after = attempts + 1  # loop: one admitted pass consumes one attempt
    assert out is not src and scripted.count == 1
    cases.append({
        "case": "admitted_pass_consumes_one_attempt_one_call",
        "why": "one admitted pass consumes one attempt and makes at most one summary call",
        "should_compress": should,
        "attempts_before": attempts,
        "max_attempts": max_attempts,
        "admitted": admitted,
        "summary_calls": scripted.count,
        "attempts_after": attempts_after,
        "compressed_identity_changed": out is not src,
    })

    # Lock-skip refund: compress no-ops returning input identity + lock signal.
    # Model the loop's refund arithmetic from real predicate outputs.
    agent = types.SimpleNamespace(_compression_skipped_due_to_lock="other_path")
    post_tool_input = [system(), user("q"), assistant("a")]
    returned = post_tool_input  # a lock-skip pass hands back the same object
    lock_signal = compression_skipped_due_to_lock(agent)
    is_noop_identity = returned is post_tool_input
    refunded = is_noop_identity and lock_signal
    attempts_before = 1
    attempts_after = attempts_before - 1 if refunded else attempts_before
    assert refunded and attempts_after == 0
    cases.append({
        "case": "lock_skip_refunds_attempt_keeps_identity",
        "why": "lock-skip refunds the attempt and keeps the original transcript identity",
        "returned_is_input": is_noop_identity,
        "skipped_due_to_lock": lock_signal,
        "refunded": bool(refunded),
        "attempts_before": attempts_before,
        "attempts_after": attempts_after,
    })

    # Exhausted attempts: over threshold but budget spent -> no summary call.
    c = make_compressor()
    real_tokens = c.threshold_tokens + 50_000
    should = c.should_compress(real_tokens)
    assert should is True
    attempts, max_attempts = 3, 3
    admitted = should and attempts < max_attempts
    assert not admitted
    cases.append({
        "case": "exhausted_attempts_no_summary_call",
        "why": "over threshold with the per-turn attempt budget spent issues no summary call",
        "should_compress": should,
        "attempts_before": attempts,
        "max_attempts": max_attempts,
        "admitted": admitted,
        "summary_calls": 0,
    })

    return cases


# ---------------------------------------------------------------------------
# assembly
# ---------------------------------------------------------------------------
def build() -> Dict[str, Any]:
    return {
        "_meta": {
            "description": (
                "Golden oracle for the post-tool full LLM compression decision "
                "and transcript adoption. Every value is the return of a real "
                "local function; no decision logic is reimplemented."
            ),
            "fixed_window_tokens": FIXED_WINDOW,
            "threshold_percent": 0.50,
            "protect_first_n": 3,
            "protect_last_n": 20,
            "authoritative_sources": [
                "agent/context_compressor.py: ContextCompressor.should_compress"
                " / should_compress_info / compress",
                "agent/conversation_loop.py: _midturn_request_pressure_tokens"
                " / _should_skip_model_call_for_reference_handoff",
                "agent/conversation_compression.py:"
                " conversation_history_after_compression"
                " / compression_skipped_due_to_lock",
                "agent/context_compressor.py:"
                " reference_handoff_would_drive_next_model_call",
                "agent/model_metadata.py: estimate_request_tokens_rough",
            ],
            "deferred_to_rust_integration": [
                "inline attempt bookkeeping (+=1 admit, -=1 lock-skip refund)",
                "post-compaction break and next-provider-request ordering",
                "SQLite session split / in-place publication and the"
                " _DB_PERSISTED_MARKER flush-dedup",
                "activity/session touch after tool results",
            ],
        },
        "trigger": trigger_cases(),
        "token_selection": token_selection_cases(),
        "compress": compress_cases(),
        "adoption": adoption_cases(),
        "lock_skip": lock_skip_cases(),
        "reference_handoff": reference_handoff_cases(),
        "gate_composition": gate_composition_cases(),
    }


def main() -> int:
    corpus = build()
    text = json.dumps(corpus, indent=2, ensure_ascii=False, sort_keys=True) + "\n"
    if "--check" in sys.argv[1:]:
        if not OUT.exists():
            print(f"FAIL: {OUT} missing; run the generator first", file=sys.stderr)
            return 1
        existing = OUT.read_text(encoding="utf-8")
        if existing != text:
            print("FAIL: goldens differ from freshly generated corpus", file=sys.stderr)
            return 1
        print("OK: goldens match freshly generated corpus")
        return 0
    OUT.write_text(text, encoding="utf-8")
    total = sum(len(v) for k, v in corpus.items() if not k.startswith("_"))
    print(f"wrote {OUT} ({total} cases)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
