#!/usr/bin/env python3
"""Extract static guidance constants for the Rust system-prompt assembler.

Parses agent/system_prompt.py to locate all imports from agent/prompt_builder.py,
then extracts static constants using AST analysis without importing runtime
modules. The two memory aliases evaluate the reviewed pure string builder.

Preserves exact string bytes, unicode sequences, ordered lists, and platform
maps. Remaining computed imports are reported in
rust/analysis/system-prompt-guidance-notes.md rather than guessed.

Usage:
    mise exec python@3.12.13 -- python3 rust/tools/gen_system_prompt_guidance.py [--check]
"""
from __future__ import annotations

import ast
import json
from pathlib import Path
import sys
from typing import Any

REPO = Path(__file__).resolve().parents[2]
SYSTEM_PROMPT_PATH = REPO / "agent/system_prompt.py"
PROMPT_BUILDER_PATH = REPO / "agent/prompt_builder.py"
OUT_JSON = REPO / "rust/tools/system-prompt-guidance.json"
OUT_NOTES = REPO / "rust/analysis/system-prompt-guidance-notes.md"


class ComputedImportError(Exception):
    """Raised when an imported symbol cannot be resolved to a static literal."""

    def __init__(self, message: str, node: ast.AST):
        super().__init__(message)
        self.node = node


def find_prompt_builder_imports(system_prompt_path: Path) -> list[tuple[str, int]]:
    """Locate all imports from agent.prompt_builder in agent/system_prompt.py in order."""
    tree = ast.parse(system_prompt_path.read_text(encoding="utf-8"), filename=str(system_prompt_path))
    imports: list[tuple[str, int]] = []
    for node in tree.body:
        for sub in ast.walk(node):
            if isinstance(sub, ast.ImportFrom) and sub.module == "agent.prompt_builder":
                for alias in sub.names:
                    imports.append((alias.name, sub.lineno))
    return imports


def index_prompt_builder_definitions(prompt_builder_path: Path) -> dict[str, ast.AST]:
    """Index all top-level assignments and functions in agent/prompt_builder.py."""
    tree = ast.parse(prompt_builder_path.read_text(encoding="utf-8"), filename=str(prompt_builder_path))
    defs: dict[str, ast.AST] = {}
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    defs[target.id] = node.value
        elif isinstance(node, ast.FunctionDef):
            defs[node.name] = node
    return defs


def eval_ast_constant(node: ast.AST, defs: dict[str, ast.AST], env: dict[str, Any]) -> Any:
    """Evaluate an AST expression node into a literal Python data structure."""
    if isinstance(node, ast.Constant):
        return node.value
    elif isinstance(node, (ast.Tuple, ast.List)):
        return [eval_ast_constant(elt, defs, env) for elt in node.elts]
    elif isinstance(node, ast.Dict):
        res: dict[str, Any] = {}
        for k, v in zip(node.keys, node.values):
            if k is None:
                raise ComputedImportError("Dict unpacking (**) is not supported in literal constants", node)
            key = eval_ast_constant(k, defs, env)
            val = eval_ast_constant(v, defs, env)
            res[key] = val
        return res
    elif isinstance(node, ast.Name):
        if node.id in env:
            return env[node.id]
        if node.id in defs:
            val = eval_ast_constant(defs[node.id], defs, env)
            env[node.id] = val
            return val
        raise ComputedImportError(f"Name '{node.id}' cannot be resolved to a static constant", node)
    elif isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        left = eval_ast_constant(node.left, defs, env)
        right = eval_ast_constant(node.right, defs, env)
        if isinstance(left, str) and isinstance(right, str):
            return left + right
        elif isinstance(left, list) and isinstance(right, list):
            return left + right
        else:
            raise ComputedImportError(f"Unsupported BinOp Add operands: {type(left)} and {type(right)}", node)
    elif isinstance(node, ast.JoinedStr):
        parts: list[str] = []
        for part in node.values:
            if isinstance(part, ast.Constant):
                parts.append(str(part.value))
            elif isinstance(part, ast.FormattedValue):
                parts.append(str(eval_ast_constant(part.value, defs, env)))
            else:
                raise ComputedImportError(f"JoinedStr contains non-constant element: {type(part).__name__}", part)
        return "".join(parts)
    elif isinstance(node, ast.Call):
        func_name = getattr(node.func, "id", ast.dump(node.func))
        raise ComputedImportError(f"Function call expression '{func_name}(...)' is computed rather than literal", node)
    elif isinstance(node, ast.FunctionDef):
        raise ComputedImportError(f"Function definition '{node.name}' is a runtime function, not a constant", node)
    else:
        raise ComputedImportError(f"Node type '{type(node).__name__}' is computed rather than literal", node)


