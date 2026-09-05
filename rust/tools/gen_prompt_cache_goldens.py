#!/usr/bin/env python3
"""Generate golden cases for prompt_cache.rs.

Executes the *real* prompt-cache routing logic from the two transports so the
Rust port is checked against CPython behaviour, not a paraphrase. Rather than
importing the modules (which drag in the whole agent runtime), we pull the five
target functions out of the source with ``ast`` and exec just those:

  agent/transports/codex.py:
    _cache_scope_from_session_id, _bounded_prompt_cache_key, _content_cache_key
    (plus the module-level _CRON_SESSION_ID_RE they depend on)
  agent/transports/chat_completions.py:
    _static_prompt_instructions, _add_prompt_cache_key

``_add_prompt_cache_key`` lazily imports the three codex helpers, so we register
a stub ``agent.transports.codex`` module (backed by the exec'd functions) in
``sys.modules`` before calling it.

Run with the pinned interpreter:

    mise x python@3.12.13 -- python rust/tools/gen_prompt_cache_goldens.py

Writes ``rust/tools/prompt-cache-goldens.json`` next to this script.
Pass ``--check`` to verify the committed fixture still matches the source.
"""
from __future__ import annotations

import ast
import copy
import hashlib  # noqa: F401 - referenced by exec'd source
import json
import re  # noqa: F401 - referenced by exec'd source
import sys
import types
from pathlib import Path
from typing import Any, Dict, List, Optional  # noqa: F401 - exec namespace

REPO_ROOT = Path(__file__).resolve().parents[2]
CODEX_SOURCE = REPO_ROOT / "agent" / "transports" / "codex.py"
CHAT_SOURCE = REPO_ROOT / "agent" / "transports" / "chat_completions.py"
OUT = Path(__file__).resolve().parent / "prompt-cache-goldens.json"

CODEX_FUNCS = {
    "_cache_scope_from_session_id",
    "_bounded_prompt_cache_key",
    "_content_cache_key",
}
CODEX_ASSIGNS = {"_CRON_SESSION_ID_RE"}
CHAT_FUNCS = {"_static_prompt_instructions", "_add_prompt_cache_key"}


def _extract(source: Path, funcs: set[str], assigns: set[str]) -> str:
    """Return source text for the named functions and assignments only."""
    tree = ast.parse(source.read_text(encoding="utf-8"))
    segments: List[str] = []
    found_funcs: set[str] = set()
    found_assigns: set[str] = set()
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name in funcs:
            segments.append(ast.unparse(node))
            found_funcs.add(node.name)
        elif isinstance(node, ast.Assign):
            names = {t.id for t in node.targets if isinstance(t, ast.Name)}
            if names & assigns:
                segments.append(ast.unparse(node))
                found_assigns |= names & assigns
    missing = (funcs - found_funcs) | (assigns - found_assigns)
    if missing:
        raise SystemExit(f"could not find in {source.name}: {sorted(missing)}")
    return "\n\n".join(segments)


def load_functions() -> Dict[str, Any]:
    """Exec the target functions from both files into one namespace."""
    ns: Dict[str, Any] = {
        "hashlib": hashlib,
        "json": json,
        "re": re,
        "Any": Any,
        "Dict": Dict,
        "List": List,
        "Optional": Optional,
    }
    exec(_extract(CODEX_SOURCE, CODEX_FUNCS, CODEX_ASSIGNS), ns)  # noqa: S102

    # _add_prompt_cache_key does `from agent.transports.codex import ...`; back
    # that import with the functions we just exec'd. Register the parent
    # packages too so the import machinery can resolve the dotted path.
    codex_stub = types.ModuleType("agent.transports.codex")
    for name in CODEX_FUNCS:
        setattr(codex_stub, name, ns[name])
    transports_stub = types.ModuleType("agent.transports")
    transports_stub.codex = codex_stub
    agent_stub = types.ModuleType("agent")
    agent_stub.transports = transports_stub
    sys.modules.setdefault("agent", agent_stub)
    sys.modules.setdefault("agent.transports", transports_stub)
    sys.modules["agent.transports.codex"] = codex_stub

    exec(_extract(CHAT_SOURCE, CHAT_FUNCS, set()), ns)  # noqa: S102
    return ns


