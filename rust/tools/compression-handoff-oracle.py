#!/usr/bin/env python3
"""Golden oracle for the full-compression *handoff layer*.

This drives the *real* Python functions that build, classify, strip, and
re-anchor a context-compaction handoff, so the Rust port can be reviewed
against CPython behaviour rather than a paraphrase. Nothing here reimplements a
decision: every recorded runtime value is the return of an authoritative local
function (or a verbatim source block executed against controlled inputs), run
under patched-out I/O.

Scope (the handoff layer only):

  * the exact summary prefix, historical heading, end marker, continuation
    strings, merge delimiters, and every frozen historical prefix, recorded as
    pure constants (no runtime);
  * prefix recognition / stripping / re-normalization
    (``_starts_with_summary_prefix``, ``_strip_summary_prefix``,
    ``_with_summary_prefix``, ``classify_summary_content``);
  * context-summary and synthetic-user classification *after* SessionDB
    projection strips private ``_``-prefixed metadata (content-only rows);
  * summary role selection and collision behaviour across user / assistant /
    tool-call template-visible combinations, including merge-into-tail and
    forced-user-leading, driven by executing the real inline decision block
    lifted verbatim from ``ContextCompressor.compress``;
  * stripping / unwrapping standalone and merged old handoffs on
    re-compression (``_strip_context_summary_handoff_message``);
  * zero-real-user continuation insertion and real-user anchor preservation
    (``_ensure_compressed_has_user_turn`` / ``_insert_real_user_anchor``);
  * ``reference_handoff_would_drive_next_model_call`` across a standalone
    handoff, a later real user turn, a tool result, a pending assistant tool
    call, and composite (merged / force-user-leading) carriers.

EXCLUDED on purpose (AGY owns that independent lane): token-tail selection and
``min_tail_user_messages``. This oracle never asserts which messages land in the
protected tail or how the tail token budget is spent; the role-selection cases
feed *controlled* head/tail role lists straight into the real decision block so
the handoff lane is exercised without entangling AGY's sizing lane. Todo
snapshot insertion is likewise out of scope.

Authoritative sources executed (current local source):

  * agent/context_compressor.py: ``ContextCompressor._starts_with_summary_prefix``
    / ``_strip_summary_prefix`` / ``_with_summary_prefix`` /
    ``classify_summary_content`` / ``_is_context_summary_content`` /
    ``_is_synthetic_compression_user_turn`` / ``_is_actionable_user_turn`` /
    ``_transcript_has_real_user_turn`` /
    ``_strip_context_summary_handoff_message``.
  * agent/context_compressor.py: ``ContextCompressor.compress``, the inline
    summary role-selection block (the ``_merge_summary_into_tail`` /
    ``last_head_role`` / ``first_tail_role`` / ``_force_user_leading`` /
    ``summary_role`` decision), compiled verbatim from source and executed.
  * agent/context_compressor.py: ``reference_handoff_would_drive_next_model_call``
    / ``is_compaction_summary_message``.
  * agent/conversation_compression.py: ``_ensure_compressed_has_user_turn`` /
    ``_insert_real_user_anchor`` / ``_is_real_user_message``.

Only construction is patched: ``get_model_context_length`` is pinned so the
compressor never probes ``/models``. No network, timestamps, randomness,
absolute paths, or credentials enter a golden. Run from the repository root
with the project virtualenv:

    .venv/bin/python rust/tools/compression-handoff-oracle.py

Writes ``rust/tools/compression-handoff-goldens.json`` next to this script.
Pass ``--check`` to regenerate in memory and compare against the checked-in
fixture.
"""
from __future__ import annotations

import ast
import inspect
import json
import logging
import sys
import textwrap
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent / "compression-handoff-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

logging.getLogger("agent.context_compressor").setLevel(logging.CRITICAL)

