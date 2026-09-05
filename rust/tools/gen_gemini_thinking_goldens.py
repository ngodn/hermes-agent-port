#!/usr/bin/env python3
"""Generate golden cases for gemini_thinking.rs.

This executes the *real* Gemini thinking-config logic from the Hermes source so
the Rust port can be checked against CPython behaviour rather than a paraphrase.
Rather than importing the modules (which drags in httpx and the whole agent
runtime), we pull the six pure functions out of the two source files with
``ast`` and exec just those into one shared namespace:

  agent/transports/chat_completions.py
    _build_gemini_thinking_config
    _snake_case_gemini_thinking_config
    _raise_gemini_thinking_max_tokens

  agent/gemini_native_adapter.py
    _normalize_thinking_config
    _thinking_requests_output_headroom
    _effective_gemini_max_output_tokens

``_raise_gemini_thinking_max_tokens`` does a lazy
``from agent.gemini_native_adapter import _effective_gemini_max_output_tokens``
inside the function body. We register a stub ``agent.gemini_native_adapter``
module in ``sys.modules`` (pointing at our exec'd copy) so that import resolves
without loading the real runtime.

Run with the pinned interpreter:

    mise x python@3.12.13 -- python rust/tools/gen_gemini_thinking_goldens.py

Writes ``rust/tools/gemini-thinking-goldens.json`` next to this script. Pass
``--check`` to verify the checked-in fixture still matches Python.
"""
from __future__ import annotations

import ast
import json
import sys
import types
from pathlib import Path
from typing import Any, Dict, List, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]
CHAT = REPO_ROOT / "agent" / "transports" / "chat_completions.py"
ADAPTER = REPO_ROOT / "agent" / "gemini_native_adapter.py"
OUT = Path(__file__).resolve().parent / "gemini-thinking-goldens.json"

CHAT_WANTED = {
    "_build_gemini_thinking_config",
    "_snake_case_gemini_thinking_config",
    "_raise_gemini_thinking_max_tokens",
}
ADAPTER_WANTED = {
    "_normalize_thinking_config",
    "_thinking_requests_output_headroom",
    "_effective_gemini_max_output_tokens",
}


def _extract(source: Path, wanted: set[str]) -> List[str]:
    tree = ast.parse(source.read_text(encoding="utf-8"))
    segments: List[str] = []
    found: set[str] = set()
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name in wanted:
            segments.append(ast.unparse(node))
            found.add(node.name)
    missing = wanted - found
    if missing:
        raise SystemExit(f"could not find in {source.name}: {sorted(missing)}")
    return segments


def _constant(source: Path, name: str) -> Any:
    """Read a module-level ``NAME = <literal>`` assignment from source."""
    tree = ast.parse(source.read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name) and target.id == name:
                    return ast.literal_eval(node.value)
    raise SystemExit(f"could not find constant {name} in {source.name}")


def load_functions() -> Dict[str, Any]:
    """Extract the six target functions and exec them into one namespace."""
    namespace: Dict[str, Any] = {
        "Any": Any,
        "Dict": Dict,
        "List": List,
        "Optional": Optional,
        "GEMINI_DEFAULT_MAX_OUTPUT_TOKENS": _constant(
            ADAPTER, "GEMINI_DEFAULT_MAX_OUTPUT_TOKENS"
        ),
    }
    segments = _extract(CHAT, CHAT_WANTED) + _extract(ADAPTER, ADAPTER_WANTED)
    exec("\n\n".join(segments), namespace)  # noqa: S102 - trusted local source

    # _raise_gemini_thinking_max_tokens does a lazy import of the adapter's
    # _effective_gemini_max_output_tokens; point it at our exec'd copy so no
    # real runtime module loads.
    stub = types.ModuleType("agent.gemini_native_adapter")
    stub._effective_gemini_max_output_tokens = namespace[
        "_effective_gemini_max_output_tokens"
    ]
    agent_pkg = sys.modules.get("agent")
    if agent_pkg is None:
        agent_pkg = types.ModuleType("agent")
        agent_pkg.__path__ = []  # mark as a package
        sys.modules["agent"] = agent_pkg
    agent_pkg.gemini_native_adapter = stub
    sys.modules["agent.gemini_native_adapter"] = stub
    return namespace