# --- Hand-authored inputs covering the behaviours the port must preserve ---

def scope_cases() -> List[Dict[str, Any]]:
    arabic8 = "٠١٢٣٤٥٦٧"
    arabic6 = "٠١٢٣٤٥"
    return [
        {"name": "cron-stripped", "session_id": "cron_job42_20250101_120000"},
        {"name": "cron-inner-underscores", "session_id": "cron_a_b_c_20250101_120000"},
        {"name": "non-cron", "session_id": "main_abc.child_1"},
        {"name": "none", "session_id": None},
        {"name": "empty", "session_id": ""},
        {"name": "cron-trailing-newline", "session_id": "cron_x_20250101_120000\n"},
        {"name": "cron-two-newlines", "session_id": "cron_x_20250101_120000\n\n"},
        {"name": "cron-empty-plus", "session_id": "cron__20250101_120000"},
        {"name": "cron-wrong-digit-count", "session_id": "cron_x_2025010_120000"},
        {"name": "cron-trailing-junk", "session_id": "cron_x_20250101_120000z"},
        {"name": "not-cron-prefix", "session_id": "job_x_20250101_120000"},
        {"name": "cron-unicode-digits", "session_id": f"cron_j_{arabic8}_{arabic6}"},
        {"name": "cron-newline-in-middle", "session_id": "cron_a\nb_20250101_120000"},
    ]


def bounded_cases() -> List[Dict[str, Any]]:
    return [
        {"name": "none", "value": None},
        {"name": "empty", "value": ""},
        {"name": "whitespace", "value": "   \t\n"},
        {"name": "short", "value": "sess-123"},
        {"name": "exactly-64", "value": "a" * 64},
        {"name": "over-64", "value": "a" * 65},
        {"name": "int", "value": 12345},
        {"name": "float", "value": 1.5},
        {"name": "bool", "value": False},
        {"name": "dict-repr", "value": {"a": 1, "b": "x"}},
        {"name": "list-repr", "value": [1, 2, 3]},
        {"name": "unicode-64-codepoints", "value": "é" * 64},
        {"name": "unicode-65-codepoints", "value": "é" * 65},
        {"name": "strip-then-measure", "value": "  " + "a" * 64 + "  "},
    ]


def static_cases() -> List[Dict[str, Any]]:
    return [
        {"name": "empty", "messages": []},
        {"name": "first-not-dict", "messages": ["hi"]},
        {"name": "wrong-role", "messages": [{"role": "user", "content": "hi"}]},
        {"name": "no-role", "messages": [{"content": "hi"}]},
        {"name": "system-str", "messages": [{"role": "system", "content": "you are helpful"}]},
        {"name": "developer-str", "messages": [{"role": "developer", "content": "dev prefix"}]},
        {"name": "missing-content", "messages": [{"role": "system"}]},
        {"name": "content-null", "messages": [{"role": "system", "content": None}]},
        {"name": "content-list", "messages": [
            {"role": "system", "content": [{"type": "text", "text": "b"}, {"a": 1}]}]},
        {"name": "content-dict-sorted", "messages": [
            {"role": "system", "content": {"z": 1, "a": {"n": 2, "m": 1}}}]},
        {"name": "content-nonascii", "messages": [
            {"role": "developer", "content": {"msg": "café 中文"}}]},
        {"name": "content-float", "messages": [
            {"role": "system", "content": {"t": 0.1, "big": 1e16, "small": 1e-05}}]},
        {"name": "role-non-string", "messages": [{"role": 5, "content": "hi"}]},
    ]