import agent.context_compressor as cc  # noqa: E402
from agent.context_compressor import (  # noqa: E402
    COMPRESSED_SUMMARY_HAS_USER_TURN_KEY,
    COMPRESSED_SUMMARY_METADATA_KEY,
    COMPRESSION_CONTINUATION_USER_CONTENT,
    HISTORICAL_TASK_HEADING,
    LEGACY_SUMMARY_PREFIX,
    MAX_ITERATIONS_SUMMARY_REQUEST,
    SUMMARY_PREFIX,
    ContextCompressor,
    _content_text_for_contains,
    _HISTORICAL_SUMMARY_PREFIXES,
    _last_template_visible_role,
    _LEGACY_COMPRESSION_CONTINUATION_USER_CONTENT,
    _MERGED_PRIOR_CONTEXT_HEADER,
    _MERGED_SUMMARY_DELIMITER,
    _SUMMARY_END_MARKER,
    _template_visible_role,
    is_compaction_summary_message,
    reference_handoff_would_drive_next_model_call,
)
from agent.conversation_compression import (  # noqa: E402
    _ensure_compressed_has_user_turn,
    _is_real_user_message,
)

MK = COMPRESSED_SUMMARY_METADATA_KEY
HK = COMPRESSED_SUMMARY_HAS_USER_TURN_KEY
FIXED_WINDOW = 8000


# ---------------------------------------------------------------------------
# message builders
# ---------------------------------------------------------------------------
def system(text: str = "system prompt") -> Dict[str, Any]:
    return {"role": "system", "content": text}


def user(text: str, **extra: Any) -> Dict[str, Any]:
    msg: Dict[str, Any] = {"role": "user", "content": text}
    msg.update(extra)
    return msg


def assistant(content: str = "", tool_calls: Optional[List[Dict[str, Any]]] = None,
              **extra: Any) -> Dict[str, Any]:
    msg: Dict[str, Any] = {"role": "assistant", "content": content}
    if tool_calls is not None:
        msg["tool_calls"] = tool_calls
    msg.update(extra)
    return msg


def tool_call(cid: str = "c1", name: str = "run", args: str = "{}") -> Dict[str, Any]:
    return {"id": cid, "type": "function", "function": {"name": name, "arguments": args}}


def tool(cid: str = "c1", content: str = "tool result") -> Dict[str, Any]:
    return {"role": "tool", "tool_call_id": cid, "content": content}


def standalone_handoff(body: str = "port work", *, with_meta: bool = True) -> Dict[str, Any]:
    """A standalone reference handoff: current prefix + body + end marker."""
    content = (
        SUMMARY_PREFIX + f"\n{HISTORICAL_TASK_HEADING}\n{body}\n\n" + _SUMMARY_END_MARKER
    )
    msg: Dict[str, Any] = {"role": "user", "content": content}
    if with_meta:
        msg[MK] = True
    return msg


def merged_carrier(prior: str = "the real tail ask", body: str = "port work",
                   *, role: str = "user", with_meta: bool = True,
                   **extra: Any) -> Dict[str, Any]:
    """A merge-into-tail carrier: prior tail content, delimiter, then summary."""
    content = (
        _MERGED_PRIOR_CONTEXT_HEADER + f"\n{prior}\n\n"
        + _MERGED_SUMMARY_DELIMITER + "\n\n"
        + SUMMARY_PREFIX + f"\n{body}\n\n" + _SUMMARY_END_MARKER
    )
    msg: Dict[str, Any] = {"role": role, "content": content}
    if with_meta:
        msg[MK] = True
    msg.update(extra)
    return msg


def force_user_leading_carrier(ask: str = "the real user ask", body: str = "port work",
                               *, with_meta: bool = True) -> Dict[str, Any]:
    """A force-user-leading carrier: summary + end marker, then the live ask."""
    content = SUMMARY_PREFIX + f"\n{body}\n\n" + _SUMMARY_END_MARKER + f"\n\n{ask}"
    msg: Dict[str, Any] = {"role": "user", "content": content}
    if with_meta:
        msg[MK] = True
    return msg


# ---------------------------------------------------------------------------
# projection helpers (never persistence-mutating; observation only)
# ---------------------------------------------------------------------------
def project_persisted(message: Dict[str, Any]) -> Dict[str, Any]:
    """Drop every private ``_``-prefixed key, exactly as SessionDB projection
    does. The role/content survive; the compression metadata does not, so the
    content-marker recognizers are what classify the row on the far side."""
    return {k: v for k, v in message.items() if not k.startswith("_")}


