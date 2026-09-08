#!/usr/bin/env python3
"""Generate golden cases for the token-budget-aware tool-result prune.

This executes the *real* ``ContextCompressor._prune_old_tool_results`` and the
real per-message budget estimator ``_estimate_msg_budget_tokens`` from
``agent/context_compressor.py`` so the Rust port is checked against CPython
behaviour rather than a paraphrase.

Three sections are produced:

  * ``estimator`` - direct ``_estimate_msg_budget_tokens(msg, charge_stale_thinking)``
    calls. This is the primitive the boundary walk and the Pass-4 pressure loop
    both sum, so the Rust port must reproduce it exactly. Covers ASCII vs
    CJK vs mixed vs non-CJK-non-ASCII text, the full ``str(tool_call)`` envelope
    overhead (not just arguments), image/multimodal content at the 6400 char /
    1600 token equivalent, the always-replayed Codex sidecars
    (``codex_reasoning_items`` / ``codex_message_items``), and the
    newest-turn-only thinking keys (``reasoning`` / ``reasoning_content`` with
    the reasoning_content-wins dedup, plus ``reasoning_details`` text-only).

  * ``prune`` - full ``_prune_old_tool_results(messages, protect_tail_count,
    protect_tail_tokens, min_prune_chars)`` calls. Covers the strict ``>``
    boundary equality, the eight-message floor cap
    (``_MAX_TAIL_MESSAGE_FLOOR``), the all-content-fits (boundary 0) case, the
    newest-only vs stale-thinking charge (generic route vs a deepseek echo-back
    route), Codex sidecar weight moving the boundary, the Pass-4 pressure
    cascade (stages 4a / 4b / 4c), the ghost-skill spare and its Pass-4
    override, and the stale ``api_content`` sidecar drop on image demotion.

  * ``tail_cut`` - direct ``_find_tail_cut_by_tokens`` cases for the summary
    boundary, including full tool-call-envelope weight, the raw-budget
    fallback, tool-group alignment, and latest user/assistant anchors.

The compressor is built with ``get_model_context_length`` patched to a fixed
window (mirroring ``tests/agent/test_context_compressor.py``) so construction
never touches the network and no environment-dependent field enters a golden.
Message inputs carry no timestamps.

Budgets for the equality / floor cases are computed FROM the real estimator so
the case is robust to estimator tweaks, then frozen to a concrete integer in the
golden; the Rust test replays that same integer.

Run from the repository root with the project virtualenv:

    .venv/bin/python rust/tools/token-budget-prune-oracle.py

Writes ``rust/tools/token-budget-prune-goldens.json`` next to this script. Pass
``--check`` to verify the checked-in fixture still matches Python.
"""
from __future__ import annotations

import copy
import json
import sys
from pathlib import Path
from typing import Any, Dict, List, Tuple
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent / "token-budget-prune-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

import agent.context_compressor as cc  # noqa: E402
from agent.context_compressor import _estimate_msg_budget_tokens  # noqa: E402

FIXED_WINDOW = 100_000


def make_compressor(
    model: str = "test/model",
    provider: str = "",
    api_mode: str = "",
    base_url: str = "",
):
    """Build a ContextCompressor without touching the network.

    Only the routing fields (model/provider/api_mode/base_url) matter to
    ``_prune_old_tool_results``: they feed ``_stale_thinking_on_wire`` which
    decides the newest-turn-only vs charge-every-turn thinking policy.
    """
    with patch.object(cc, "get_model_context_length", return_value=FIXED_WINDOW):
        c = cc.ContextCompressor(
            model=model,
            provider=provider,
            api_mode=api_mode,
            base_url=base_url,
            quiet_mode=True,
        )
        _ = c.context_length  # resolve while the patch is active
        return c


GENERIC = make_compressor(model="test/model")
# A deepseek route makes _stale_thinking_on_wire() True (echo-back family), so
# the boundary walk charges reasoning/reasoning_content on EVERY assistant turn
# instead of only the newest one.
DEEPSEEK = make_compressor(model="deepseek-chat", provider="deepseek")

assert GENERIC._stale_thinking_on_wire() is False
assert DEEPSEEK._stale_thinking_on_wire() is True


# ---------------------------------------------------------------------------
# message builders
# ---------------------------------------------------------------------------
def user(text: str) -> Dict[str, Any]:
    return {"role": "user", "content": text}


