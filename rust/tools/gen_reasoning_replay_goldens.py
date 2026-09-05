#!/usr/bin/env python3
"""Generate golden test fixtures for reasoning_replay.rs.

Extracts functions from agent/message_sanitization.py and utils.py via AST and
executes them under CPython 3.12.13 so the Rust port is checked against the real
CPython behaviour without approximating.

Run with:
    mise x python@3.12.13 -- python rust/tools/gen_reasoning_replay_goldens.py
    mise x python@3.12.13 -- python rust/tools/gen_reasoning_replay_goldens.py --check
"""
from __future__ import annotations

import ast
import copy
import json
import logging
import sys
import types
from pathlib import Path
from typing import Any, Dict, List
from urllib.parse import urlparse

REPO_ROOT = Path(__file__).resolve().parents[2]
UTILS_PATH = REPO_ROOT / "utils.py"
SANITIZATION_PATH = REPO_ROOT / "agent" / "message_sanitization.py"
OUT_PATH = Path(__file__).resolve().parent / "reasoning-replay-goldens.json"


def load_extracted_functions() -> Dict[str, Any]:
    """Extract required functions and rules from utils.py and message_sanitization.py via AST."""
    # 1. Extract utils helpers
    utils_tree = ast.parse(UTILS_PATH.read_text(encoding="utf-8"))
    wanted_utils = {"base_url_hostname", "base_url_host_matches"}
    utils_nodes = [
        n
        for n in utils_tree.body
        if isinstance(n, ast.FunctionDef) and n.name in wanted_utils
    ]

    utils_mod = types.ModuleType("utils")
    utils_mod.urlparse = urlparse
    utils_mod.__dict__["urlparse"] = urlparse
    for n in utils_nodes:
        code = compile(ast.Module(body=[n], type_ignores=[]), "utils.py", "exec")
        exec(code, utils_mod.__dict__)
    sys.modules["utils"] = utils_mod

    # 2. Extract message_sanitization helpers
    sanit_tree = ast.parse(SANITIZATION_PATH.read_text(encoding="utf-8"))
    wanted_sanit = {
        "_family_rule",
        "matches_reasoning_echo_family",
        "reasoning_echo_family",
        "needs_reasoning_echo",
        "apply_reasoning_content_policy",
    }
    sanit_nodes: List[ast.AST] = []
    for n in sanit_tree.body:
        if (
            isinstance(n, ast.AnnAssign)
            and isinstance(n.target, ast.Name)
            and n.target.id == "_REASONING_ECHO_RULES"
        ):
            sanit_nodes.append(n)
        elif isinstance(n, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "_REASONING_ECHO_RULES"
            for t in n.targets
        ):
            sanit_nodes.append(n)
        elif isinstance(n, ast.FunctionDef) and n.name in wanted_sanit:
            sanit_nodes.append(n)

    ns: Dict[str, Any] = {
        "Any": object,
        "frozenset": frozenset,
        "logger": logging.getLogger("reasoning_replay_goldens"),
    }
    for n in sanit_nodes:
        code = compile(
            ast.Module(body=[n], type_ignores=[]),
            "agent/message_sanitization.py",
            "exec",
        )
        exec(code, ns)

    return ns


def build_hostname_cases(ns: Dict[str, Any]) -> List[Dict[str, Any]]:
    base_cases = [
        ("https://api.moonshot.ai/v1", "moonshot.ai"),
        ("https://moonshot.ai", "moonshot.ai"),
        ("https://evil.com/moonshot.ai/v1", "moonshot.ai"),
        ("https://moonshot.ai.evil/v1", "moonshot.ai"),
        ("moonshot.ai", "moonshot.ai"),
        ("api.moonshot.ai:8080", "moonshot.ai"),
        ("https://api.kimi.com/v1", "api.kimi.com"),
        ("https://api.kimi.com./v1", "api.kimi.com"),
        ("   https://api.deepseek.com/v1   ", "api.deepseek.com"),
        ("https://api.xiaomimimo.com", "xiaomimimo.com"),
        ("https://xiaomimimo.com", "api.xiaomimimo.com"),
        ("http://localhost:8000/v1", "localhost"),
        ("http://127.0.0.1:11434", "127.0.0.1"),
        ("http://[::1]:8080/v1", "::1"),
        ("", "moonshot.ai"),
        ("https://api.kimi.com", ""),
        ("//api.kimi.com", "api.kimi.com"),
        ("https://evilmoonshot.ai/v1", "moonshot.ai"),
    ]
    utils = sys.modules["utils"]
    out = []
    for idx, (base_url, domain) in enumerate(base_cases):
        host = utils.base_url_hostname(base_url)
        matches = utils.base_url_host_matches(base_url, domain)
        out.append({
            "name": f"host-{idx}-{domain}",
            "base_url": base_url,
            "domain": domain,
            "expected_hostname": host,
            "expected_match": matches,
        })
    return out