# ---------------------------------------------------------------------------
# real inline role-selection decision block, lifted verbatim from
# ContextCompressor.compress and executed against controlled inputs.
# ---------------------------------------------------------------------------
def _load_role_decision():
    """Compile the summary role-selection block straight from compress()."""
    src_lines = inspect.getsource(cc.ContextCompressor.compress).splitlines()

    def _line(sub: str) -> int:
        return next(i for i, ln in enumerate(src_lines) if sub in ln)

    start = _line("_merge_summary_into_tail = False")
    end = _line("_merge_summary_into_tail = bool(tail_messages)")
    block = textwrap.dedent("\n".join(src_lines[start:end + 1]))
    # Fail loud if the block ever drifts to reference names we do not provide;
    # the golden must never silently execute a re-paraphrased decision.
    provided = {
        "Any", "Dict", "Optional", "_content_text_for_contains",
        "_last_template_visible_role", "_template_visible_role",
        "compressed", "tail_messages", "compress_start",
        # locals the block assigns
        "_merge_summary_into_tail", "last_head_role", "first_tail_role",
        "first_tail_visible_idx", "_force_user_leading", "summary_role",
        "_is_nonempty_user_turn", "_user_survives", "flipped",
        # comprehension / builtin names
        "message", "m", "idx", "role", "any", "bool", "next", "enumerate",
        "int", "str",
    }
    referenced = {n.id for n in ast.walk(ast.parse(block)) if isinstance(n, ast.Name)}
    unexpected = referenced - provided
    if unexpected:
        raise AssertionError(f"role-selection block references {sorted(unexpected)}")
    return compile(block, "<compress role-selection block>", "exec"), start, end


_ROLE_DECISION_CODE, _ROLE_BLOCK_START, _ROLE_BLOCK_END = _load_role_decision()


def decide_summary_role(
    compressed: List[Dict[str, Any]],
    tail_messages: List[Dict[str, Any]],
    compress_start: int,
) -> Dict[str, Any]:
    """Execute the real decision block; return its computed locals."""
    ns: Dict[str, Any] = {
        "Any": Any, "Dict": Dict, "Optional": Optional,
        "_content_text_for_contains": _content_text_for_contains,
        "_last_template_visible_role": _last_template_visible_role,
        "_template_visible_role": _template_visible_role,
        "compressed": compressed,
        "tail_messages": tail_messages,
        "compress_start": compress_start,
    }
    exec(_ROLE_DECISION_CODE, ns)
    return {
        "summary_role": ns["summary_role"],
        "merge_into_tail": ns["_merge_summary_into_tail"],
        "force_user_leading": ns["_force_user_leading"],
        "last_head_role": ns["last_head_role"],
        "first_tail_role": ns["first_tail_role"],
        "first_tail_visible_idx": ns["first_tail_visible_idx"],
    }


# ===========================================================================
# 1. constants  (pure, no runtime)
# ===========================================================================
def constant_cases() -> List[Dict[str, Any]]:
    """Exact byte constants the Rust port must reproduce verbatim.

    These carry no runtime coverage; they are the frozen wire/recognition
    strings. em dashes appear inside the recorded Python constants (byte
    parity requires them) and only there.
    """
    return [
        {"name": "SUMMARY_PREFIX", "value": SUMMARY_PREFIX},
        {"name": "LEGACY_SUMMARY_PREFIX", "value": LEGACY_SUMMARY_PREFIX},
        {"name": "HISTORICAL_TASK_HEADING", "value": HISTORICAL_TASK_HEADING},
        {"name": "SUMMARY_END_MARKER", "value": _SUMMARY_END_MARKER},
        {"name": "MERGED_PRIOR_CONTEXT_HEADER", "value": _MERGED_PRIOR_CONTEXT_HEADER},
        {"name": "MERGED_SUMMARY_DELIMITER", "value": _MERGED_SUMMARY_DELIMITER},
        {"name": "COMPRESSED_SUMMARY_METADATA_KEY", "value": COMPRESSED_SUMMARY_METADATA_KEY},
        {"name": "COMPRESSED_SUMMARY_HAS_USER_TURN_KEY", "value": COMPRESSED_SUMMARY_HAS_USER_TURN_KEY},
        {"name": "COMPRESSION_CONTINUATION_USER_CONTENT", "value": COMPRESSION_CONTINUATION_USER_CONTENT},
        {"name": "LEGACY_COMPRESSION_CONTINUATION_USER_CONTENT",
         "value": _LEGACY_COMPRESSION_CONTINUATION_USER_CONTENT},
        {"name": "MAX_ITERATIONS_SUMMARY_REQUEST", "value": MAX_ITERATIONS_SUMMARY_REQUEST},
        {"name": "HISTORICAL_SUMMARY_PREFIXES_count", "value": len(_HISTORICAL_SUMMARY_PREFIXES)},
        {"name": "HISTORICAL_SUMMARY_PREFIXES", "value": list(_HISTORICAL_SUMMARY_PREFIXES)},
    ]