def build_notes_markdown(
    static_constants: dict[str, Any],
    computed_imports: dict[str, dict[str, Any]],
) -> str:
    """Format markdown analysis reporting computed imports and describing static constants."""
    lines: list[str] = [
        "# System Prompt Guidance Notes & AST Import Analysis",
        "",
        "This document records the AST extraction analysis performed by",
        "`rust/tools/gen_system_prompt_guidance.py` on `agent/system_prompt.py` and",
        "`agent/prompt_builder.py`. It documents both the extracted static constants and",
        "the non-literal / computed imports that cannot be evaluated at parse time without",
        "guessing runtime behavior.",
        "",
        "## Summary",
        "",
        f"- **Total imports from `agent.prompt_builder`**: {len(static_constants) + len(computed_imports)}",
        f"- **Static constants extracted**: {len(static_constants)} (written to `rust/tools/system-prompt-guidance.json`)",
        f"- **Computed / non-literal imports reported**: {len(computed_imports)}",
        "",
        "## Extracted Static Constants",
        "",
        "These constants are literal or constant-folded string/list/dict definitions,",
        "plus MEMORY_GUIDANCE and USER_PROFILE_GUIDANCE, whose aliases evaluate the",
        "reviewed pure build_memory_guidance function with literal booleans. No runtime",
        "modules are imported. The results are extracted into",
        "`rust/tools/system-prompt-guidance.json` with exact string bytes, unicode sequences,",
        "and element order preserved:",
        "",
        "| Constant Name | Data Type | Size / Length | Purpose / Tier |",
        "|---|---|---|---|",
    ]

    for name, val in static_constants.items():
        if isinstance(val, list):
            type_desc = "Array (ordered)"
            size_desc = f"{len(val)} items"
        elif isinstance(val, dict):
            type_desc = "Map (ordered)"
            size_desc = f"{len(val)} entries"
        elif isinstance(val, str):
            type_desc = "String (UTF-8)"
            size_desc = f"{len(val)} chars ({len(val.encode('utf-8'))} bytes)"
        else:
            type_desc = type(val).__name__
            size_desc = str(len(val)) if hasattr(val, "__len__") else "-"

        tier = "Tier 1 (Stable)"
        if name == "PLATFORM_HINTS":
            tier = "Tier 1 / Context (Platform hint)"
        elif name in ("EXECUTION_GUIDANCE_MODELS", "TOOL_USE_ENFORCEMENT_MODELS"):
            tier = "Tier 1 (Model-family gate)"

        lines.append(f"| `{name}` | {type_desc} | {size_desc} | {tier} |")

    lines.extend([
        "",
        "## Computed / Non-Literal Imports Analysis",
        "",
        "The following symbols imported by `agent/system_prompt.py` from `agent/prompt_builder.py`",
        "are computed rather than literal in Python AST. Rather than guessing their runtime values",
        "or executing unconstrained Python code during AST extraction, their AST structure,",
        "computational nature, and Rust port recommendations are documented below:",
        "",
    ])

    for name, info in computed_imports.items():
        lineno = info["lineno"]
        node_type = info["node_type"]
        reason = info["reason"]
        lines.extend([
            f"### `{name}`",
            "",
            f"- **Import Site**: `agent/system_prompt.py:{lineno}`",
            f"- **AST Node Type**: `{node_type}`",
            f"- **Classification Reason**: {reason}",
        ])

        if name == "MEMORY_GUIDANCE":
            lines.extend([
                "- **Definition in `prompt_builder.py:303`**:",
                "  ```python",
                "  MEMORY_GUIDANCE = build_memory_guidance(True, True)",
                "  ```",
                "- **Analysis**: In `agent/prompt_builder.py`, `MEMORY_GUIDANCE` is an alias created by",
                "  calling the function `build_memory_guidance(memory_enabled=True, profile_enabled=True)`. In",
                "  `agent/system_prompt.py:530-536`, `MEMORY_GUIDANCE` is injected only when `\"memory\"` is",
                "  in `valid_tool_names` and `agent._memory_enabled` is true.",
                "- **Rust Tier Assembler Recommendation**: In the native Rust prompt assembler, do not",
                "  treat memory guidance as a single fixed static text. Instead, implement a dynamic function",
                "  `build_memory_guidance(memory_enabled: bool, profile_enabled: bool) -> Option<&'static str>`",
                "  (or returning `String`), mirroring the configuration gating in Python.",
            ])
        elif name == "USER_PROFILE_GUIDANCE":
            lines.extend([
                "- **Definition in `prompt_builder.py:305`**:",
                "  ```python",
                "  USER_PROFILE_GUIDANCE = build_memory_guidance(False, True)",
                "  ```",
                "- **Analysis**: In `agent/prompt_builder.py`, `USER_PROFILE_GUIDANCE` is an alias created by",
                "  calling `build_memory_guidance(memory_enabled=False, profile_enabled=True)`. It is injected",
                "  when `agent._memory_enabled` is false but `agent._user_profile_enabled` is true.",
                "- **Rust Tier Assembler Recommendation**: Reuses the native Rust `build_memory_guidance`",
                "  implementation with `memory_enabled: false, profile_enabled: true`.",
            ])
        elif name == "drain_truncation_warnings":
            lines.extend([
                "- **Definition in `prompt_builder.py:1585`**:",
                "  ```python",
                "  def drain_truncation_warnings() -> list:",
                "  ```",
                "- **Analysis**: This is a runtime helper function, not a constant. It mutates and drains",
                "  the module-level `_TRUNCATION_WARNINGS` queue populated during context file reads.",
                "- **Rust Tier Assembler Recommendation**: Truncation warnings should be tracked as part of",
                "  context file discovery state in Rust and emitted through the agent status channel during assembly.",
            ])
        elif name == "execution_guidance_text":
            lines.extend([
                "- **Definition in `prompt_builder.py:672`**:",
                "  ```python",
                "  def execution_guidance_text(valid_tool_names=None) -> str:",
                "  ```",
                "- **Analysis**: This is a runtime formatting function, not a constant. It transforms",
                "  `OPENAI_MODEL_EXECUTION_GUIDANCE` by conditionally stripping out references to `web_search`",
                "  when `web_search` is not present in `valid_tool_names`.",
                "- **Rust Tier Assembler Recommendation**: Implement `execution_guidance_text` as a Rust helper:",
                "  ```rust",
                "  pub fn execution_guidance_text(valid_tool_names: Option<&HashSet<String>>) -> String",
                "  ```",
                "  which applies the string replacements onto the static constant",
                "  `OPENAI_MODEL_EXECUTION_GUIDANCE` extracted in `system-prompt-guidance.json`.",
            ])

        lines.append("")

    lines.extend([
        "## Invariants & Preservation Guarantees",
        "",
        "1. **Zero Runtime Imports**: Extracted strictly via Python's standard `ast` library.",
        "   No project modules (e.g. `agent.prompt_builder`, `utils`, `yaml`) are imported.",
        "2. **Byte-Level String Fidelity**: Preserves all multi-line indentation, markdown syntax,",
        "   control characters, and unicode characters (`—`, `•`, `≤`, `企业微信`, `腾讯元宝`, etc.).",
        "3. **Ordered Preservation**: Preserves the exact ordering of keys in `PLATFORM_HINTS` (21 platforms),",
        "   model families in `EXECUTION_GUIDANCE_MODELS` (11 models) and `TOOL_USE_ENFORCEMENT_MODELS` (9 models),",
        "   and the top-level import ordering from `agent/system_prompt.py`.",
        "",
    ])

    return "\n".join(lines)