def call(cid: str, name: str, args: str) -> Dict[str, Any]:
    return {"id": cid, "type": "function", "function": {"name": name, "arguments": args}}


def assistant(content: str = "", tool_calls: List[Dict[str, Any]] | None = None,
              **extra: Any) -> Dict[str, Any]:
    msg: Dict[str, Any] = {"role": "assistant", "content": content}
    if tool_calls is not None:
        msg["tool_calls"] = tool_calls
    msg.update(extra)
    return msg


def tool(cid: str, content: Any, **extra: Any) -> Dict[str, Any]:
    msg: Dict[str, Any] = {"role": "tool", "tool_call_id": cid, "content": content}
    msg.update(extra)
    return msg


def image_part() -> Dict[str, Any]:
    return {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}


# ---------------------------------------------------------------------------
# estimator cases
# ---------------------------------------------------------------------------
def estimator_cases() -> List[Dict[str, Any]]:
    cases: List[Tuple[str, Dict[str, Any], bool]] = []

    # Plain ASCII string content: (len+3)//4 + 10.
    cases.append(("ascii-string", {"role": "tool", "content": "A" * 400}, True))
    # CJK-dense: each codepoint counts as ~1 token (100 chars -> 100 + 10).
    cases.append(("cjk-dense", {"role": "tool", "content": "漢" * 100}, True))
    # Mixed CJK + ASCII: dense chars 1 each, ASCII remainder byte-counted /4.
    cases.append(("mixed-cjk-ascii",
                  {"role": "tool", "content": "漢字 hello world " * 5}, True))
    # Non-ASCII, non-CJK (Cyrillic): UTF-8 byte length /4, not char length.
    cases.append(("cyrillic-bytes",
                  {"role": "user", "content": "привет " * 20}, True))
    # Emoji (non-ASCII, non-CJK): byte counted.
    cases.append(("emoji-bytes", {"role": "user", "content": "\U0001f600" * 30}, True))
    # Empty content still charges the +10 role/key overhead.
    cases.append(("empty-content", {"role": "tool", "content": ""}, True))

    # Tool-call envelope: the FULL str(tool_call) is charged, not just the args.
    small_args = '{"command":"ls"}'
    cases.append(("tool-call-overhead",
                  {"role": "assistant", "content": "",
                   "tool_calls": [call("c1", "terminal", small_args)]}, True))
    # Two parallel tool calls: both envelopes charged.
    cases.append(("tool-call-parallel",
                  {"role": "assistant", "content": "",
                   "tool_calls": [call("c1", "terminal", small_args),
                                  call("c2", "read_file", '{"path":"a.py"}')]}, True))

    # Image content-part list: image counts as 6400 chars (1600 tokens).
    cases.append(("image-part-list",
                  {"role": "tool", "content": [image_part(),
                                               {"type": "text", "text": "caption"}]}, True))
    # Native multimodal envelope dict is measured via str() length fallback.
    cases.append(("multimodal-envelope",
                  {"role": "tool", "content": {"_multimodal": True,
                                               "content": [image_part()],
                                               "text_summary": "a screenshot"}}, True))

    # Codex sidecars are ALWAYS charged regardless of charge_stale_thinking.
    codex_msg = {"role": "assistant", "content": "ok",
                 "codex_reasoning_items": [{"encrypted_content": "Z" * 200}],
                 "codex_message_items": [{"id": "m1"}]}
    cases.append(("codex-sidecar-charged", codex_msg, True))
    cases.append(("codex-sidecar-charged-newest-off", codex_msg, False))

    # reasoning key: charged only when charge_stale_thinking is True.
    reasoning_msg = {"role": "assistant", "content": "answer",
                     "reasoning": "R" * 400}
    cases.append(("reasoning-charged", reasoning_msg, True))
    cases.append(("reasoning-skipped-when-not-newest", reasoning_msg, False))

    # reasoning_content wins: when both present, reasoning is skipped to avoid
    # double-charging the same thinking text.
    both_msg = {"role": "assistant", "content": "answer",
                "reasoning": "R" * 400, "reasoning_content": "C" * 120}
    cases.append(("reasoning-content-wins", both_msg, True))

    # reasoning_details: only the thinking TEXT is charged, never the signed
    # envelope; only when neither reasoning nor reasoning_content is present.
    details_msg = {"role": "assistant", "content": "answer",
                   "reasoning_details": [
                       {"type": "reasoning.text", "text": "T" * 160,
                        "signature": "S" * 1000}]}
    cases.append(("reasoning-details-text-only", details_msg, True))
    cases.append(("reasoning-details-skipped-when-not-newest", details_msg, False))

    out = []
    for name, msg, charge in cases:
        expected = _estimate_msg_budget_tokens(msg, charge_stale_thinking=charge)
        out.append({
            "name": name,
            "charge_stale_thinking": charge,
            "message": msg,
            "expected_tokens": expected,
        })
    return out


