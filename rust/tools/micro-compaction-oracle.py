#!/usr/bin/env python3
"""Generate golden cases for the ContextCompressor micro-compaction state machine.

This executes the *real* ``ContextCompressor._micro_compact`` and its helpers
from ``agent/context_compressor.py`` so the Rust port is checked against CPython
behaviour rather than a paraphrase. Micro-compaction folds the single oldest
un-absorbed assistant/tool exchange into a rolling summary once per due turn,
splicing the absorbed span out and leaving one cumulative summary marker.

Only the auxiliary summarizer LLM is patched. ``_micro_summarize_one`` reaches
the network via ``agent.auxiliary_client.call_llm`` (imported lazily inside the
method), so a scripted fake stands in for it: each case supplies an explicit
``script`` list, one entry consumed per aux call, in order across passes and
defrag calls. The Rust port replays the same script, so the state machine, not
the summarizer, is what the golden pins. ``aux_interrupt_protection`` is
replaced with a null context manager, and ``get_model_context_length`` with a
fixed window, so construction and resolution never touch the network. No time,
random data, absolute paths, network, or credentials enter a golden.

The session DB is deliberately left unbound: ``_sync_micro_compact_to_db``
no-ops without ``_session_db``/``_session_id``, which is the exact shape the
in-memory splice must survive, so no persistence layer is patched or faked.

Each case records the pristine input, the initial in-memory micro state, the
scripted summarizer replies, and, per pass, the full output message list plus
the observable compressor state (cursor, rolling summary, failure counters,
pass/token totals, cadence counter, flush-cursor flag) and the marker count.

Run from the repository root with the project virtualenv:

    .venv/bin/python rust/tools/micro-compaction-oracle.py

Writes ``rust/tools/micro-compaction-goldens.json`` next to this script. Pass
``--check`` to verify the checked-in fixture still matches Python.
"""
from __future__ import annotations

import contextlib
import copy
import json
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent / "micro-compaction-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

import agent.context_compressor as cc  # noqa: E402
from agent.context_compressor import (  # noqa: E402
    COMPRESSED_SUMMARY_METADATA_KEY,
    COMPRESSED_SUMMARY_HAS_USER_TURN_KEY,
    MICRO_COMPACT_MARKER_KEY,
    _DB_PERSISTED_MARKER,
    _MICRO_COMPACT_MAX_CONSECUTIVE_FAILURES,
    HISTORICAL_TASK_HEADING,
    SUMMARY_PREFIX,
    _SUMMARY_END_MARKER,
)

FIXED_WINDOW = 40960

# State fields that make up one observable micro-compaction snapshot.
_STATE_FIELDS = (
    "_micro_compact_cursor",
    "_micro_compact_rolling_summary",
    "_micro_compact_consecutive_failures",
    "_micro_compact_last_failure_cursor",
    "_micro_compact_passes",
    "_micro_compact_tokens_saved_total",
    "_micro_compact_turns_since_pass",
    "_flush_scan_cursor_invalidated",
)


# ---------------------------------------------------------------------------
# scripted auxiliary summarizer
# ---------------------------------------------------------------------------
class _Message:
    def __init__(self, content: str) -> None:
        self.content = content


class _Choice:
    def __init__(self, content: str, finish_reason: str) -> None:
        self.message = _Message(content)
        self.finish_reason = finish_reason


class _Response:
    def __init__(self, content: str, finish_reason: str) -> None:
        self.choices = [_Choice(content, finish_reason)]


@contextlib.contextmanager
def scripted_aux(script: List[Dict[str, Any]]):
    """Patch the auxiliary summarizer with a deterministic scripted stand-in.

    Each ``call_llm`` call pops the next script entry:

      * ``ok``     -> a normal response whose content becomes the summary
      * ``empty``  -> a ``stop`` response with empty content (returns None)
      * ``length`` -> a ``length``-finish response (partial; returns None)
      * ``raise``  -> the aux call raises (returns None)

    An unscripted call is a mis-built case and raises.
    """
    calls = {"n": 0}

    def fake_call_llm(**kwargs: Any) -> _Response:
        i = calls["n"]
        calls["n"] += 1
        if i >= len(script):
            raise AssertionError(f"unscripted aux call #{i}")
        entry = script[i]
        kind = entry["kind"]
        if kind == "raise":
            raise RuntimeError("scripted summarizer failure")
        if kind == "length":
            return _Response(entry.get("content", "partial summary"), "length")
        if kind == "empty":
            return _Response("", "stop")
        if kind == "ok":
            return _Response(entry["content"], "stop")
        raise AssertionError(f"unknown script kind {kind!r}")

    def fake_interrupt(*_a: Any, **_k: Any):
        return contextlib.nullcontext()

    with patch("agent.auxiliary_client.call_llm", fake_call_llm), \
            patch("agent.auxiliary_client.aux_interrupt_protection", fake_interrupt), \
            patch.object(cc, "get_model_context_length", return_value=FIXED_WINDOW):
        yield calls