# ===========================================================================
# 2. prefix recognition / stripping / normalization  (runtime)
# ===========================================================================
def prefix_recognition_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    body = f"{HISTORICAL_TASK_HEADING}\nport work\nmore body"

    # current prefix
    cur = f"{SUMMARY_PREFIX}\n{body}\n\n{_SUMMARY_END_MARKER}"
    assert ContextCompressor._starts_with_summary_prefix(cur) is True
    stripped = ContextCompressor._strip_summary_prefix(cur)
    assert not stripped.startswith(SUMMARY_PREFIX)
    assert _SUMMARY_END_MARKER not in stripped
    renorm = ContextCompressor._with_summary_prefix(cur)
    assert renorm.startswith(SUMMARY_PREFIX)
    cases.append({
        "case": "current_prefix_recognized_stripped_renormalized",
        "why": "current handoff prefix is recognized, its prefix + end marker stripped, then re-normalized",
        "starts_with_prefix": True,
        "classification": ContextCompressor.classify_summary_content(cur),
        "stripped_body": stripped,
        "renormalized_starts_with_current": renorm.startswith(SUMMARY_PREFIX),
    })

    # legacy prefix
    legacy = f"{LEGACY_SUMMARY_PREFIX} old summary body"
    assert ContextCompressor._starts_with_summary_prefix(legacy) is True
    cases.append({
        "case": "legacy_prefix_recognized",
        "why": "legacy [CONTEXT SUMMARY]: prefix is still recognized and stripped",
        "starts_with_prefix": True,
        "classification": ContextCompressor.classify_summary_content(legacy),
        "stripped_body": ContextCompressor._strip_summary_prefix(legacy),
    })

    # every historical prefix must remain recognized and re-normalized to current
    for i, hist in enumerate(_HISTORICAL_SUMMARY_PREFIXES):
        text = f"{hist}\n## Historical Task Snapshot\nstale directive body"
        assert ContextCompressor._starts_with_summary_prefix(text) is True
        renorm = ContextCompressor._with_summary_prefix(text)
        assert renorm.startswith(SUMMARY_PREFIX)
        assert not renorm[len(SUMMARY_PREFIX):].lstrip().startswith("[CONTEXT COMPACTION")
        cases.append({
            "case": f"historical_prefix_{i}_recognized_and_renormalized",
            "why": "a summary persisted under an older prefix stays strippable so the stale directive never survives re-compaction",
            "starts_with_prefix": True,
            "classification": ContextCompressor.classify_summary_content(text),
            "renormalized_to_current_prefix": renorm.startswith(SUMMARY_PREFIX),
            "old_prefix_removed": not renorm[len(SUMMARY_PREFIX):].lstrip().startswith("[CONTEXT COMPACTION"),
        })

    # merged carrier: prefix lands AFTER the delimiter -> classified "merged"
    merged = merged_carrier()["content"]
    assert ContextCompressor.classify_summary_content(merged) == "merged"
    merged_body = ContextCompressor._strip_summary_prefix(merged)
    assert _MERGED_PRIOR_CONTEXT_HEADER not in merged_body
    assert not merged_body.startswith(SUMMARY_PREFIX)
    cases.append({
        "case": "merged_carrier_classified_merged_and_stripped_to_body",
        "why": "merge-into-tail carrier is 'merged'; strip drops prior-tail wrapper up to the delimiter and the prefix",
        "classification": "merged",
        "stripped_body": merged_body,
    })

    # plain user text: no prefix
    plain = "just a normal user question about the weather"
    assert ContextCompressor._starts_with_summary_prefix(plain) is False
    assert ContextCompressor.classify_summary_content(plain) is None
    cases.append({
        "case": "plain_user_text_not_a_summary",
        "why": "ordinary user text is neither recognized as a prefix nor classified as a summary",
        "starts_with_prefix": False,
        "classification": None,
    })

    # content that merely quotes the heading is not a handoff
    quote = f"can you look at {HISTORICAL_TASK_HEADING} in the doc?"
    assert ContextCompressor.classify_summary_content(quote) is None
    cases.append({
        "case": "heading_quote_not_a_summary",
        "why": "quoting the historical heading without the prefix is not a handoff",
        "starts_with_prefix": False,
        "classification": None,
    })

    return cases