def wrapper_cases() -> List[Dict[str, Any]]:
    """Model + reasoning_config + requested, exercising build/snake/raise."""
    return [
        {"name": "none-config", "model": "gemini-2.5-flash",
         "reasoning_config": None, "requested": 4096},
        {"name": "non-dict-config", "model": "gemini-2.5-flash",
         "reasoning_config": "high", "requested": 4096},
        {"name": "non-gemini-model", "model": "gpt-4",
         "reasoning_config": {"effort": "high"}, "requested": 4096},
        {"name": "gemma-model-omits", "model": "gemma-3-27b",
         "reasoning_config": {"effort": "high"}, "requested": 4096},
        {"name": "google-prefix-stripped", "model": "google/gemini-2.5-pro",
         "reasoning_config": {"effort": "high"}, "requested": 4096},
        {"name": "disabled-config", "model": "gemini-2.5-flash",
         "reasoning_config": {"enabled": False}, "requested": 4096},
        {"name": "disabled-config-raises-cap", "model": "gemini-2.5-flash",
         "reasoning_config": {"enabled": False}, "requested": 1024},
        {"name": "disabled-config-none-requested", "model": "gemini-2.5-flash",
         "reasoning_config": {"enabled": False}, "requested": None},
        {"name": "enabled-not-false-int", "model": "gemini-2.5-flash",
         "reasoning_config": {"enabled": 0, "effort": "high"}, "requested": 100},
        {"name": "effort-none-string", "model": "gemini-2.5-flash",
         "reasoning_config": {"effort": "none"}, "requested": 4096},
        {"name": "gemini-25-ignores-level", "model": "gemini-2.5-pro",
         "reasoning_config": {"effort": "high"}, "requested": 100000},
        {"name": "gemini-3-flash-low", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "minimal"}, "requested": 4096},
        {"name": "gemini-3-flash-high", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "ultra"}, "requested": 4096},
        {"name": "gemini-3-flash-medium", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "medium"}, "requested": 4096},
        {"name": "gemini-3-pro-high", "model": "gemini-3-pro",
         "reasoning_config": {"effort": "high"}, "requested": 4096},
        {"name": "gemini-3-pro-low", "model": "gemini-3-pro",
         "reasoning_config": {"effort": "low"}, "requested": 4096},
        {"name": "gemini-3-plain-no-level", "model": "gemini-3",
         "reasoning_config": {"effort": "high"}, "requested": 4096},
        {"name": "gemini-31-flash", "model": "gemini-3.1-flash",
         "reasoning_config": {"effort": "high"}, "requested": 4096},
        {"name": "effort-missing-defaults-medium", "model": "gemini-3-flash",
         "reasoning_config": {"foo": "bar"}, "requested": 4096},
        {"name": "effort-falsy-defaults-medium", "model": "gemini-3-flash",
         "reasoning_config": {"effort": ""}, "requested": 4096},
        {"name": "effort-unknown-defaults-medium", "model": "gemini-3-pro",
         "reasoning_config": {"effort": "bogus"}, "requested": 4096},
        {"name": "effort-uppercase-normalized", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "HIGH"}, "requested": 4096},
        {"name": "model-uppercase-normalized", "model": "GEMINI-2.5-FLASH",
         "reasoning_config": {"effort": "high"}, "requested": 4096},
        {"name": "requested-string-coerced", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "high"}, "requested": "2048"},
        {"name": "requested-float-truncated", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "high"}, "requested": 2048.9},
        {"name": "requested-invalid-string", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "high"}, "requested": "abc"},
        {"name": "requested-zero", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "high"}, "requested": 0},
        {"name": "requested-negative", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "high"}, "requested": -5},
        {"name": "requested-above-ceiling", "model": "gemini-3-flash",
         "reasoning_config": {"effort": "high"}, "requested": 999999},
    ]


def config_cases() -> List[Dict[str, Any]]:
    """Arbitrary thinking_config values for normalize + headroom."""
    return [
        {"name": "empty", "thinking_config": {}},
        {"name": "non-dict", "thinking_config": "nope"},
        {"name": "none", "thinking_config": None},
        {"name": "include-true", "thinking_config": {"includeThoughts": True}},
        {"name": "include-false", "thinking_config": {"includeThoughts": False}},
        {"name": "snake-include", "thinking_config": {"include_thoughts": True}},
        {"name": "level-only", "thinking_config": {"thinkingLevel": "  HIGH "}},
        {"name": "snake-level", "thinking_config": {"thinking_level": "Low"}},
        {"name": "level-blank", "thinking_config": {"thinkingLevel": "   "}},
        {"name": "budget-int", "thinking_config": {"thinkingBudget": 512}},
        {"name": "budget-float", "thinking_config": {"thinkingBudget": 512.9}},
        {"name": "budget-zero", "thinking_config": {"thinkingBudget": 0}},
        {"name": "budget-negative", "thinking_config": {"thinkingBudget": -1}},
        {"name": "snake-budget", "thinking_config": {"thinking_budget": 256}},
        {"name": "prefers-camel-budget",
         "thinking_config": {"thinkingBudget": 1, "thinking_budget": 999}},
        {"name": "false-with-level",
         "thinking_config": {"includeThoughts": False, "thinkingLevel": "low"}},
        {"name": "false-with-budget",
         "thinking_config": {"includeThoughts": False, "thinkingBudget": 100}},
        {"name": "false-with-zero-budget",
         "thinking_config": {"includeThoughts": False, "thinkingBudget": 0}},
        {"name": "budget-zero-no-level", "thinking_config": {"thinkingBudget": 0}},
        {"name": "budget-neg-with-level",
         "thinking_config": {"thinkingBudget": -3, "thinkingLevel": "high"}},
        {"name": "full", "thinking_config": {
            "includeThoughts": True, "thinkingLevel": "HIGH", "thinkingBudget": 42}},
    ]