def generate() -> tuple[str, str]:
    """Execute AST extraction and return serialized JSON and markdown notes."""
    imports = find_prompt_builder_imports(SYSTEM_PROMPT_PATH)
    defs = index_prompt_builder_definitions(PROMPT_BUILDER_PATH)

    env: dict[str, Any] = {}
    static_constants: dict[str, Any] = {}
    computed_imports: dict[str, dict[str, Any]] = {}

    for name, lineno in imports:
        if name not in defs:
            computed_imports[name] = {
                "lineno": lineno,
                "node_type": "Missing",
                "reason": "Not defined as top-level statement in agent/prompt_builder.py",
            }
            continue

        node = defs[name]
        try:
            val = eval_ast_constant(node, defs, env)
            static_constants[name] = val
        except ComputedImportError as e:
            if name in {"MEMORY_GUIDANCE", "USER_PROFILE_GUIDANCE"}:
                # These two aliases call a reviewed pure string builder with
                # literal booleans. Execute only that AST function and alias;
                # do not import prompt_builder or evaluate arbitrary calls.
                namespace = {}
                function = defs["build_memory_guidance"]
                exec(compile(ast.Module(body=[function], type_ignores=[]), str(PROMPT_BUILDER_PATH), "exec"), namespace)
                static_constants[name] = eval(compile(ast.Expression(node), str(PROMPT_BUILDER_PATH), "eval"), namespace)
                continue
            computed_imports[name] = {
                "lineno": lineno,
                "node_type": type(node).__name__,
                "reason": str(e),
            }

    json_content = json.dumps(static_constants, indent=2, ensure_ascii=False) + "\n"
    notes_content = build_notes_markdown(static_constants, computed_imports)
    return json_content, notes_content