# ---------------------------------------------------------------------------
# compressor construction (mirrors tests/agent/test_micro_compaction.py)
# ---------------------------------------------------------------------------
def make_compressor(
    *,
    enabled: bool = True,
    every_n_turns: int = 1,
    protect_first_n: int = 1,
    protect_last_n: int = 2,
    model: str = "test-model",
    provider: str = "test",
    defrag_threshold_tokens: Optional[int] = None,
) -> "cc.ContextCompressor":
    """Build a ContextCompressor with micro-compaction configured, no network."""
    with patch.object(cc, "get_model_context_length", return_value=FIXED_WINDOW):
        c = cc.ContextCompressor(
            model=model,
            threshold_percent=0.75,
            protect_first_n=protect_first_n,
            protect_last_n=protect_last_n,
            quiet_mode=True,
            config_context_length=FIXED_WINDOW,
            provider=provider,
        )
        # Resolve the derived budgets while the window patch is active so no
        # later property access can fire a /models probe.
        _ = c.tail_token_budget
        _ = c.threshold_tokens
    c._micro_compact_enabled = enabled
    c._micro_compact_every_n_turns = every_n_turns
    if defrag_threshold_tokens is not None:
        c._micro_compact_defrag_threshold_tokens = defrag_threshold_tokens
    return c


def apply_state(c: "cc.ContextCompressor", state: Dict[str, Any]) -> None:
    for key, value in state.items():
        setattr(c, key, value)


def snapshot_state(c: "cc.ContextCompressor") -> Dict[str, Any]:
    return {field: getattr(c, field) for field in _STATE_FIELDS}


def marker_count(messages: List[Dict[str, Any]]) -> int:
    return sum(1 for m in messages if isinstance(m, dict)
               and m.get(COMPRESSED_SUMMARY_METADATA_KEY))