# ===========================================================================
# 3. classification after persistence projection  (runtime)
# ===========================================================================
def classification_cases() -> List[Dict[str, Any]]:
    """Context-summary + synthetic-user classification after private metadata
    is stripped by SessionDB projection (content-only rows)."""
    cases: List[Dict[str, Any]] = []

    def record(name: str, why: str, message: Dict[str, Any]) -> None:
        cases.append({
            "case": name,
            "why": why,
            "role": message.get("role"),
            "is_compaction_summary_message": is_compaction_summary_message(message),
            "is_context_summary_content": ContextCompressor._is_context_summary_content(
                message.get("content")
            ),
            "is_synthetic_compression_user_turn":
                ContextCompressor._is_synthetic_compression_user_turn(message),
            "is_actionable_user_turn": ContextCompressor._is_actionable_user_turn(message),
            "is_real_user_message": _is_real_user_message(message),
        })

    # standalone handoff, metadata stripped -> content prefix still classifies it
    record(
        "projected_standalone_handoff_classified_by_content",
        "after projection drops the private marker, the content prefix keeps a standalone handoff recognized as summary/synthetic and non-actionable",
        project_persisted(standalone_handoff()),
    )

    # merged carrier, metadata stripped
    record(
        "projected_merged_carrier_classified_by_content",
        "a merged carrier is recognized by the post-delimiter prefix even without the marker",
        project_persisted(merged_carrier()),
    )

    # continuation placeholder row (marker content, no private flag)
    record(
        "continuation_placeholder_is_synthetic",
        "the zero-user continuation content is synthetic scaffolding, not a real human turn",
        user(COMPRESSION_CONTINUATION_USER_CONTENT),
    )
    record(
        "legacy_continuation_placeholder_is_synthetic",
        "the legacy continuation wording is still recognized as synthetic",
        user(_LEGACY_COMPRESSION_CONTINUATION_USER_CONTENT),
    )
    record(
        "max_iterations_request_is_synthetic",
        "the max-iterations summary request is runtime scaffolding, not user intent",
        user(MAX_ITERATIONS_SUMMARY_REQUEST),
    )
    record(
        "background_process_notification_is_synthetic",
        "a background-process notification prefix marks a synthetic user row",
        user("[IMPORTANT: Background process 1234 finished with exit code 0]"),
    )

    # a genuine human turn survives projection as real + actionable
    record(
        "real_user_turn_is_actionable",
        "an ordinary human question is real, actionable, and not synthetic",
        user("please port the compressor to Rust"),
    )

    # metadata-marked summary that carries role=user but empty-looking content
    marked = user("", **{MK: True})
    cases.append({
        "case": "metadata_marked_row_is_summary_pre_projection",
        "why": "before projection the private marker alone classifies a row as a summary and synthetic",
        "role": "user",
        "is_compaction_summary_message": is_compaction_summary_message(marked),
        "is_synthetic_compression_user_turn":
            ContextCompressor._is_synthetic_compression_user_turn(marked),
        "is_actionable_user_turn": ContextCompressor._is_actionable_user_turn(marked),
    })

    # transcript-level real-user detection: synthetic rows never count
    transcript_synthetic_only = [
        system(),
        project_persisted(standalone_handoff()),
        user(COMPRESSION_CONTINUATION_USER_CONTENT),
    ]
    transcript_with_real = transcript_synthetic_only + [user("do the real thing")]
    assert ContextCompressor._transcript_has_real_user_turn(transcript_synthetic_only) is False
    assert ContextCompressor._transcript_has_real_user_turn(transcript_with_real) is True
    cases.append({
        "case": "transcript_real_user_detection",
        "why": "a transcript of only handoff/continuation rows has no real user; a genuine turn flips it",
        "synthetic_only_has_real_user": False,
        "with_real_turn_has_real_user": True,
    })

    return cases