def main() -> int:
    json_content, notes_content = generate()

    if sys.argv[1:] == ["--check"]:
        if not OUT_JSON.exists():
            raise SystemExit(f"Missing output file: {OUT_JSON}")
        if not OUT_NOTES.exists():
            raise SystemExit(f"Missing notes file: {OUT_NOTES}")
        if OUT_JSON.read_text(encoding="utf-8") != json_content:
            raise SystemExit(f"{OUT_JSON} differs from AST-extracted constants")
        if OUT_NOTES.read_text(encoding="utf-8") != notes_content:
            raise SystemExit(f"{OUT_NOTES} differs from AST-extracted analysis notes")
        data = json.loads(json_content)
        print(f"Verified {len(data)} static guidance constants in {OUT_JSON} and analysis notes in {OUT_NOTES}")
        return 0
    elif not sys.argv[1:]:
        OUT_JSON.parent.mkdir(parents=True, exist_ok=True)
        OUT_NOTES.parent.mkdir(parents=True, exist_ok=True)
        OUT_JSON.write_text(json_content, encoding="utf-8")
        OUT_NOTES.write_text(notes_content, encoding="utf-8")
        data = json.loads(json_content)
        print(f"Generated {len(data)} static guidance constants into {OUT_JSON} and notes into {OUT_NOTES}")
        return 0
    else:
        raise SystemExit("Usage: gen_system_prompt_guidance.py [--check]")


if __name__ == "__main__":
    raise SystemExit(main())