def markers(messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    return [m for m in messages if isinstance(m, dict)
            and m.get(COMPRESSED_SUMMARY_METADATA_KEY)]


# ---------------------------------------------------------------------------
# message builders
# ---------------------------------------------------------------------------
def system(text: str = "system prompt") -> Dict[str, Any]:
    return {"role": "system", "content": text}


def user(text: str, **extra: Any) -> Dict[str, Any]:
    msg = {"role": "user", "content": text}
    msg.update(extra)
    return msg


def call(cid: str, name: str = "f", args: str = "{}") -> Dict[str, Any]:
    return {"id": cid, "type": "function", "function": {"name": name, "arguments": args}}


def assistant(content: str = "", tool_calls: Optional[List[Dict[str, Any]]] = None,
              **extra: Any) -> Dict[str, Any]:
    msg: Dict[str, Any] = {"role": "assistant", "content": content}
    if tool_calls is not None:
        msg["tool_calls"] = tool_calls
    msg.update(extra)
    return msg


def tool(cid: str, content: Any = "T" * 400, **extra: Any) -> Dict[str, Any]:
    msg = {"role": "tool", "tool_call_id": cid, "content": content}
    msg.update(extra)
    return msg


def conversation(exchanges: int = 6) -> List[Dict[str, Any]]:
    """Plain alternating transcript, mirroring the test fixture."""
    msgs = [system()]
    for i in range(exchanges):
        msgs.append(user(f"question {i}"))
        msgs.append(assistant(f"answer {i} " + "z" * 400))
    return msgs


def tool_conversation(exchanges: int = 8, tools_per: int = 3) -> List[Dict[str, Any]]:
    """Tool-bearing transcript: each turn is assistant + N tool results."""
    msgs = [system("sys")]
    for i in range(exchanges):
        msgs.append(user(f"q{i}"))
        msgs.append(assistant(
            f"a{i}",
            [call(f"c{i}-{j}") for j in range(tools_per)],
        ))
        for j in range(tools_per):
            msgs.append(tool(f"c{i}-{j}", "T" * 500))
    return msgs


# ---------------------------------------------------------------------------
# case runner
# ---------------------------------------------------------------------------
def run_case(
    *,
    name: str,
    doc: str,
    config: Dict[str, Any],
    input_messages: List[Dict[str, Any]],
    script: List[Dict[str, Any]],
    passes: int,
    initial_state: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Execute *passes* real ``_micro_compact`` calls and record every pass.

    The output of each pass is fed into the next, mirroring how
    ``finalize_turn`` re-invokes the compressor with the compacted list.
    """
    c = make_compressor(**config)
    if initial_state:
        apply_state(c, initial_state)

    frozen_input = copy.deepcopy(input_messages)
    messages = copy.deepcopy(input_messages)
    pass_records: List[Dict[str, Any]] = []

    with scripted_aux(script) as calls:
        for _ in range(passes):
            aux_before = calls["n"]
            result = c._micro_compact(messages)
            pass_records.append({
                "output": copy.deepcopy(result),
                "state": snapshot_state(c),
                "marker_count": marker_count(result),
                "aux_calls": calls["n"] - aux_before,
            })
            messages = result
        total_aux = calls["n"]

    return {
        "name": name,
        "doc": doc,
        "config": config,
        "initial_state": initial_state or {},
        "input": frozen_input,
        "script": script,
        "passes": pass_records,
        "aux_calls_total": total_aux,
    }


def produce(
    config: Dict[str, Any],
    base_messages: List[Dict[str, Any]],
    script: List[Dict[str, Any]],
    n_passes: int,
) -> "tuple[cc.ContextCompressor, List[Dict[str, Any]]]":
    """Drive a throwaway compressor to build realistic input + state.

    Returns the compressor (holding the post-run micro state) and the
    resulting message list. Used to seed resume / defrag / batch-marker cases
    with a transcript that a real run would actually have produced.
    """
    c = make_compressor(**config)
    messages = copy.deepcopy(base_messages)
    with scripted_aux(script):
        for _ in range(n_passes):
            messages = c._micro_compact(messages)
    return c, messages


# ---------------------------------------------------------------------------
# cases
# ---------------------------------------------------------------------------
def build_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # -- disabled no-op ----------------------------------------------------
    dis = run_case(
        name="disabled-no-op",
        doc="Micro-compaction off: the transcript is returned unchanged and "
            "the summarizer is never called.",
        config={"enabled": False},
        input_messages=conversation(6),
        script=[],
        passes=1,
    )
    assert dis["passes"][0]["output"] == dis["input"]
    assert dis["aux_calls_total"] == 0
    cases.append(dis)

    # -- short transcript no-op -------------------------------------------
    short = run_case(
        name="short-transcript-no-op",
        doc="Fewer than four messages: below the structural floor, so the "
            "pass no-ops without an aux call.",
        config={},
        input_messages=[system("sys"), user("hi"), assistant("hello")],
        script=[],
        passes=1,
    )
    assert short["passes"][0]["output"] == short["input"]
    assert short["aux_calls_total"] == 0
    cases.append(short)

    # -- one exchange absorbed per due pass --------------------------------
    one = run_case(
        name="absorbs-exactly-one-exchange",
        doc="A single due pass folds exactly one assistant exchange into the "
            "rolling summary and leaves one assistant-role summary marker; the "
            "next exchange is untouched.",
        config={},
        input_messages=conversation(6),
        script=[{"kind": "ok", "content": "ROLLING SUMMARY"}],
        passes=1,
    )
    out0 = one["passes"][0]["output"]
    assert not any("answer 0" in str(m.get("content")) for m in out0)
    assert any("answer 1" in str(m.get("content")) for m in out0)
    assert one["passes"][0]["marker_count"] == 1
    the_marker = markers(out0)[0]
    assert the_marker["role"] == "assistant"
    assert the_marker[MICRO_COMPACT_MARKER_KEY] is True
    assert the_marker[COMPRESSED_SUMMARY_HAS_USER_TURN_KEY] is False
    assert one["passes"][0]["aux_calls"] == 1
    cases.append(one)

    # -- protected head and token-selected tail ----------------------------
    head_tail = run_case(
        name="protected-head-and-tail-preserved",
        doc="The system prompt at the head and the most recent turn in the "
            "token-selected tail are never absorbed.",
        config={},
        input_messages=conversation(6),
        script=[{"kind": "ok", "content": "ROLLING SUMMARY"}],
        passes=1,
    )
    ht_out = head_tail["passes"][0]["output"]
    assert ht_out[0] == head_tail["input"][0]
    assert ht_out[-1] == head_tail["input"][-1]
    cases.append(head_tail)

    # -- every-N-turn cadence ---------------------------------------------
    cadence = run_case(
        name="cadence-every-third-turn",
        doc="With every_n_turns=3 the first two passes only advance the "
            "cadence counter (no marker, no aux call); the third pass is due "
            "and absorbs, resetting the counter.",
        config={"every_n_turns": 3},
        input_messages=conversation(8),
        script=[{"kind": "ok", "content": "ROLLING SUMMARY"}],
        passes=3,
    )
    assert cadence["passes"][0]["marker_count"] == 0
    assert cadence["passes"][1]["marker_count"] == 0
    assert cadence["passes"][0]["state"]["_micro_compact_turns_since_pass"] == 1
    assert cadence["passes"][1]["state"]["_micro_compact_turns_since_pass"] == 2
    assert cadence["passes"][2]["marker_count"] == 1
    assert cadence["passes"][2]["state"]["_micro_compact_turns_since_pass"] == 0
    assert cadence["aux_calls_total"] == 1
    cases.append(cadence)

    # -- cadence clamped to at least one -----------------------------------
    clamp = run_case(
        name="cadence-clamped-to-one",
        doc="A bogus every_n_turns of 0 degrades to 'every turn' rather than "
            "disabling compaction or dividing by zero.",
        config={"every_n_turns": 0},
        input_messages=conversation(8),
        script=[{"kind": "ok", "content": "ROLLING SUMMARY"}],
        passes=1,
    )
    assert clamp["passes"][0]["marker_count"] == 1
    cases.append(clamp)

    # -- cumulative supersession, user bytes, alternation ------------------
    supersede = run_case(
        name="cumulative-supersession-keeps-one-marker",
        doc="Across successive passes the rolling summary is cumulative, so "
            "each pass supersedes the previous micro marker: exactly one "
            "marker survives, every user byte stays verbatim, and no two "
            "consecutive messages share the user/assistant role.",
        config={},
        input_messages=conversation(10),
        script=[{"kind": "ok", "content": f"ROLLING SUMMARY v{i}"} for i in range(5)],
        passes=5,
    )
    sup_out = supersede["passes"][-1]["output"]
    assert marker_count(sup_out) == 1
    original_users = [m["content"] for m in supersede["input"] if m["role"] == "user"]
    surviving = "\n\n".join(
        m["content"] for m in sup_out
        if m.get("role") == "user" and not m.get(COMPRESSED_SUMMARY_METADATA_KEY)
    )
    for orig in original_users:
        assert orig in surviving, orig
    for a, b in zip(sup_out, sup_out[1:]):
        ra, rb = a.get("role"), b.get("role")
        assert not (ra == rb and ra in ("user", "assistant")), (ra, rb)
    cases.append(supersede)

    # -- cursor relocation after tool-bearing splices ----------------------
    cursor = run_case(
        name="cursor-relocates-after-tool-splice",
        doc="A tool-bearing exchange collapses several messages into one "
            "marker, shifting every later index. The cursor is re-derived "
            "from the spliced list and sits exactly one past the marker, so "
            "the next pass does not skip the following exchange.",
        config={},
        input_messages=tool_conversation(8, tools_per=3),
        script=[{"kind": "ok", "content": f"ROLLING SUMMARY {i}"} for i in range(4)],
        passes=4,
    )
    for rec in cursor["passes"]:
        out = rec["output"]
        midx = next(i for i, m in enumerate(out)
                    if m.get(COMPRESSED_SUMMARY_METADATA_KEY))
        assert rec["state"]["_micro_compact_cursor"] == midx + 1
    cases.append(cursor)

    # -- marker rendering and rolling-summary extraction -------------------
    render = run_case(
        name="marker-rendering-and-extraction",
        doc="The marker content is the SUMMARY_PREFIX / historical-heading / "
            "end-marker wrapper around the rolling summary, and "
            "_rolling_summary_from_marker round-trips it back to the exact "
            "rolling summary text.",
        config={},
        input_messages=conversation(6),
        script=[{"kind": "ok", "content": "decisions: use the existing helper"}],
        passes=1,
    )
    r_marker = markers(render["passes"][0]["output"])[0]
    rendered = cc.ContextCompressor._render_micro_marker_content(
        "decisions: use the existing helper")
    assert r_marker["content"] == rendered
    extracted = cc.ContextCompressor._rolling_summary_from_marker(r_marker["content"])
    assert extracted == "decisions: use the existing helper"
    render["derived"] = {
        "rendered_marker_content": rendered,
        "extracted_rolling_summary": extracted,
    }
    cases.append(render)

    # -- bounded repeated-summary failure ----------------------------------
    fail = run_case(
        name="bounded-summarize-failure",
        doc="An exchange the summarizer cannot handle (empty content) leaves "
            "the transcript untouched and increments the consecutive-failure "
            "counter on that cursor position.",
        config={},
        input_messages=conversation(6),
        script=[{"kind": "empty"}, {"kind": "empty"}],
        passes=2,
    )
    assert fail["passes"][0]["output"] == fail["input"]
    assert fail["passes"][0]["state"]["_micro_compact_consecutive_failures"] == 1
    assert fail["passes"][1]["state"]["_micro_compact_consecutive_failures"] == 2
    assert fail["passes"][0]["marker_count"] == 0
    cases.append(fail)

    # -- poison-exchange cursor skip ---------------------------------------
    poison = run_case(
        name="poison-exchange-cursor-skip",
        doc="After MAX consecutive failures on the same cursor the stuck "
            "exchange is skipped: the cursor advances past it and the failure "
            "counters reset so the next turn attempts new material.",
        config={},
        input_messages=conversation(6),
        script=[{"kind": "empty"} for _ in range(_MICRO_COMPACT_MAX_CONSECUTIVE_FAILURES)],
        passes=_MICRO_COMPACT_MAX_CONSECUTIVE_FAILURES,
    )
    last = poison["passes"][-1]["state"]
    assert last["_micro_compact_cursor"] > 0
    assert last["_micro_compact_consecutive_failures"] == 0
    assert last["_micro_compact_last_failure_cursor"] == -1
    # No marker was ever created (every pass failed).
    assert all(rec["marker_count"] == 0 for rec in poison["passes"])
    cases.append(poison)

    # -- length-stop and raise guards (single-pass no-ops) -----------------
    length_stop = run_case(
        name="summarize-length-stop-no-op",
        doc="A finish_reason=length response is a partial summary and is "
            "discarded: the pass no-ops and counts one failure.",
        config={},
        input_messages=conversation(6),
        script=[{"kind": "length"}],
        passes=1,
    )
    assert length_stop["passes"][0]["output"] == length_stop["input"]
    assert length_stop["passes"][0]["state"]["_micro_compact_consecutive_failures"] == 1
    cases.append(length_stop)

    raise_guard = run_case(
        name="summarize-raise-no-op",
        doc="An aux call that raises is caught: the pass no-ops and counts one "
            "failure.",
        config={},
        input_messages=conversation(6),
        script=[{"kind": "raise"}],
        passes=1,
    )
    assert raise_guard["passes"][0]["output"] == raise_guard["input"]
    assert raise_guard["passes"][0]["state"]["_micro_compact_consecutive_failures"] == 1
    cases.append(raise_guard)

    # -- resume rehydration from a micro marker ----------------------------
    prod_c, resume_input = produce(
        {}, conversation(10),
        [{"kind": "ok", "content": "HISTORY: decisions and paths"} for _ in range(3)],
        3,
    )
    assert marker_count(resume_input) == 1
    resume = run_case(
        name="resume-rehydrates-from-marker",
        doc="A fresh (resumed) compressor starts with an empty rolling summary "
            "but the marker holding the compacted history is still in the "
            "transcript. The first pass rehydrates the rolling summary from "
            "that marker, so its supersede keeps one merged marker instead of "
            "replacing the whole history with a one-exchange summary.",
        config={},
        input_messages=resume_input,
        script=[{"kind": "ok", "content": "MERGED: history plus newest exchange"}],
        passes=1,
    )
    res_out = resume["passes"][0]["output"]
    assert marker_count(res_out) == 1
    assert "MERGED" in markers(res_out)[0]["content"]
    # The rolling summary was rehydrated before the merge (non-empty going in).
    cases.append(resume)

    # -- failed rehydration preserves the old marker -----------------------
    # A micro marker whose body is blank cannot be recovered, so the rolling
    # summary stays empty, the pass does not supersede, and the un-carried
    # history marker is retained alongside the new one.
    blank_marker = {
        "role": "assistant",
        "content": cc.ContextCompressor._render_micro_marker_content(""),
        COMPRESSED_SUMMARY_METADATA_KEY: True,
        MICRO_COMPACT_MARKER_KEY: True,
        COMPRESSED_SUMMARY_HAS_USER_TURN_KEY: False,
    }
    failed_rehydrate_input = [system(), user("earlier prompt"), blank_marker]
    for i in range(6):
        failed_rehydrate_input.append(user(f"question {i}"))
        failed_rehydrate_input.append(assistant(f"answer {i} " + "z" * 400))
    assert cc.ContextCompressor._rolling_summary_from_marker(
        blank_marker["content"]) == ""
    failed = run_case(
        name="failed-rehydration-preserves-marker",
        doc="When the prior marker's summary cannot be recovered (blank body), "
            "rehydration yields nothing, the pass does not supersede, and the "
            "un-carried history marker survives beside the freshly added one.",
        config={},
        input_messages=failed_rehydrate_input,
        script=[{"kind": "ok", "content": "BRAND NEW SUMMARY"}],
        passes=1,
    )
    f_out = failed["passes"][0]["output"]
    assert marker_count(f_out) == 2
    cases.append(failed)

    # -- defrag success ----------------------------------------------------
    # Seed a real micro marker via one absorb pass, then oversize the rolling
    # summary so the next pass defrags instead of absorbing: shape-neutral
    # rewrite of the marker in place, cursor unmoved, flush cursor invalidated.
    defrag_c, defrag_input = produce(
        {}, conversation(8),
        [{"kind": "ok", "content": "seed summary"}], 1,
    )
    defrag_seed_state = snapshot_state(defrag_c)
    defrag_seed_state["_micro_compact_rolling_summary"] = "x" * 40_000
    shape_before = [m.get("role") for m in defrag_input]
    cursor_before = defrag_seed_state["_micro_compact_cursor"]
    defrag = run_case(
        name="defrag-rewrites-marker-in-place",
        doc="Once the rolling summary itself grows past the defrag threshold, "
            "the pass re-summarizes the summary text and rewrites the existing "
            "micro marker in place: no message is spliced, the cursor does not "
            "move, and the flush-scan cursor is invalidated because the marker "
            "dict's persistence stamp was popped in place.",
        config={},
        input_messages=defrag_input,
        script=[{"kind": "ok", "content": "FRESH DEFRAGGED SUMMARY"}],
        passes=1,
        initial_state=defrag_seed_state,
    )
    d_out = defrag["passes"][0]["output"]
    assert [m.get("role") for m in d_out] == shape_before
    assert defrag["passes"][0]["state"]["_micro_compact_cursor"] == cursor_before
    assert defrag["passes"][0]["state"]["_micro_compact_rolling_summary"] == \
        "FRESH DEFRAGGED SUMMARY"
    assert defrag["passes"][0]["state"]["_flush_scan_cursor_invalidated"] is True
    assert marker_count(d_out) == 1
    assert "FRESH DEFRAGGED SUMMARY" in markers(d_out)[0]["content"]
    cases.append(defrag)

    # -- defrag failure ----------------------------------------------------
    # Same setup, but the defrag aux call fails: the old rolling summary is
    # restored, the marker is not rewritten, and no flush invalidation fires.
    dfail_c, dfail_input = produce(
        {}, conversation(8),
        [{"kind": "ok", "content": "seed summary"}], 1,
    )
    dfail_seed_state = snapshot_state(dfail_c)
    old_rolling = "y" * 40_000
    dfail_seed_state["_micro_compact_rolling_summary"] = old_rolling
    dfail_shape = [m.get("role") for m in dfail_input]
    dfail = run_case(
        name="defrag-failure-restores-summary",
        doc="If the defrag aux call fails, the oversized rolling summary is "
            "restored unchanged, the marker keeps its content, and the "
            "flush-scan cursor is not invalidated.",
        config={},
        input_messages=dfail_input,
        script=[{"kind": "empty"}],
        passes=1,
        initial_state=dfail_seed_state,
    )
    df_out = dfail["passes"][0]["output"]
    assert [m.get("role") for m in df_out] == dfail_shape
    assert dfail["passes"][0]["state"]["_micro_compact_rolling_summary"] == old_rolling
    assert dfail["passes"][0]["state"]["_flush_scan_cursor_invalidated"] is False
    cases.append(dfail)

    # -- defrag never rewrites a batch marker ------------------------------
    batch_defrag_input = [system("sys"), {
        "role": "user",
        "content": "[batch summary] CRITICAL HISTORY: exchanges 1..m",
        COMPRESSED_SUMMARY_METADATA_KEY: True,
    }]
    for i in range(6):
        batch_defrag_input.append(user(f"q{i}"))
        batch_defrag_input.append(assistant(f"a{i} " + "z" * 400))
    batch_defrag = run_case(
        name="defrag-never-rewrites-batch-marker",
        doc="Defrag rewrites only micro-tagged markers. A batch-compaction "
            "marker (shared metadata key but no micro tag) holds history the "
            "rolling summary does not contain, so its content is left intact.",
        config={},
        input_messages=batch_defrag_input,
        script=[{"kind": "ok", "content": "DEFRAGGED"}],
        passes=1,
        initial_state={"_micro_compact_rolling_summary": "z" * 40_000},
    )
    bd_out = batch_defrag["passes"][0]["output"]
    assert any("CRITICAL HISTORY" in str(m.get("content")) for m in bd_out)
    cases.append(batch_defrag)

    # -- supersede never drops a batch marker ------------------------------
    # One absorb pass, then swap the micro marker for a batch marker (no micro
    # tag). The next pass's supersede must not treat it as redundant.
    sup_c, sup_seed = produce(
        {}, conversation(8),
        [{"kind": "ok", "content": "MICRO SUMMARY (exchanges 1..k)"}], 1,
    )
    sup_state = snapshot_state(sup_c)
    micro_idx = next(i for i, m in enumerate(sup_seed)
                     if m.get(COMPRESSED_SUMMARY_METADATA_KEY))
    batch_marker = {
        "role": "user",
        "content": "[batch summary] CRITICAL HISTORY: exchanges 1..m",
        COMPRESSED_SUMMARY_METADATA_KEY: True,
    }
    sup_batch_input = sup_seed[:micro_idx] + [batch_marker] + sup_seed[micro_idx + 1:]
    batch_supersede = run_case(
        name="supersede-never-drops-batch-marker",
        doc="A batch marker that landed after the last micro pass holds more "
            "history than the stale rolling summary. Supersede drops only "
            "micro-tagged markers, so the batch summary survives the next "
            "pass.",
        config={},
        input_messages=sup_batch_input,
        script=[{"kind": "ok", "content": "MICRO SUMMARY v2"}],
        passes=1,
        initial_state=sup_state,
    )
    bs_out = batch_supersede["passes"][0]["output"]
    assert any("CRITICAL HISTORY" in str(m.get("content")) for m in bs_out)
    cases.append(batch_supersede)

    # -- stale api_content removed when adjacent user turns merge ----------
    # Pass 1 leaves a marker between two real user turns (one carrying an
    # api_content sidecar). Pass 2 supersedes that marker, making the two user
    # turns adjacent; the merge joins them and drops the now-stale sidecar.
    merge_input = [
        system("sys"),
        user("first real user prompt", api_content="WIRE-BYTES-FIRST"),
        assistant("a0", [call("m0")]),
        tool("m0", "T" * 300),
        user("second real user prompt"),
        assistant("a1", [call("m1")]),
        tool("m1", "T" * 300),
    ]
    # Trailing filler keeps the compress window open past the second exchange
    # so pass 2 is due to absorb it and supersede the pass-1 marker.
    for i in range(6):
        merge_input.append(user(f"filler {i}"))
        merge_input.append(assistant(f"fa{i} " + "z" * 200))
    merge = run_case(
        name="stale-api-content-dropped-on-user-merge",
        doc="Superseding a marker that sat between two real user turns leaves "
            "them adjacent. Merging them (\\n\\n-joined) keeps every user byte "
            "and drops the api_content sidecar of the rewritten turn, since "
            "replaying it would resend bytes the merge changed.",
        config={},
        input_messages=merge_input,
        script=[{"kind": "ok", "content": "SUM v1"}, {"kind": "ok", "content": "SUM v2"}],
        passes=2,
    )
    # After pass 1 the sidecar is still present (no supersede yet).
    p1_users = [m for m in merge["passes"][0]["output"]
                if m.get("role") == "user" and "first real user prompt" in str(m.get("content"))]
    assert p1_users and p1_users[0].get("api_content") == "WIRE-BYTES-FIRST"
    # After pass 2 the two user turns merged and the sidecar is gone.
    m2 = merge["passes"][1]["output"]
    merged = [m for m in m2 if m.get("role") == "user"
              and "first real user prompt" in str(m.get("content"))]
    assert merged, "the first user turn must survive"
    assert "second real user prompt" in merged[0]["content"], "user bytes merged"
    assert "api_content" not in merged[0], "stale sidecar must be dropped"
    assert marker_count(m2) == 1
    cases.append(merge)

    # -- db-persisted stamps survive the splice ----------------------------
    stamped_input = conversation(8)
    for m in stamped_input:
        m[_DB_PERSISTED_MARKER] = True
    stamped = run_case(
        name="splice-preserves-db-persisted-stamps",
        doc="With no DB bound, _sync_micro_compact_to_db no-ops. The splice "
            "must not strip _db_persisted from surviving messages, or the next "
            "append-only flush would re-insert them as duplicate active rows.",
        config={},
        input_messages=stamped_input,
        script=[{"kind": "ok", "content": "ROLLING SUMMARY"}],
        passes=1,
    )
    st_out = stamped["passes"][0]["output"]
    unstamped = [m for m in st_out
                 if not m.get(_DB_PERSISTED_MARKER)
                 and not m.get(COMPRESSED_SUMMARY_METADATA_KEY)]
    assert not unstamped
    cases.append(stamped)

    return cases


# ---------------------------------------------------------------------------
# assembly
# ---------------------------------------------------------------------------
def build() -> Dict[str, Any]:
    return {
        "source": "agent/context_compressor.py",
        "notes": (
            "Generated by rust/tools/micro-compaction-oracle.py from the real "
            "ContextCompressor micro-compaction state machine. Do not hand-edit; "
            "regenerate instead. Only the auxiliary summarizer is faked (scripted "
            "per case); the DB is left unbound so _sync_micro_compact_to_db "
            "no-ops. Each pass records the full output list and observable state."
        ),
        "constants": {
            "fixed_context_window": FIXED_WINDOW,
            "max_consecutive_failures": _MICRO_COMPACT_MAX_CONSECUTIVE_FAILURES,
            "default_defrag_threshold_tokens": 2000,
            "chars_per_token": cc._CHARS_PER_TOKEN,
            "compressed_summary_metadata_key": COMPRESSED_SUMMARY_METADATA_KEY,
            "compressed_summary_has_user_turn_key": COMPRESSED_SUMMARY_HAS_USER_TURN_KEY,
            "micro_compact_marker_key": MICRO_COMPACT_MARKER_KEY,
            "db_persisted_marker": _DB_PERSISTED_MARKER,
            "historical_task_heading": HISTORICAL_TASK_HEADING,
            "summary_prefix": SUMMARY_PREFIX,
            "summary_end_marker": _SUMMARY_END_MARKER,
        },
        "cases": build_cases(),
    }


def main(argv: List[str]) -> int:
    data = build()
    rendered = json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if "--check" in argv:
        if not OUT.exists():
            print(f"MISSING: {OUT.relative_to(REPO_ROOT)} does not exist; run the "
                  "generator without --check first.", file=sys.stderr)
            return 1
        current = OUT.read_text(encoding="utf-8")
        if current != rendered:
            print(f"DRIFT: {OUT.relative_to(REPO_ROOT)} is stale relative to "
                  "agent/context_compressor.py; regenerate it.", file=sys.stderr)
            return 1
        print(f"OK: {OUT.relative_to(REPO_ROOT)} matches Python "
              f"({len(data['cases'])} cases).")
        return 0
    OUT.write_text(rendered, encoding="utf-8")
    print(f"Wrote {OUT.relative_to(REPO_ROOT)} ({len(data['cases'])} cases).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