# ===========================================================================
# 4. summary role selection + collision behaviour  (runtime, lifted block)
# ===========================================================================
def role_selection_cases() -> List[Dict[str, Any]]:
    """Drive the real inline decision block with controlled head/tail role
    lists. Head/tail composition here is the *input* to the handoff lane; the
    token-tail selection that would produce it in production is AGY's lane and
    is not asserted."""
    U = user("a genuine user turn")
    A = assistant("an assistant reply")
    ATC = assistant("", tool_calls=[tool_call()])
    TL = tool()
    SYS = system()

    scenarios = [
        (
            "head_user_tail_assistant_collision_merges",
            "head ends visible user, tail opens assistant: summary would be assistant, collides with tail, flip back to user collides with head, so it merges into the tail",
            [SYS, U], [A], 1,
        ),
        (
            "head_assistant_tail_user_collision_merges",
            "head ends assistant, tail opens user: both standalone roles collide, so the summary merges into the tail carrier",
            [SYS, U, A], [U], 1,
        ),
        (
            "compress_start_zero_forces_user_leading",
            "compress_start==0 means the summary opens the request; it is pinned role=user (force-user-leading) and not flipped",
            [U], [A], 0,
        ),
        (
            "system_only_head_forces_user_leading",
            "when only the system prompt sits in the head the summary is the first visible message and is pinned role=user",
            [SYS], [A], 1,
        ),
        (
            "tool_flow_head_visible_role_is_user",
            "head literally ends [user, assistant(tool_calls), tool] but the template-visible last role is user, so the summary is assistant and does not collide with the user tail",
            [SYS, U, ATC, TL], [U], 1,
        ),
        (
            "all_exempt_head_opens_with_user",
            "an all-template-exempt head (tool flow only) gives last_head_role=None, so the summary must open the visible sequence as user",
            [ATC, TL], [A], 1,
        ),
        (
            "clean_alternation_summary_assistant",
            "head ends user, tail opens assistant-tool-exempt then user: summary is a clean standalone assistant with no collision",
            [SYS, U], [ATC, TL, U], 1,
        ),
        (
            "empty_tail_head_user_standalone_assistant",
            "no tail messages, head ends user: standalone summary is assistant, never merged",
            [SYS, U], [], 1,
        ),
    ]

    cases: List[Dict[str, Any]] = []
    for name, why, head, tail, cs in scenarios:
        decision = decide_summary_role(head, tail, cs)
        cases.append({
            "case": name,
            "why": why,
            "head_roles": [m.get("role") for m in head],
            "tail_roles": [m.get("role") for m in tail],
            "compress_start": cs,
            **decision,
        })
    return cases


# ===========================================================================
# 5. strip / unwrap old handoffs on re-compression  (runtime)
# ===========================================================================
def strip_unwrap_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    def record(name: str, why: str, message: Dict[str, Any]) -> None:
        result = ContextCompressor._strip_context_summary_handoff_message(message)
        if result is None:
            projected: Any = None
        else:
            projected = {
                "role": result.get("role"),
                "content": result.get("content"),
                "keeps_summary_marker": MK in result,
            }
        cases.append({"case": name, "why": why, "stripped": projected})

    # standalone handoff -> dropped entirely (None)
    record(
        "standalone_handoff_dropped",
        "a standalone reference handoff carries no live content, so re-compaction drops it",
        standalone_handoff(),
    )

    # merged carrier -> unwrapped to the genuine prior-tail content, marker cleared
    record(
        "merged_carrier_unwrapped_to_prior_content",
        "a merge-into-tail carrier keeps the real prior-tail content before the delimiter; the summary marker is cleared",
        merged_carrier(prior="the surviving live ask"),
    )

    # force-user-leading carrier -> remainder after the end marker survives
    record(
        "force_user_leading_carrier_keeps_live_ask",
        "a force-user-leading carrier keeps the live ask after the end marker; the marker is cleared",
        force_user_leading_carrier(ask="finish the port now"),
    )

    # list-content merged carrier -> unwrapped prior blocks
    list_merged = {
        "role": "user",
        "content": [
            _MERGED_PRIOR_CONTEXT_HEADER + "\nprior tail text\n\n" + _MERGED_SUMMARY_DELIMITER
            + "\n\n" + SUMMARY_PREFIX + "\nbody\n\n" + _SUMMARY_END_MARKER,
        ],
        MK: True,
    }
    record(
        "list_content_merged_carrier_unwrapped",
        "list-shaped merged content is unwrapped to the prior blocks before the delimiter",
        list_merged,
    )

    # a plain non-summary message -> returned as a copy unchanged
    plain = user("an ordinary human message")
    result = ContextCompressor._strip_context_summary_handoff_message(plain)
    assert result is not None and result == plain and result is not plain
    cases.append({
        "case": "non_summary_returned_as_copy",
        "why": "a non-summary row is returned unchanged (a copy), never dropped",
        "stripped": {"role": result.get("role"), "content": result.get("content"),
                     "keeps_summary_marker": MK in result},
        "is_distinct_object": result is not plain,
    })

    return cases