def build_echo_cases(ns: Dict[str, Any]) -> List[Dict[str, Any]]:
    raw_inputs = [
        ("kimi-coding", None, "https://x"),
        ("kimi-coding-cn", None, "https://x"),
        ("custom", None, "https://api.kimi.com/v1"),
        ("custom", None, "https://api.moonshot.ai/v1"),
        ("custom", None, "https://api.moonshot.cn/v1"),
        ("deepseek", "whatever", "https://x"),
        ("DeepSeek", "whatever", "https://x"),
        ("openrouter", "deepseek/deepseek-v3", "https://openrouter.ai"),
        ("custom", None, "https://api.deepseek.com"),
        ("xiaomi", None, "https://x"),
        ("custom", "MiMo-7B", "https://x"),
        ("custom", None, "https://api.xiaomimimo.com/v1"),
        ("openai", "gpt-5", "https://api.openai.com/v1"),
        ("mistral", "mistral-large", "https://api.mistral.ai/v1"),
        (None, None, None),
        ("KIMI-CODING", None, "https://x"),
        ("custom", "deepseek-chat", "https://api.kimi.com"),
        ("custom", "kimi-k3", "https://x"),
        ("custom", "deepseek-r1", "https://proxy.example.com"),
        ("custom", "model-mimo-v1", "https://proxy.example.com"),
        ("groq", "llama-3", "https://api.groq.com/openai/v1"),
        ("cerebras", "llama3.1-8b", "https://api.cerebras.ai/v1"),
        ("sambanova", "Meta-Llama-3-70B", "https://api.sambanova.ai/v1"),
        ("", "", ""),
    ]

    reasoning_echo_family = ns["reasoning_echo_family"]
    needs_reasoning_echo = ns["needs_reasoning_echo"]
    matches_reasoning_echo_family = ns["matches_reasoning_echo_family"]

    out = []
    for idx, (provider, model, base_url) in enumerate(raw_inputs):
        fam = reasoning_echo_family(provider, model, base_url)
        needs = needs_reasoning_echo(provider, model, base_url)
        m_kimi = matches_reasoning_echo_family("kimi", provider, model, base_url)
        m_deepseek = matches_reasoning_echo_family("deepseek", provider, model, base_url)
        m_mimo = matches_reasoning_echo_family("mimo", provider, model, base_url)
        out.append({
            "name": f"echo-rule-{idx}-{provider}-{fam}",
            "provider": provider or "",
            "model": model or "",
            "base_url": base_url or "",
            "expected_family": fam,
            "expected_needs_echo": needs,
            "matches_kimi": m_kimi,
            "matches_deepseek": m_deepseek,
            "matches_mimo": m_mimo,
        })
    return out