def effective_cases() -> List[Dict[str, Any]]:
    """max_tokens + thinking_config for _effective_gemini_max_output_tokens."""
    hi = {"includeThoughts": True, "thinkingLevel": "high"}
    off = {"includeThoughts": False}
    return [
        {"name": "unsigned-json-cap", "max_tokens": 2**64 - 1, "thinking_config": hi},
        {"name": "unsigned-string-cap", "max_tokens": str(2**64 - 1), "thinking_config": off},
        {"name": "none-tokens", "max_tokens": None, "thinking_config": hi},
        {"name": "no-headroom-passthrough", "max_tokens": 2048,
         "thinking_config": off},
        {"name": "headroom-raises", "max_tokens": 1024, "thinking_config": hi},
        {"name": "headroom-keeps-larger", "max_tokens": 100000,
         "thinking_config": hi},
        {"name": "zero-tokens", "max_tokens": 0, "thinking_config": hi},
        {"name": "negative-tokens", "max_tokens": -10, "thinking_config": hi},
        {"name": "float-tokens", "max_tokens": 1024.9, "thinking_config": hi},
        {"name": "string-tokens", "max_tokens": "3000", "thinking_config": hi},
        {"name": "string-invalid", "max_tokens": "nope", "thinking_config": hi},
        {"name": "bool-tokens", "max_tokens": True, "thinking_config": hi},
        {"name": "list-tokens", "max_tokens": [1, 2], "thinking_config": hi},
        {"name": "no-config", "max_tokens": 2048, "thinking_config": None},
    ]


def main() -> None:
    ns = load_functions()
    build = ns["_build_gemini_thinking_config"]
    snake = ns["_snake_case_gemini_thinking_config"]
    raise_cap = ns["_raise_gemini_thinking_max_tokens"]
    normalize = ns["_normalize_thinking_config"]
    headroom = ns["_thinking_requests_output_headroom"]
    effective = ns["_effective_gemini_max_output_tokens"]

    wrapper = []
    for case in wrapper_cases():
        cfg = build(case["model"], case["reasoning_config"])
        wrapper.append({
            "name": case["name"],
            "model": case["model"],
            "reasoning_config": case["reasoning_config"],
            "requested": case["requested"],
            "build": cfg,
            "snake": snake(cfg),
            "raised": raise_cap(
                case["model"], case["reasoning_config"], case["requested"]
            ),
        })

    configs = []
    for case in config_cases():
        configs.append({
            "name": case["name"],
            "thinking_config": case["thinking_config"],
            "normalized": normalize(case["thinking_config"]),
            "headroom": headroom(case["thinking_config"]),
        })

    effectives = []
    for case in effective_cases():
        effectives.append({
            "name": case["name"],
            "max_tokens": case["max_tokens"],
            "thinking_config": case["thinking_config"],
            "expected": effective(case["max_tokens"], case["thinking_config"]),
        })

    payload = {"wrapper": wrapper, "configs": configs, "effective": effectives}
    content = json.dumps(payload, indent=2) + "\n"
    total = len(wrapper) + len(configs) + len(effectives)
    if sys.argv[1:] == ["--check"]:
        assert OUT.read_text(encoding="utf-8") == content, (
            "Gemini thinking fixtures differ from Python"
        )
    elif not sys.argv[1:]:
        OUT.write_text(content, encoding="utf-8")
    else:
        raise SystemExit("usage: gen_gemini_thinking_goldens.py [--check]")
    print(f"Verified {total} gemini thinking cases")


if __name__ == "__main__":
    main()