# ===========================================================================
# 6. continuation insertion + real-user anchor preservation  (runtime)
# ===========================================================================
def continuation_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # zero real user -> continuation placeholder appended
    compressed = [system(), assistant("only assistant work"), assistant("more work")]
    originals = [assistant("only assistant work"), assistant("more work")]
    outcome = _ensure_compressed_has_user_turn(originals, compressed)
    assert outcome == "placeholder_appended"
    assert compressed[-1]["content"] == COMPRESSION_CONTINUATION_USER_CONTENT
    cases.append({
        "case": "zero_real_user_appends_continuation_placeholder",
        "why": "with no real user turn to anchor, the deterministic continuation placeholder is appended",
        "outcome": outcome,
        "appended_content": compressed[-1]["content"],
        "appended_role": compressed[-1]["role"],
    })

    # a real user turn already present -> nothing inserted
    compressed = [system(), user("real ask survives"), assistant("a")]
    outcome = _ensure_compressed_has_user_turn([user("real ask survives")], compressed)
    assert outcome == "already_present"
    cases.append({
        "case": "real_user_present_no_insertion",
        "why": "a surviving real user turn needs no anchor restoration",
        "outcome": outcome,
        "final_len": len(compressed),
    })

    # only a synthetic handoff present -> the real user from originals is anchored
    compressed = [system(), project_persisted(standalone_handoff()), assistant("a")]
    originals = [user("the original real ask"), assistant("a")]
    outcome = _ensure_compressed_has_user_turn(originals, compressed)
    assert outcome == "inserted"
    anchored = any(
        isinstance(m, dict) and m.get("role") == "user"
        and m.get("content") == "the original real ask"
        for m in compressed
    )
    assert anchored
    cases.append({
        "case": "synthetic_only_anchors_real_user_from_originals",
        "why": "a handoff-only transcript re-anchors the last real user turn from the originals",
        "outcome": outcome,
        "real_user_anchored": anchored,
        "final_roles": [m.get("role") for m in compressed],
    })

    # trailing summary tail -> anchor appended after it (never merged into a summary)
    compressed = [system(), project_persisted(standalone_handoff())]
    originals = [user("the ask to re-anchor")]
    outcome = _ensure_compressed_has_user_turn(originals, compressed)
    assert outcome == "inserted"
    assert compressed[-1].get("content") == "the ask to re-anchor"
    cases.append({
        "case": "anchor_appended_after_trailing_summary",
        "why": "the real user anchor is appended after a trailing summary, never merged into it",
        "outcome": outcome,
        "final_roles": [m.get("role") for m in compressed],
        "last_content": compressed[-1].get("content"),
    })

    return cases


