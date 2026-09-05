#!/usr/bin/env python3
"""Generate golden cases for the thinking-only repair sanitizer.

This executes the *real* CPython implementations so the Rust port is checked
against source behaviour, not a paraphrase. Three definitions are involved:

  * ``AIAgent._is_thinking_only_assistant``  (``run_agent.py``)  : decides
    whether one assistant turn carries only reasoning and no visible output.
  * ``drop_thinking_only_and_merge_users``   (``agent/agent_runtime_helpers.py``)
    : drops those turns from the wire copy and merges any adjacent user
    messages left behind.
  * ``has_compaction_checkpoint``            (``agent/native_compaction.py``)
    : the dependency that makes a native-compaction carrier never count as
    thinking-only.

Importing ``run_agent`` outright would drag in the whole agent runtime
(httpx, provider profiles, ...), so each definition is pulled out of its
source file with ``ast`` and exec'd into one namespace. The wiring mirrors
production: ``drop_thinking_only_and_merge_users`` reaches the detector via a
``_ra()`` accessor, and the detector lazily imports ``has_compaction_checkpoint``
from ``agent.native_compaction`` : resolved here by a stub module so no real
import runs. No behaviour is reimplemented; only glue (a fake ``_ra`` and a
no-op logger) is supplied.

Run with the pinned interpreter:

    mise x python@3.12.13 -- python rust/tools/gen_thinking_repair_goldens.py

Add ``--check`` to assert the committed JSON still matches source output.
Writes ``rust/tools/thinking-repair-goldens.json`` next to this script.
"""
from __future__ import annotations

import ast
import copy
import json
import sys
import types
from pathlib import Path
from typing import Any, Callable, Dict, List

REPO_ROOT = Path(__file__).resolve().parents[2]
RUN_AGENT = REPO_ROOT / "run_agent.py"
HELPERS = REPO_ROOT / "agent" / "agent_runtime_helpers.py"
NATIVE_COMPACTION = REPO_ROOT / "agent" / "native_compaction.py"
OUT = Path(__file__).resolve().parent / "thinking-repair-goldens.json"