# ---------------------------------------------------------------------------
# prune helpers
# ---------------------------------------------------------------------------
def tail_tokens(messages: List[Dict[str, Any]], start: int, charge_all: bool,
                newest_asst_idx: int) -> int:
    """Sum estimator tokens over messages[start:] the way the walk charges them."""
    total = 0
    for i in range(start, len(messages)):
        total += _estimate_msg_budget_tokens(
            messages[i],
            charge_stale_thinking=(charge_all or i == newest_asst_idx),
        )
    return total


def newest_assistant_index(messages: List[Dict[str, Any]]) -> int:
    idx = -1
    for i, m in enumerate(messages):
        if m.get("role") == "assistant":
            idx = i
    return idx


def run_prune(comp, messages, protect_tail_count, protect_tail_tokens, min_prune_chars):
    """Call the real prune on a deep copy and return (output, pruned_count)."""
    src = copy.deepcopy(messages)
    out, n = comp._prune_old_tool_results(
        src, protect_tail_count,
        protect_tail_tokens=protect_tail_tokens,
        min_prune_chars=min_prune_chars,
    )
    # Input immutability contract: the passed list's dicts are copied inside.
    return out, n


def prune_case(name: str, comp, messages, protect_tail_count,
               protect_tail_tokens, min_prune_chars=cc._PRUNE_MIN_CHARS,
               route: Dict[str, str] | None = None) -> Dict[str, Any]:
    out, n = run_prune(comp, messages, protect_tail_count, protect_tail_tokens,
                       min_prune_chars)
    return {
        "name": name,
        "route": route or {},
        "protect_tail_count": protect_tail_count,
        "protect_tail_tokens": protect_tail_tokens,
        "min_prune_chars": min_prune_chars,
        "input": copy.deepcopy(messages),
        "expected_output": out,
        "expected_pruned": n,
    }


def blob(seed: str, n: int) -> str:
    """A distinct ASCII tool body of ~n chars (distinct so Pass 1 never dedups)."""
    head = f"[{seed}] "
    return head + ("x" * max(0, n - len(head)))


def comp_tail_sum(comp, messages: List[Dict[str, Any]], start: int) -> int:
    """Exact estimator sum of messages[start:] under comp's stale-thinking policy."""
    return tail_tokens(
        messages,
        start,
        charge_all=comp._stale_thinking_on_wire(),
        newest_asst_idx=newest_assistant_index(messages),
    )