def build_apply_cases(ns: Dict[str, Any]) -> List[Dict[str, Any]]:
    cases_spec = [
        # Non-assistant messages are untouched
        {
            "name": "user-untouched-require",
            "source": {"role": "user", "content": "u", "reasoning_content": "keep"},
            "api": {"role": "user", "content": "u", "reasoning_content": "keep"},
            "needs_echo": True,
        },
        {
            "name": "user-untouched-strict",
            "source": {"role": "user", "content": "u", "reasoning_content": "keep"},
            "api": {"role": "user", "content": "u", "reasoning_content": "keep"},
            "needs_echo": False,
        },
        {
            "name": "system-untouched-require",
            "source": {"role": "system", "content": "sys", "reasoning": "r"},
            "api": {"role": "system", "content": "sys"},
            "needs_echo": True,
        },
        {
            "name": "tool-untouched-require",
            "source": {"role": "tool", "content": "t", "tool_call_id": "c1"},
            "api": {"role": "tool", "content": "t", "tool_call_id": "c1"},
            "needs_echo": True,
        },
        # Require-side: existing reasoning_content preserved
        {
            "name": "require-preserves-existing",
            "source": {"role": "assistant", "content": "x", "reasoning_content": "thoughts"},
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": True,
        },
        # Require-side: empty string upgraded to space
        {
            "name": "require-upgrades-empty-string-to-space",
            "source": {"role": "assistant", "content": "x", "reasoning_content": ""},
            "api": {"role": "assistant", "content": "x", "reasoning_content": ""},
            "needs_echo": True,
        },
        # Require-side: single space preserved
        {
            "name": "require-preserves-space-pad",
            "source": {"role": "assistant", "content": "x", "reasoning_content": " "},
            "api": {"role": "assistant", "content": "x", "reasoning_content": " "},
            "needs_echo": True,
        },
        # Require-side: cross-provider poisoned history (tool_calls + reasoning, no reasoning_content)
        {
            "name": "require-poisoned-history-pads-space",
            "source": {
                "role": "assistant",
                "content": "x",
                "reasoning": "foreign-model CoT",
                "tool_calls": [{"id": "c", "function": {"name": "f", "arguments": "{}"}}],
            },
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": True,
        },
        # Require-side: healthy session promotes reasoning -> reasoning_content
        {
            "name": "require-healthy-session-promotes-reasoning",
            "source": {"role": "assistant", "content": "x", "reasoning": "genuine thought"},
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": True,
        },
        # Require-side: bare assistant turn receives single space pad
        {
            "name": "require-bare-assistant-receives-space",
            "source": {"role": "assistant", "content": "x"},
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": True,
        },
        # Require-side: assistant with tool_calls but no reasoning receives space
        {
            "name": "require-tool-calls-without-reasoning-receives-space",
            "source": {
                "role": "assistant",
                "content": "x",
                "tool_calls": [{"id": "c"}],
            },
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": True,
        },
        # Require-side: non-string reasoning_content (None) receives space pad
        {
            "name": "require-none-reasoning-content-receives-space",
            "source": {"role": "assistant", "content": "x", "reasoning_content": None},
            "api": {"role": "assistant", "content": "x", "reasoning_content": None},
            "needs_echo": True,
        },
        # Require-side: empty reasoning string with tool_calls receives space
        {
            "name": "require-empty-reasoning-with-tool-calls",
            "source": {"role": "assistant", "content": "x", "reasoning": "", "tool_calls": [{"id": "c"}]},
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": True,
        },
        # Require-side: empty tool_calls list promotes reasoning (not poisoned because [] is falsy)
        {
            "name": "require-empty-tool-calls-promotes-reasoning",
            "source": {"role": "assistant", "content": "x", "reasoning": "valid", "tool_calls": []},
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": True,
        },
        # Strict-side: strips existing reasoning_content
        {
            "name": "strict-strips-existing-reasoning-content",
            "source": {"role": "assistant", "content": "x", "reasoning_content": "thoughts"},
            "api": {"role": "assistant", "content": "x", "reasoning_content": "thoughts"},
            "needs_echo": False,
        },
        # Strict-side: strips single space pad
        {
            "name": "strict-strips-space-pad",
            "source": {"role": "assistant", "content": "x", "reasoning_content": " "},
            "api": {"role": "assistant", "content": "x", "reasoning_content": " "},
            "needs_echo": False,
        },
        # Strict-side: strips empty string
        {
            "name": "strict-strips-empty-string",
            "source": {"role": "assistant", "content": "x", "reasoning_content": ""},
            "api": {"role": "assistant", "content": "x", "reasoning_content": ""},
            "needs_echo": False,
        },
        # Strict-side: strips None
        {
            "name": "strict-strips-none",
            "source": {"role": "assistant", "content": "x", "reasoning_content": None},
            "api": {"role": "assistant", "content": "x", "reasoning_content": None},
            "needs_echo": False,
        },
        # Strict-side: does not promote reasoning
        {
            "name": "strict-does-not-promote-reasoning",
            "source": {"role": "assistant", "content": "x", "reasoning": "genuine thought"},
            "api": {"role": "assistant", "content": "x", "reasoning_content": "stale"},
            "needs_echo": False,
        },
        # Strict-side: bare assistant turn remains without reasoning_content
        {
            "name": "strict-bare-assistant-stays-without-key",
            "source": {"role": "assistant", "content": "x"},
            "api": {"role": "assistant", "content": "x"},
            "needs_echo": False,
        },
    ]

    apply_fn = ns["apply_reasoning_content_policy"]
    out = []
    for case in cases_spec:
        src = copy.deepcopy(case["source"])
        api = copy.deepcopy(case["api"])
        needs_echo = case["needs_echo"]

        apply_fn(src, api, needs_echo)

        in_place = copy.deepcopy(case["source"])
        apply_fn(in_place, in_place, needs_echo)

        out.append({
            "name": case["name"],
            "source_msg": case["source"],
            "api_msg": case["api"],
            "needs_thinking_pad": needs_echo,
            "expected_api_msg": api,
            "expected_in_place_msg": in_place,
        })
    return out


def main() -> None:
    ns = load_extracted_functions()
    goldens = {
        "hostname_cases": build_hostname_cases(ns),
        "echo_cases": build_echo_cases(ns),
        "apply_cases": build_apply_cases(ns),
    }
    content = json.dumps(goldens, indent=2) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not OUT_PATH.exists():
            raise SystemExit(f"Golden file does not exist: {OUT_PATH}")
        existing = OUT_PATH.read_text(encoding="utf-8")
        assert existing == content, "Reasoning replay golden fixtures differ from Python execution"
        print(f"Verified reasoning replay goldens against Python AST execution ({len(goldens['apply_cases'])} apply cases, {len(goldens['echo_cases'])} echo cases)")
    elif not sys.argv[1:]:
        OUT_PATH.write_text(content, encoding="utf-8")
        total = sum(len(v) for v in goldens.values())
        print(f"Wrote {total} reasoning replay golden cases across {len(goldens)} sections to {OUT_PATH}")
    else:
        raise SystemExit("usage: gen_reasoning_replay_goldens.py [--check]")


if __name__ == "__main__":
    main()