def _extract_module_func(source: Path, name: str) -> str:
    """Return the unparsed source of a top-level ``def name`` in ``source``."""
    tree = ast.parse(source.read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name == name:
            node.decorator_list = []
            return ast.unparse(node)
    raise SystemExit(f"could not find function {name!r} in {source}")


def _extract_method(source: Path, cls: str, name: str) -> str:
    """Return the unparsed source of ``cls.name``, stripped of decorators.

    ``_is_thinking_only_assistant`` is a ``@staticmethod``; dropping the
    decorator turns it into a plain function whose first parameter is the
    message dict, which is exactly how the sanitizer calls it.
    """
    tree = ast.parse(source.read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and node.name == cls:
            for item in node.body:
                if isinstance(item, ast.FunctionDef) and item.name == name:
                    item.decorator_list = []
                    return ast.unparse(item)
    raise SystemExit(f"could not find {cls}.{name} in {source}")


def load_oracle() -> Callable[[List[Any], bool], List[Any]]:
    """Wire the three real definitions together and return the sanitizer.

    Returns a callable ``(messages, drop_codex_reasoning_items) -> result``.
    """
    typing_ns: Dict[str, Any] = {"Any": Any, "Dict": Dict, "List": List}

    # 1. has_compaction_checkpoint -> a standalone stub module so the
    #    detector's lazy `from agent.native_compaction import ...` resolves
    #    without importing the real (runtime-heavy) package.
    hcc_src = _extract_module_func(NATIVE_COMPACTION, "has_compaction_checkpoint")
    nc_stub = types.ModuleType("agent.native_compaction")
    nc_stub.__dict__.update(typing_ns)
    exec(hcc_src, nc_stub.__dict__)  # noqa: S102 - trusted local source
    agent_pkg = types.ModuleType("agent")
    agent_pkg.__path__ = []  # mark as a package so submodule lookup works
    agent_pkg.native_compaction = nc_stub  # type: ignore[attr-defined]
    sys.modules.setdefault("agent", agent_pkg)
    sys.modules["agent.native_compaction"] = nc_stub

    # 2. _is_thinking_only_assistant -> plain function.
    detector_src = _extract_method(
        RUN_AGENT, "AIAgent", "_is_thinking_only_assistant"
    )
    detector_ns: Dict[str, Any] = dict(typing_ns)
    exec(detector_src, detector_ns)  # noqa: S102 - trusted local source
    detector = detector_ns["_is_thinking_only_assistant"]

    # 3. drop_thinking_only_and_merge_users -> reaches the detector and a
    #    logger through `_ra()`; supply a fake module carrying both.
    fake_ra = types.SimpleNamespace(
        AIAgent=types.SimpleNamespace(_is_thinking_only_assistant=detector),
        logger=types.SimpleNamespace(debug=lambda *a, **k: None),
    )
    sanitizer_src = _extract_module_func(HELPERS, "drop_thinking_only_and_merge_users")
    sanitizer_ns: Dict[str, Any] = dict(typing_ns)
    sanitizer_ns["_ra"] = lambda: fake_ra
    exec(sanitizer_src, sanitizer_ns)  # noqa: S102 - trusted local source
    sanitizer = sanitizer_ns["drop_thinking_only_and_merge_users"]

    def run(messages: List[Any], drop_codex_reasoning_items: bool) -> List[Any]:
        return sanitizer(
            messages, drop_codex_reasoning_items=drop_codex_reasoning_items
        )

    return run


# --- Case fixtures -----------------------------------------------------------
# Each case is (name, messages, drop_codex_reasoning_items). The expected
# output is computed by the real oracle, never hand-written.

def _user(content: Any) -> Dict[str, Any]:
    return {"role": "user", "content": content}


def _asst(**extra: Any) -> Dict[str, Any]:
    m: Dict[str, Any] = {"role": "assistant"}
    m.update(extra)
    return m


def cases() -> List[Dict[str, Any]]:
    out: List[Dict[str, Any]] = []

    def add(name: str, messages: List[Any], drop: bool = True) -> None:
        out.append({"name": name, "messages": messages, "drop": drop})

    # -- detector: thinking-only TRUE (turn is dropped) -----------------------
    # Wrapped between two users so the drop is observable and a merge follows.
    def wrapped(name: str, asst: Dict[str, Any], drop: bool = True) -> None:
        add(name, [_user("before"), asst, _user("after")], drop)

    wrapped("reasoning-string-null-content",
            _asst(content=None, reasoning="pondering"))
    wrapped("reasoning-content-empty-string-content",
            _asst(content="", reasoning_content="pondering"))
    wrapped("reasoning-whitespace-content",
            _asst(content="   \n\t ", reasoning="pondering"))
    wrapped("reasoning-empty-list-content",
            _asst(content=[], reasoning="pondering"))
    wrapped("thinking-block-content",
            _asst(content=[{"type": "thinking", "thinking": "hmm"}],
                  reasoning="pondering"))
    wrapped("redacted-thinking-block-content",
            _asst(content=[{"type": "redacted_thinking"}], reasoning="x"))
    wrapped("blank-text-block-content",
            _asst(content=[{"type": "text", "text": "   "}], reasoning="x"))
    wrapped("thinking-plus-blank-text-blocks",
            _asst(content=[{"type": "thinking", "thinking": "h"},
                           {"type": "text", "text": ""}], reasoning="x"))
    # Prefill precedence: checked before content, so visible text still drops.
    wrapped("prefill-flag-beats-visible-content",
            _asst(content="visible", _thinking_prefill=True))
    wrapped("prefill-flag-null-content",
            _asst(content=None, _thinking_prefill=True))
    # reasoning precedence: reasoning_content OR reasoning.
    wrapped("reasoning-content-blank-falls-to-reasoning",
            _asst(content=None, reasoning_content="", reasoning="real"))
    wrapped("reasoning-content-wins-over-reasoning",
            _asst(content=None, reasoning_content="real", reasoning="other"))
    # reasoning_details list form.
    wrapped("reasoning-details-nonempty",
            _asst(content=None, reasoning_details=[{"format": "x"}]))
    wrapped("reasoning-details-multi",
            _asst(content=None, reasoning_details=[{"a": 1}, {"b": 2}]))
    # codex reasoning items with drop flag True -> real reasoning item drops.
    wrapped("codex-reasoning-item-drop-true",
            _asst(content=None,
                  codex_reasoning_items=[{"type": "reasoning", "id": "r1"}]),
            drop=True)
    wrapped("codex-reasoning-among-junk-drop-true",
            _asst(content=None,
                  codex_reasoning_items=[{"type": "message"},
                                         {"type": "reasoning"}]),
            drop=True)

    # -- detector: thinking-only FALSE (turn is kept) -------------------------
    def wrapped_kept(name: str, asst: Dict[str, Any], drop: bool = True) -> None:
        add(name, [_user("before"), asst, _user("after")], drop)

    # truthy tool_calls -> never thinking-only, even with reasoning.
    wrapped_kept("truthy-tool-calls-with-reasoning",
                 _asst(content=None, reasoning="x",
                       tool_calls=[{"id": "c1", "function": {"name": "f"}}]))
    wrapped_kept("truthy-tool-calls-string",
                 _asst(content=None, reasoning="x", tool_calls="weird"))
    # visible text / list payloads.
    wrapped_kept("visible-string-content",
                 _asst(content="hello there", reasoning="x"))
    wrapped_kept("text-block-content",
                 _asst(content=[{"type": "text", "text": "hi"}], reasoning="x"))
    wrapped_kept("tool-use-block-content",
                 _asst(content=[{"type": "tool_use", "name": "f"}], reasoning="x"))
    wrapped_kept("image-block-content",
                 _asst(content=[{"type": "image"}], reasoning="x"))
    # non-dict blocks: truthy string -> real payload.
    wrapped_kept("nondict-truthy-block",
                 _asst(content=["raw"], reasoning="x"))
    # non-dict falsy blocks only, no reasoning -> empty turn, kept.
    wrapped_kept("nondict-falsy-blocks-no-reasoning",
                 _asst(content=["", 0, None]))
    # unknown scalar content shapes -> real payload.
    wrapped_kept("int-content",
                 _asst(content=5, reasoning="x"))
    wrapped_kept("dict-content",
                 _asst(content={"k": "v"}, reasoning="x"))
    # empty turn: no reasoning of any kind.
    wrapped_kept("empty-turn-null-content", _asst(content=None))
    wrapped_kept("empty-turn-blank-string", _asst(content=""))
    # reasoning empty / details empty -> falls through, kept.
    wrapped_kept("reasoning-blank-only", _asst(content=None, reasoning="   "))
    wrapped_kept("reasoning-details-empty-list",
                 _asst(content=None, reasoning_details=[]))
    # checkpoint protection: compaction item -> never thinking-only.
    wrapped_kept("checkpoint-protects-with-reasoning",
                 _asst(content=None, reasoning="x",
                       codex_reasoning_items=[{"type": "compaction"}]))
    wrapped_kept("checkpoint-protects-with-reasoning-details",
                 _asst(content=None, reasoning_details=[{"a": 1}],
                       codex_reasoning_items=[{"type": "compaction"}]))
    wrapped_kept("checkpoint-alongside-reasoning-item",
                 _asst(content=None,
                       codex_reasoning_items=[{"type": "reasoning"},
                                              {"type": "compaction"}]),
                 drop=True)
    # codex items but no real reasoning item -> kept.
    wrapped_kept("codex-junk-items-drop-true",
                 _asst(content=None,
                       codex_reasoning_items=[{"type": "message"}]), drop=True)
    wrapped_kept("codex-empty-list-drop-true",
                 _asst(content=None, codex_reasoning_items=[]), drop=True)
    wrapped_kept("codex-nondict-items-drop-true",
                 _asst(content=None, codex_reasoning_items=["x", 1, None]),
                 drop=True)
    # role != assistant is never thinking-only.
    wrapped_kept("system-role-with-reasoning",
                 {"role": "system", "content": None, "reasoning": "x"})
    add("tool-role-with-reasoning",
        [_user("before"),
         {"role": "tool", "content": None, "reasoning": "x",
          "tool_call_id": "c1"},
         _user("after")])

    # -- both drop_codex_reasoning_items values on the SAME message -----------
    codex_only = _asst(content=None,
                       codex_reasoning_items=[{"type": "reasoning", "id": "r"}])
    add("codex-reasoning-drop-true",
        [_user("a"), copy.deepcopy(codex_only), _user("b")], drop=True)
    add("codex-reasoning-drop-false",
        [_user("a"), copy.deepcopy(codex_only), _user("b")], drop=False)
    # A checkpoint carrier is protected regardless of the flag.
    ckpt = _asst(content=None, codex_reasoning_items=[{"type": "compaction"}])
    add("checkpoint-drop-true",
        [_user("a"), copy.deepcopy(ckpt), _user("b")], drop=True)
    add("checkpoint-drop-false",
        [_user("a"), copy.deepcopy(ckpt), _user("b")], drop=False)
    # A real string-reasoning carrier drops under both flag values (flag only
    # gates the codex_reasoning_items branch).
    strr = _asst(content=None, reasoning="think")
    add("string-reasoning-drop-true",
        [_user("a"), copy.deepcopy(strr), _user("b")], drop=True)
    add("string-reasoning-drop-false",
        [_user("a"), copy.deepcopy(strr), _user("b")], drop=False)

    # -- adjacent user merges: all content shape pairs ------------------------
    # After the thinking-only assistant is dropped, the two users merge. The
    # pair (prev_content, cur_content) exercises each merge branch.
    drop_asst = _asst(content=None, reasoning="x")

    def merge_pair(name: str, prev_content: Any, cur_content: Any) -> None:
        add("merge-" + name,
            [_user(prev_content), copy.deepcopy(drop_asst), _user(cur_content)])

    merge_pair("str-str", "left", "right")
    merge_pair("str-str-prev-empty", "", "right")
    merge_pair("str-str-cur-empty", "left", "")
    merge_pair("str-str-both-empty", "", "")
    merge_pair("list-list",
               [{"type": "text", "text": "L"}], [{"type": "text", "text": "R"}])
    merge_pair("list-str",
               [{"type": "text", "text": "L"}], "right")
    merge_pair("list-str-empty",
               [{"type": "text", "text": "L"}], "")
    merge_pair("str-list",
               "left", [{"type": "text", "text": "R"}])
    merge_pair("str-list-prev-empty",
               "", [{"type": "text", "text": "R"}])
    # Unknown content shape on either side -> fall back to appending, no merge.
    merge_pair("unknown-prev-int", 7, "right")
    merge_pair("unknown-cur-int", "left", 7)
    merge_pair("unknown-both-none", None, None)
    # cur user with no content key at all (defaults to "").
    add("merge-cur-missing-content",
        [_user("left"), copy.deepcopy(drop_asst), {"role": "user"}])
    add("merge-prev-missing-content",
        [{"role": "user"}, copy.deepcopy(drop_asst), _user("right")])

    # Three users collapse after two drops.
    add("merge-three-users",
        [_user("one"), copy.deepcopy(drop_asst),
         _user("two"), copy.deepcopy(drop_asst), _user("three")])
    # Users already adjacent (no drop) still merge: dropped==0, merges>0.
    add("merge-already-adjacent-no-drop", [_user("one"), _user("two")])
    # Assistant that is NOT thinking-only stays between users -> no merge.
    add("no-merge-real-assistant-between",
        [_user("one"), _asst(content="answer"), _user("two")])
    # Trailing thinking-only turn dropped, nothing to merge after it.
    add("trailing-thinking-only-drop",
        [_user("q"), _asst(content="a"), copy.deepcopy(drop_asst)])
    # Leading thinking-only turn dropped.
    add("leading-thinking-only-drop",
        [copy.deepcopy(drop_asst), _user("q"), _asst(content="a")])

    # -- pass-through / no-op cases -------------------------------------------
    add("empty-messages", [])
    add("single-user", [_user("hi")])
    add("clean-alternating",
        [_user("q1"), _asst(content="a1"), _user("q2"), _asst(content="a2")])
    # Assistant real reply then user then thinking-only user-adjacent drop.
    add("mixed-drop-and-merge",
        [_user("start"), copy.deepcopy(drop_asst), _user("mid"),
         _asst(content="real"), _user("end1"),
         copy.deepcopy(drop_asst), _user("end2")])

    return out


def main() -> None:
    run = load_oracle()
    rows: List[Dict[str, Any]] = []
    for case in cases():
        messages = case["messages"]
        drop = case["drop"]
        before = copy.deepcopy(messages)
        expected = run(messages, drop)
        # Source purity: the sanitizer must not mutate its inputs.
        assert messages == before, f"input mutated by oracle in case {case['name']!r}"
        rows.append({
            "name": case["name"],
            "messages": messages,
            "drop_codex_reasoning_items": drop,
            "expected": expected,
        })

    if len(rows) >= 350:
        raise SystemExit(f"fixture count {len(rows)} exceeds bound (<350)")

    content = json.dumps(rows, indent=2, ensure_ascii=False) + "\n"
    argv = sys.argv[1:]
    if argv == ["--check"]:
        current = OUT.read_text(encoding="utf-8") if OUT.exists() else ""
        if current != content:
            raise SystemExit("thinking-repair fixtures differ from Python output")
    elif not argv:
        OUT.write_text(content, encoding="utf-8")
    else:
        raise SystemExit("usage: gen_thinking_repair_goldens.py [--check]")
    print(f"Verified {len(rows)} thinking-repair cases")


if __name__ == "__main__":
    main()