# ---------------------------------------------------------------------------
# prune cases
# ---------------------------------------------------------------------------
def prune_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    # --- boundary equality (strict >) -------------------------------------
    # The walk breaks on ``accumulated + msg > budget`` (strict ``>``), so the
    # message whose inclusion makes the running total EXACTLY equal to the
    # budget stays protected; one token less flips it. The swing message here
    # is a modest tool body (index 1) sitting just behind a long, light tail.
    # The tail is kept light so the protected region stays under the Pass-4
    # soft ceiling (1.5x budget) and pressure demotion never re-prunes the
    # protected tool, isolating the boundary behaviour.
    eq_msgs: List[Dict[str, Any]] = [
        assistant("", [call("c0", "read_file", '{"path":"a.py"}')]),
        tool("c0", blob("swing", 260)),
    ]
    for i in range(16):
        eq_msgs.append(user(f"light follow-up number {i}"))
    # Budget == exact generic sum of the light tail (indices 2..end). At the
    # boundary walk the index-1 tool is the message that makes the total equal.
    eq_budget = comp_tail_sum(GENERIC, eq_msgs, start=2)
    eq_hit = prune_case("boundary-equality-exact", GENERIC, eq_msgs,
                        protect_tail_count=2, protect_tail_tokens=eq_budget)
    eq_miss = prune_case("boundary-equality-minus-one", GENERIC, eq_msgs,
                         protect_tail_count=2, protect_tail_tokens=eq_budget - 1)
    # Equal keeps the index-1 tool protected (nothing prunes); one less drops
    # it out of the protected region so it demotes.
    assert eq_hit["expected_pruned"] == 0, eq_hit["expected_pruned"]
    assert eq_hit["expected_output"][1]["content"] == eq_msgs[1]["content"]
    assert eq_miss["expected_pruned"] == 1, eq_miss["expected_pruned"]
    assert eq_miss["expected_output"][1]["content"].startswith("[read_file]"), \
        eq_miss["expected_output"][1]["content"]
    cases.append(eq_hit)
    cases.append(eq_miss)

    # --- eight-message floor cap ------------------------------------------
    # protect_tail_count=20 is capped at _MAX_TAIL_MESSAGE_FLOOR (8), so on a
    # 12-message transcript only the last 8 are protected and the two bulky
    # tool bodies in the head demote. Without the cap, count=20 would protect
    # all 12 and prune nothing. The 8 protected messages are light user rows,
    # so Pass 4 runs but finds nothing to demote.
    floor_msgs: List[Dict[str, Any]] = [
        assistant("", [call("f0", "read_file", '{"path":"a.py"}')]),
        tool("f0", blob("head0", 800)),
        assistant("", [call("f1", "read_file", '{"path":"b.py"}')]),
        tool("f1", blob("head1", 800)),
    ]
    for i in range(8):
        floor_msgs.append(user(f"recent question {i}"))  # len == 12
    floor = prune_case("eight-message-floor-cap", GENERIC, floor_msgs,
                       protect_tail_count=20, protect_tail_tokens=50)
    # Boundary = len - 8 = 4, so the two head tools (indices 1, 3) demote.
    assert floor["expected_pruned"] == 2, floor["expected_pruned"]
    assert floor["expected_output"][1]["content"].startswith("[read_file]")
    assert floor["expected_output"][3]["content"].startswith("[read_file]")
    # The protected tail (light user rows) is untouched.
    assert floor["expected_output"][4:] == floor_msgs[4:]
    cases.append(floor)

    # --- all content fits (boundary 0) ------------------------------------
    # A budget larger than the whole transcript leaves prune_boundary at 0:
    # nothing is demoted and the output is identical to the input.
    fit_msgs = [
        user("hi"),
        assistant("", [call("g0", "read_file", '{"path":"a.py"}')]),
        tool("g0", blob("a", 800)),
        user("thanks"),
    ]
    fit = prune_case("all-content-fits", GENERIC, fit_msgs,
                     protect_tail_count=2, protect_tail_tokens=10_000_000)
    assert fit["expected_pruned"] == 0
    assert fit["expected_output"] == fit["input"]
    cases.append(fit)

    # --- newest-only vs stale-thinking charging ---------------------------
    # A STALE assistant turn (index 2, not the newest) carries a reasoning
    # blob. The generic route charges thinking only on the newest assistant
    # turn, so that blob is free in the tail walk and the budget reaches
    # exactly to index 2, keeping the older tool (index 1) protected. The
    # deepseek echo-back route charges the same blob on every turn, so the
    # budget is exhausted one message sooner, the boundary lands at index 2,
    # and the index-1 tool demotes. The protected region carries no other tool
    # body, so Pass 4 has nothing to re-prune and cannot mask the difference.
    think_msgs: List[Dict[str, Any]] = [
        user("start"),
        tool("t0", blob("swing", 260)),          # older tool (the swing)
        assistant("", reasoning="R" * 400),      # STALE reasoning turn
        assistant("acknowledged"),               # newest assistant turn
    ]
    for i in range(26):
        think_msgs.append(user(f"light tail row {i}"))
    # Budget == exact generic sum from index 2 to the end (the stale reasoning
    # is not charged, so index 2 lands on the equality boundary).
    think_budget = comp_tail_sum(GENERIC, think_msgs, start=2)
    think_generic = prune_case("stale-thinking-newest-only-generic", GENERIC,
                               think_msgs, protect_tail_count=2,
                               protect_tail_tokens=think_budget,
                               route={"model": "test/model"})
    think_deepseek = prune_case("stale-thinking-charge-every-turn-deepseek",
                                DEEPSEEK, think_msgs, protect_tail_count=2,
                                protect_tail_tokens=think_budget,
                                route={"model": "deepseek-chat",
                                       "provider": "deepseek"})
    assert think_generic["expected_pruned"] == 0, think_generic["expected_pruned"]
    assert think_generic["expected_output"][1]["content"] == think_msgs[1]["content"]
    assert think_deepseek["expected_pruned"] == 1, think_deepseek["expected_pruned"]
    assert think_deepseek["expected_output"][1]["content"].startswith("[unknown]") \
        or "chars" in think_deepseek["expected_output"][1]["content"]
    cases.append(think_generic)
    cases.append(think_deepseek)

    # --- Codex replay sidecars move the boundary --------------------------
    # codex_reasoning_items / codex_message_items are charged on EVERY retained
    # turn. The same transcript with the sidecars present fills the tail budget
    # faster, pushing the boundary earlier and pruning more than the sidecar-
    # free control at an identical budget.
    codex_present = [
        user("start"),
        assistant("ok", codex_reasoning_items=[{"encrypted_content": "Z" * 2400}],
                  codex_message_items=[{"id": "m1"}],
                  tool_calls=[call("k0", "read_file", '{"path":"a.py"}')]),
        tool("k0", blob("a", 800)),
        assistant("ok", [call("k1", "read_file", '{"path":"b.py"}')]),
        tool("k1", blob("b", 800)),
        user("continue"),
    ]
    codex_absent = copy.deepcopy(codex_present)
    codex_absent[1].pop("codex_reasoning_items")
    codex_absent[1].pop("codex_message_items")
    codex_budget = comp_tail_sum(GENERIC, codex_absent, start=2)
    codex_on = prune_case("codex-sidecar-fills-tail", GENERIC, codex_present,
                          protect_tail_count=2, protect_tail_tokens=codex_budget)
    codex_off = prune_case("codex-sidecar-absent-control", GENERIC, codex_absent,
                           protect_tail_count=2, protect_tail_tokens=codex_budget)
    assert codex_on["expected_pruned"] > codex_off["expected_pruned"], (
        codex_off["expected_pruned"], codex_on["expected_pruned"])
    cases.append(codex_on)
    cases.append(codex_off)

    # --- Pass 4 stage 4a: pressure loop inside the protected region -------
    # Everything is inside the protected tail (prune_boundary 0), but the
    # protected region blows the soft ceiling (1.5x budget). Stage 4a walks
    # [prune_boundary, len - keep_recent) demoting bulky tool bodies until the
    # region fits, keeping the last _PRESSURE_KEEP_RECENT_MESSAGES verbatim.
    p4a_msgs = [
        tool("a0", blob("r0", 2000)),
        tool("a1", blob("r1", 2000)),
        tool("a2", blob("r2", 2000)),
        user("q1"),
        user("q2"),
        user("q3"),
    ]
    p4a = prune_case("pass4a-pressure-loop", GENERIC, p4a_msgs,
                     protect_tail_count=8, protect_tail_tokens=700)
    # keep_recent=3 -> the loop can only touch indices 0..2 (all tools). It
    # stops early once the region fits, so at least one but not all demote.
    demoted_head = sum(
        1 for i in range(3)
        if p4a["expected_output"][i]["content"] != p4a_msgs[i]["content"]
    )
    assert 1 <= demoted_head <= 3, demoted_head
    # The recent floor (last three user rows) is never rewritten.
    assert p4a["expected_output"][3:] == p4a_msgs[3:]
    cases.append(p4a)

    # --- Pass 4 stage 4b: demote all protected tools except the newest ----
    # The bulky tools live INSIDE the keep-recent floor, so the 4a loop cannot
    # reach them. Stage 4b then demotes every protected tool result except the
    # single most recent one.
    p4b_msgs = [
        user("q1"),
        user("q2"),
        user("q3"),
        tool("b0", blob("r0", 2000)),
        tool("b1", blob("r1", 2000)),
        tool("b2", blob("r2", 2000)),
    ]
    p4b = prune_case("pass4b-demote-all-but-newest", GENERIC, p4b_msgs,
                     protect_tail_count=8, protect_tail_tokens=400)
    assert p4b["expected_output"][3]["content"] != p4b_msgs[3]["content"]
    assert p4b["expected_output"][4]["content"] != p4b_msgs[4]["content"]
    # The newest tool body (index 5) survives 4b.
    assert p4b["expected_output"][5]["content"] == p4b_msgs[5]["content"]
    cases.append(p4b)

    # --- Pass 4 stage 4c: last-resort demote the newest tool --------------
    # A single tool body larger than the whole soft ceiling remains as the
    # newest tool. After 4b spares it, stage 4c demotes even that last body so
    # compression can still reclaim headroom.
    p4c_msgs = [
        user("q1"),
        user("q2"),
        user("q3"),
        user("q4"),
        user("q5"),
        tool("c0", blob("huge", 8000)),
    ]
    p4c = prune_case("pass4c-last-resort-newest-tool", GENERIC, p4c_msgs,
                     protect_tail_count=8, protect_tail_tokens=400)
    assert p4c["expected_output"][5]["content"] != p4c_msgs[5]["content"]
    assert p4c["expected_output"][5]["content"].startswith("[read_file]") is False
    assert p4c["expected_pruned"] == 1
    cases.append(p4c)

    # --- protected skill: spared by normal prune, overridden by Pass 4 ----
    # A skill_view body referenced by a user message in the protected tail is
    # spared by the ordinary Pass 2 demotion even though it sits before the
    # boundary. Other tool bodies in the same head still demote.
    skill_body = blob("acid skill contents", 3000)
    skill_msgs: List[Dict[str, Any]] = [
        user("open the acid skill"),
        assistant("", [call("s0", "skill_view",
                            '{"name":"acid-transactions"}')]),
        tool("s0", skill_body),
        assistant("", [call("s1", "read_file", '{"path":"a.py"}')]),
        tool("s1", blob("a", 3000)),
    ]
    # Pad so the skill call sits OUTSIDE the recent window and the boundary,
    # forcing protection to come from the tail user mention only.
    for i in range(9):
        skill_msgs.append(user(f"filler turn {i}"))
    skill_msgs.append(user("remember the acid-transactions rules"))  # tail mention
    boundary_count = 2  # protect only the last two messages -> prune_boundary large
    skill_spared = prune_case("protected-skill-spared-normal-prune", GENERIC,
                              skill_msgs, protect_tail_count=boundary_count,
                              protect_tail_tokens=None)
    # skill_view body at index 2 kept verbatim; the plain read_file at index 4
    # is demoted.
    assert skill_spared["expected_output"][2]["content"] == skill_body
    assert skill_spared["expected_output"][4]["content"] != skill_msgs[4]["content"]
    cases.append(skill_spared)

    # Same skill body, but now the protected region blows the soft ceiling so
    # Pass 4 pressure demotion overrides the skill spare (spare_protected_skills
    # is False under pressure).
    skill_pressure_msgs = [
        user("open the acid skill"),
        assistant("", [call("s0", "skill_view",
                            '{"name":"acid-transactions"}')]),
        tool("s0", skill_body),
        user("remember the acid-transactions rules"),
    ]
    skill_override = prune_case("protected-skill-pass4-override", GENERIC,
                                skill_pressure_msgs, protect_tail_count=8,
                                protect_tail_tokens=200)
    assert skill_override["expected_output"][2]["content"] != skill_body, \
        "pressure demotion must override the skill spare"
    cases.append(skill_override)

    # --- stale api_content dropped on image demotion ----------------------
    # Older image-bearing tool results are retired by Pass 3.5 (keep newest 3).
    # The rewrite drops each retired message's api_content sidecar so replay
    # cannot resend the pre-strip bytes; the newest three keep image + sidecar.
    img_msgs: List[Dict[str, Any]] = [user("look at these")]
    for i in range(5):
        img_msgs.append(assistant("", [call(f"i{i}", "vision_analyze",
                                            '{"kind":"screenshot"}')]))
        img_msgs.append(tool(f"i{i}",
                             [image_part(), {"type": "text", "text": f"frame {i}"}],
                             api_content=f"WIRE-BYTES-{i}"))
    img_msgs.append(user("what changed"))
    img = prune_case("stale-api-content-image-demotion", GENERIC, img_msgs,
                     protect_tail_count=20, protect_tail_tokens=None)
    # Tool result indices: 2,4,6,8,10 (i0..i4). keep_newest=3 -> i0,i1 retired.
    retired = img["expected_output"]
    assert "api_content" not in retired[2] and "api_content" not in retired[4], \
        "retired image messages must lose their stale api_content sidecar"
    assert retired[6].get("api_content") == "WIRE-BYTES-2"
    assert retired[10].get("api_content") == "WIRE-BYTES-4"
    # The retired bodies are text-only now (image part replaced by a placeholder).
    assert not _tool_content_has_images(retired[2]["content"])
    assert _tool_content_has_images(retired[10]["content"])
    cases.append(img)

    return cases