def content_cases() -> List[Dict[str, Any]]:
    tool_a = {"name": "alpha", "description": "a", "parameters": {"type": "object"}}
    tool_b = {"name": "beta", "type": "function"}
    return [
        {"name": "both-empty", "instructions": "", "tools": None, "scope_id": ""},
        {"name": "instructions-only", "instructions": "sys", "tools": None, "scope_id": ""},
        {"name": "tools-only", "instructions": "", "tools": [tool_a], "scope_id": ""},
        {"name": "empty-tools-list", "instructions": "", "tools": [], "scope_id": ""},
        {"name": "tools-nondict-only", "instructions": "", "tools": ["x", 5], "scope_id": ""},
        {"name": "sorted-by-name", "instructions": "sys", "tools": [tool_b, tool_a], "scope_id": "s1"},
        {"name": "sorted-reordered", "instructions": "sys", "tools": [tool_a, tool_b], "scope_id": "s1"},
        {"name": "sort-by-type-fallback", "instructions": "", "scope_id": "",
         "tools": [{"type": "web"}, {"type": "ackermann"}]},
        {"name": "sort-nondict-filtered", "instructions": "", "scope_id": "",
         "tools": [tool_b, "junk", tool_a, 42]},
        {"name": "scope-changes-hash", "instructions": "sys", "tools": [tool_a], "scope_id": "other"},
        {"name": "nested-function-name-ignored", "instructions": "", "scope_id": "",
         "tools": [{"name": "b", "function": {"name": "a"}},
                   {"name": "a", "function": {"name": "b"}}]},
        {"name": "numeric-name-coercion", "instructions": "", "scope_id": "",
         "tools": [{"name": 2}, {"name": 10}]},
        {"name": "nonascii-and-float", "instructions": "café", "scope_id": "s",
         "tools": [{"name": "t", "x": 0.1, "y": 1e16}]},
    ]


def apply_cases() -> List[Dict[str, Any]]:
    sys_msg = [{"role": "system", "content": "you are helpful"}]
    tool = [{"name": "alpha", "type": "function"}]
    return [
        # caller top-level key, short -> kept.
        {"name": "caller-top-short", "api_kwargs": {"prompt_cache_key": "sess-1"},
         "messages": sys_msg, "tools": None, "supports": True,
         "session_id": "s1", "cache_scope_id": None},
        # caller top-level key, long -> bounded in place.
        {"name": "caller-top-long", "api_kwargs": {"prompt_cache_key": "z" * 100},
         "messages": sys_msg, "tools": None, "supports": True,
         "session_id": "s1", "cache_scope_id": None},
        # caller top-level key, empty -> removed, no autogenerate.
        {"name": "caller-top-empty-removed", "api_kwargs": {"prompt_cache_key": "   "},
         "messages": sys_msg, "tools": tool, "supports": True,
         "session_id": "s1", "cache_scope_id": None},
        # caller key honored even when unsupported.
        {"name": "caller-top-unsupported", "api_kwargs": {"prompt_cache_key": "y" * 80},
         "messages": sys_msg, "tools": None, "supports": False,
         "session_id": None, "cache_scope_id": None},
        # caller key inside extra_body, long -> bounded there, siblings kept.
        {"name": "caller-extra-long",
         "api_kwargs": {"extra_body": {"prompt_cache_key": "q" * 100, "keep": 1}},
         "messages": sys_msg, "tools": None, "supports": True,
         "session_id": None, "cache_scope_id": None},
        # caller key inside extra_body, empty -> removed.
        {"name": "caller-extra-empty",
         "api_kwargs": {"extra_body": {"prompt_cache_key": "", "keep": 2}},
         "messages": sys_msg, "tools": None, "supports": True,
         "session_id": None, "cache_scope_id": None},
        # both locations present -> both bounded separately.
        {"name": "caller-both",
         "api_kwargs": {"prompt_cache_key": "a" * 70,
                        "extra_body": {"prompt_cache_key": "b" * 70}},
         "messages": sys_msg, "tools": None, "supports": True,
         "session_id": None, "cache_scope_id": None},
        # extra_body present but not a dict, no top key -> autogenerate path.
        {"name": "extra-body-not-dict",
         "api_kwargs": {"extra_body": "nope"},
         "messages": sys_msg, "tools": tool, "supports": True,
         "session_id": "s1", "cache_scope_id": None},
        # supported, no caller key -> autogenerate.
        {"name": "autogenerate", "api_kwargs": {"model": "gpt-5"},
         "messages": sys_msg, "tools": tool, "supports": True,
         "session_id": "cron_j_20250101_120000", "cache_scope_id": None},
        # cache_scope_id precedence over session_id.
        {"name": "scope-precedence", "api_kwargs": {},
         "messages": sys_msg, "tools": tool, "supports": True,
         "session_id": "physical", "cache_scope_id": "logical-scope"},
        # unsupported, no caller key -> untouched.
        {"name": "unsupported-no-caller", "api_kwargs": {"model": "x"},
         "messages": sys_msg, "tools": tool, "supports": False,
         "session_id": "s1", "cache_scope_id": None},
        # supported but nothing static (empty messages, no tools) -> untouched.
        {"name": "nothing-static", "api_kwargs": {},
         "messages": [], "tools": None, "supports": True,
         "session_id": "s1", "cache_scope_id": None},
    ]