# ===========================================================================
# 7. reference_handoff_would_drive_next_model_call  (runtime)
# ===========================================================================
def reference_handoff_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []
    sole = standalone_handoff()

    def record(name: str, why: str, messages: List[Dict[str, Any]], expected: bool) -> None:
        result = reference_handoff_would_drive_next_model_call(messages)
        assert result is expected, f"{name}: {result} != {expected}"
        cases.append({
            "case": name, "why": why,
            "would_drive_next_model_call": result,
        })

    record(
        "sole_standalone_handoff_drives",
        "a sole standalone handoff after a completed turn would drive the next call by itself: suppress it",
        [system(), sole], True,
    )
    record(
        "trailing_real_user_does_not_drive",
        "an actionable real user turn after the handoff keeps the model call",
        [system(), sole, user("do the thing now")], False,
    )
    record(
        "trailing_tool_result_does_not_drive",
        "a tool result after the handoff means an in-flight exchange continues",
        [system(), sole, tool(content="mid-flight output")], False,
    )
    record(
        "pending_assistant_tool_calls_do_not_drive",
        "a pending assistant tool call after the handoff is a live in-flight exchange",
        [system(), sole, assistant("", tool_calls=[tool_call()])], False,
    )
    record(
        "trailing_continuation_placeholder_still_drives",
        "a synthetic continuation row after the handoff is not a real user turn, so the handoff still drives",
        [system(), sole, user(COMPRESSION_CONTINUATION_USER_CONTENT)], True,
    )
    record(
        "merged_carrier_with_live_ask_does_not_drive",
        "a merge-into-tail carrier still carries a live user ask, so it is not a sole-handoff driver",
        [system(), merged_carrier(prior="the live ask still here")], False,
    )
    record(
        "force_user_leading_carrier_with_live_ask_does_not_drive",
        "a force-user-leading carrier keeps the live ask after the end marker: not a sole driver",
        [system(), force_user_leading_carrier(ask="keep going")], False,
    )
    record(
        "merged_completed_assistant_carrier_drives",
        "a completed merged assistant carrier (finish_reason stop, no tool_calls) preserves prose only and drives the next call",
        [system(), merged_carrier(role="assistant", finish_reason="stop")], True,
    )
    record(
        "merged_assistant_carrier_with_pending_calls_does_not_drive",
        "a merged assistant carrier with pending tool_calls remains a live exchange, not a sole driver",
        [system(), merged_carrier(role="assistant", tool_calls=[tool_call()])], False,
    )
    record(
        "empty_transcript_does_not_drive",
        "an empty transcript has no handoff to drive the next call",
        [], False,
    )
    record(
        "no_handoff_does_not_drive",
        "a plain transcript with no handoff never drives from a summary",
        [system(), user("hi"), assistant("hello")], False,
    )

    return cases


# ---------------------------------------------------------------------------
# assembly
# ---------------------------------------------------------------------------
def build() -> Dict[str, Any]:
    return {
        "_meta": {
            "description": (
                "Golden oracle for the full-compression handoff layer (prefix / "
                "classification / role selection / strip-unwrap / continuation / "
                "reference-handoff drive). Every runtime value is the return of a "
                "real local function or the verbatim inline role-selection block "
                "executed against controlled inputs; no decision is reimplemented."
            ),
            "excluded_agy_lane": (
                "token-tail selection and min_tail_user_messages are AGY's "
                "independent lane; role-selection cases feed controlled head/tail "
                "role lists directly and never assert tail sizing. Todo snapshot "
                "insertion is out of scope."
            ),
            "role_selection_block_source": (
                "agent/context_compressor.py ContextCompressor.compress, inline "
                "summary role-selection block, compiled verbatim from source "
                f"(offsets {_ROLE_BLOCK_START}-{_ROLE_BLOCK_END} within the "
                "compress() source)."
            ),
            "authoritative_sources": [
                "agent/context_compressor.py: ContextCompressor._starts_with_summary_prefix"
                " / _strip_summary_prefix / _with_summary_prefix / classify_summary_content",
                "agent/context_compressor.py: ContextCompressor._is_context_summary_content"
                " / _is_synthetic_compression_user_turn / _is_actionable_user_turn"
                " / _transcript_has_real_user_turn",
                "agent/context_compressor.py: ContextCompressor.compress"
                " (inline summary role-selection decision block)",
                "agent/context_compressor.py: ContextCompressor._strip_context_summary_handoff_message",
                "agent/context_compressor.py: reference_handoff_would_drive_next_model_call"
                " / is_compaction_summary_message",
                "agent/conversation_compression.py: _ensure_compressed_has_user_turn"
                " / _insert_real_user_anchor / _is_real_user_message",
            ],
            "pure_constants_section": "constants",
        },
        "constants": constant_cases(),
        "prefix_recognition": prefix_recognition_cases(),
        "classification": classification_cases(),
        "role_selection": role_selection_cases(),
        "strip_unwrap": strip_unwrap_cases(),
        "continuation": continuation_cases(),
        "reference_handoff": reference_handoff_cases(),
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