from agent.context_compressor import _tool_content_has_images  # noqa: E402


# ---------------------------------------------------------------------------
# summary tail-cut cases
# ---------------------------------------------------------------------------
def tail_cut_cases() -> List[Dict[str, Any]]:
    cases: List[Dict[str, Any]] = []

    heavy = []
    for i in range(20):
        heavy.append(assistant("", [
            call(f"call_{i:02d}_{'a' * 24}", "read_file", '{"path":"a"}')
            for _ in range(5)
        ]))
    heavy_messages = [user("start")] + heavy
    per_message = _estimate_msg_budget_tokens(heavy_messages[-1])
    heavy_budget = int(per_message * 6 / 1.5)
    cases.append({
        "name": "parallel-tool-call-envelope",
        "route": {"model": "test/model"},
        "protect_last_n": 20,
        "head_end": 1,
        "token_budget": heavy_budget,
        "input": heavy_messages,
        "expected_cut": GENERIC._find_tail_cut_by_tokens(
            heavy_messages, 1, heavy_budget),
    })

    alternating = [
        {"role": "user" if i % 2 == 0 else "assistant", "content": f"m{i}"}
        for i in range(12)
    ]
    cases.append({
        "name": "all-fit-raw-budget-fallback",
        "route": {"model": "test/model"},
        "protect_last_n": 20,
        "head_end": 2,
        "token_budget": 1_000_000,
        "input": alternating,
        "expected_cut": GENERIC._find_tail_cut_by_tokens(
            alternating, 2, 1_000_000),
    })

    grouped = [
        user("u0"), assistant("a0"), user("u1"),
        assistant("", [call("c", "read_file", "{}")]),
        tool("c", "x" * 1000), assistant("final"),
        user("u2"), assistant("a2"),
    ]
    cases.append({
        "name": "tool-group-and-latest-reply-anchors",
        "route": {"model": "test/model"},
        "protect_last_n": 20,
        "head_end": 2,
        "token_budget": 20,
        "input": grouped,
        "expected_cut": GENERIC._find_tail_cut_by_tokens(grouped, 2, 20),
    })
    return cases