def main() -> None:
    ns = load_functions()
    scope = ns["_cache_scope_from_session_id"]
    bounded = ns["_bounded_prompt_cache_key"]
    static = ns["_static_prompt_instructions"]
    content = ns["_content_cache_key"]
    add_key = ns["_add_prompt_cache_key"]

    out: Dict[str, Any] = {"scope": [], "bounded": [], "static": [], "content": [], "apply": []}

    for case in scope_cases():
        out["scope"].append({
            "name": case["name"],
            "session_id": case["session_id"],
            "expected": scope(case["session_id"]),
        })

    for case in bounded_cases():
        out["bounded"].append({
            "name": case["name"],
            "value": case["value"],
            "expected": bounded(case["value"]),
        })

    for case in static_cases():
        out["static"].append({
            "name": case["name"],
            "messages": case["messages"],
            "expected": static(case["messages"]),
        })

    for case in content_cases():
        tools = case.get("tools")
        out["content"].append({
            "name": case["name"],
            "instructions": case["instructions"],
            "tools": tools,
            "scope_id": case["scope_id"],
            "expected": content(case["instructions"], tools, case["scope_id"]),
        })

    for case in apply_cases():
        api_kwargs = copy.deepcopy(case["api_kwargs"])
        add_key(
            api_kwargs,
            messages=case["messages"],
            tools=case["tools"],
            supports_prompt_cache_key=case["supports"],
            session_id=case["session_id"],
            cache_scope_id=case["cache_scope_id"],
        )
        out["apply"].append({
            "name": case["name"],
            "api_kwargs": copy.deepcopy(case["api_kwargs"]),
            "messages": case["messages"],
            "tools": case["tools"],
            "supports": case["supports"],
            "session_id": case["session_id"],
            "cache_scope_id": case["cache_scope_id"],
            "expected": api_kwargs,
        })

    content_out = json.dumps(out, indent=2, ensure_ascii=False) + "\n"
    total = sum(len(v) for v in out.values())
    if sys.argv[1:] == ["--check"]:
        assert OUT.read_text(encoding="utf-8") == content_out, "Prompt cache fixtures differ from Python"
    elif not sys.argv[1:]:
        OUT.write_text(content_out, encoding="utf-8")
    else:
        raise SystemExit("usage: gen_prompt_cache_goldens.py [--check]")
    print(f"Verified {total} prompt cache cases")


if __name__ == "__main__":
    main()