# ---------------------------------------------------------------------------
# assembly
# ---------------------------------------------------------------------------
def build() -> Dict[str, Any]:
    return {
        "source": "agent/context_compressor.py",
        "notes": (
            "Generated by rust/tools/token-budget-prune-oracle.py from the real "
            "ContextCompressor. Do not hand-edit; regenerate instead."
        ),
        "constants": {
            "chars_per_token": cc._CHARS_PER_TOKEN,
            "role_key_overhead_tokens": 10,
            "image_token_estimate": cc._IMAGE_TOKEN_ESTIMATE,
            "image_char_equivalent": cc._IMAGE_CHAR_EQUIVALENT,
            "prune_min_chars": cc._PRUNE_MIN_CHARS,
            "max_tail_message_floor": cc._MAX_TAIL_MESSAGE_FLOOR,
            "pressure_keep_recent_messages": cc._PRESSURE_KEEP_RECENT_MESSAGES,
            "max_keep_tool_images": cc._MAX_KEEP_TOOL_IMAGES,
            "skill_prune_recent_window": cc._SKILL_PRUNE_RECENT_WINDOW,
            "pruned_tool_placeholder": cc._PRUNED_TOOL_PLACEHOLDER,
        },
        "estimator": estimator_cases(),
        "prune": prune_cases(),
        "tail_cut": tail_cut_cases(),
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
              f"({len(data['estimator'])} estimator + {len(data['prune'])} prune + "
              f"{len(data['tail_cut'])} tail-cut cases).")
        return 0
    OUT.write_text(rendered, encoding="utf-8")
    print(f"Wrote {OUT.relative_to(REPO_ROOT)} "
          f"({len(data['estimator'])} estimator + {len(data['prune'])} prune + "
          f"{len(data['tail_cut'])} tail-cut cases).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
