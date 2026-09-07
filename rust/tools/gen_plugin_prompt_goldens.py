#!/usr/bin/env python3
"""Generate golden test fixtures for plugin prompt framing and recovery.

Exercises `agent/system_prompt.py::_restore_plugin_prompt_sections` and
`hermes_cli.plugins::format_system_prompt_sections` to produce authoritative
test fixtures for Rust `hermes-gateway::plugin_prompt`.

Usage:
    .venv/bin/python rust/tools/gen_plugin_prompt_goldens.py
    .venv/bin/python rust/tools/gen_plugin_prompt_goldens.py --check
"""
from __future__ import annotations

import ast
import dataclasses
import json
from pathlib import Path
import re
import sys
import types
from typing import Any, Dict, List

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

GOLDENS_PATH = REPO_ROOT / "rust/tools/plugin-prompt-goldens.json"


def _load_oracle():
    """Load recovery and formatting functions via direct import or AST extraction."""
    try:
        from agent.system_prompt import _restore_plugin_prompt_sections
        from hermes_cli.plugins import (
            PLUGIN_SECTIONS_END,
            PLUGIN_SECTIONS_START,
            RenderedPluginSystemPromptSection,
            format_system_prompt_section,
            format_system_prompt_sections,
        )

        return (
            _restore_plugin_prompt_sections,
            format_system_prompt_sections,
            format_system_prompt_section,
            RenderedPluginSystemPromptSection,
            PLUGIN_SECTIONS_START,
            PLUGIN_SECTIONS_END,
        )
    except Exception:
        plugins_path = REPO_ROOT / "hermes_cli/plugins.py"
        sys_path = REPO_ROOT / "agent/system_prompt.py"

        tree_p = ast.parse(plugins_path.read_text(encoding="utf-8"))
        needed_assigns_p = {
            "MAX_SYSTEM_PROMPT_SECTION_CHARS",
            "_SYSTEM_PROMPT_SECTION_ID_RE",
            "_SYSTEM_PROMPT_SECTION_HEADING_PREFIX",
            "PLUGIN_SECTIONS_START",
            "PLUGIN_SECTIONS_END",
        }
        p_nodes = []
        for node in tree_p.body:
            if isinstance(node, ast.Assign):
                if any(isinstance(t, ast.Name) and t.id in needed_assigns_p for t in node.targets):
                    p_nodes.append(node)
            elif isinstance(node, ast.ClassDef) and node.name == "RenderedPluginSystemPromptSection":
                p_nodes.append(node)
            elif isinstance(node, ast.FunctionDef) and node.name in {
                "format_system_prompt_section",
                "format_system_prompt_sections",
                "is_valid_system_prompt_section_id",
            }:
                p_nodes.append(node)

        ns: Dict[str, Any] = {
            "dataclass": dataclasses.dataclass,
            "re": re,
            "Any": Any,
            "List": list,
            "Dict": dict,
        }
        mod_p = ast.Module(body=p_nodes, type_ignores=[])
        exec(compile(mod_p, "hermes_cli/plugins.py", "exec"), ns)

        fake_plugins = types.ModuleType("hermes_cli.plugins")
        for k, v in ns.items():
            setattr(fake_plugins, k, v)
        sys.modules["hermes_cli.plugins"] = fake_plugins

        tree_s = ast.parse(sys_path.read_text(encoding="utf-8"))
        s_nodes = []
        for node in tree_s.body:
            if isinstance(node, ast.Assign) and any(
                isinstance(t, ast.Name) and t.id == "_PLUGIN_SECTION_FRAME_RE" for t in node.targets
            ):
                s_nodes.append(node)
            elif isinstance(node, ast.FunctionDef) and node.name == "_restore_plugin_prompt_sections":
                s_nodes.append(node)

        mod_s = ast.Module(body=s_nodes, type_ignores=[])
        exec(compile(mod_s, "agent/system_prompt.py", "exec"), ns)

        return (
            ns["_restore_plugin_prompt_sections"],
            ns["format_system_prompt_sections"],
            ns["format_system_prompt_section"],
            ns["RenderedPluginSystemPromptSection"],
            ns["PLUGIN_SECTIONS_START"],
            ns["PLUGIN_SECTIONS_END"],
        )


(
    _restore_plugin_prompt_sections,
    format_system_prompt_sections,
    format_system_prompt_section,
    RenderedPluginSystemPromptSection,
    PLUGIN_SECTIONS_START,
    PLUGIN_SECTIONS_END,
) = _load_oracle()


def _sec(section_id: str, content: str) -> RenderedPluginSystemPromptSection:
    return RenderedPluginSystemPromptSection(
        id=section_id,
        content=content,
        position="after_memory",
        plugin="persisted-prompt",
    )


def build_oracle_cases() -> List[Dict[str, Any]]:
    """Build approximately 30 oracle cases covering all specification requirements."""
    start = PLUGIN_SECTIONS_START
    end = PLUGIN_SECTIONS_END
    std_footer = "\n\nConversation started: Monday, September 07, 2026"

    prompts: List[str] = []

    # --- 1. Canonical Formatting & Ordering ---
    # Case 1: Single basic valid section
    s1 = format_system_prompt_sections([
        _sec("weather.display", "Current temperature in Tokyo: 22C, partly cloudy.")
    ])
    prompts.append(f"System prompt prefix\n\n{s1}{std_footer}")

    # Case 2: Multiple valid sections in deterministic order
    s2 = format_system_prompt_sections([
        _sec("calendar.events", "- 10:00 AM Standup\n- 2:00 PM Design Review"),
        _sec("git.status", "branch: feature/plugins\nclean: true"),
        _sec("notes.scratch", "Remember to check edge cases."),
    ])
    prompts.append(f"Stable identity\n\n{s2}{std_footer}")

    # Case 3: Section containing multiline markdown formatting (fences, tables)
    code_text = (
        "### Implementation\n"
        "```rust\n"
        "fn main() {\n"
        "    println!(\"hello\");\n"
        "}\n"
        "```\n"
        "| Key | Val |\n"
        "| --- | --- |\n"
        "| a   | 1   |"
    )
    s3 = format_system_prompt_sections([_sec("code.snippet", code_text)])
    prompts.append(f"Prefix\n\n{s3}{std_footer}")

    # Case 4: Prefix instructions and rich multi-field footer
    rich_footer = "\n\nConversation started: Tuesday, October 01, 2026\nModel: claude-3-5-sonnet\nPlatform: cli"
    s4 = format_system_prompt_sections([_sec("user.preferences", "Theme: dark\nLanguage: en-US")])
    prompts.append(f"System prompt stable\nContext files loaded\n\n{s4}{rich_footer}")

    # --- 2. Unicode Character Length & Multi-byte UTF-8 ---
    # Case 5: Multi-byte CJK and emoji characters (char count vs byte length)
    s5 = format_system_prompt_sections([_sec("sample.notes", "你好 🦀\nkeep trailing space ")])
    prompts.append(f"stable\n\n{s5}{std_footer}")

    # Case 6: Accented and international characters
    s6 = format_system_prompt_sections([
        _sec("i18n.locales", "Café, résumé, naïve, München, Москва, القاهرة, ∑(x) ≥ 0, 100%, OK!")
    ])
    prompts.append(f"prefix\n\n{s6}{std_footer}")

    # Case 7: Unicode byte count declared instead of char count (byte count 33 vs char count 25)
    unicode_text = "你好 🦀\nkeep trailing space "
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: sample.notes\n"
        f"<!-- hermes-plugin-section-chars:33 -->\n\n{unicode_text}\n{end}{std_footer}"
    )

    # --- 3. Identifier Validation ---
    # Case 8: Boundary minimum length ID (1 character)
    s8 = format_system_prompt_sections([_sec("a", "Minimal length section identifier")])
    prompts.append(f"prefix\n\n{s8}{std_footer}")

    # Case 9: Boundary maximum length ID (128 characters)
    max_id = "x" * 128
    s9 = format_system_prompt_sections([_sec(max_id, "Max length section identifier")])
    prompts.append(f"prefix\n\n{s9}{std_footer}")

    # Case 10: Allowed special symbols in ID (dots, hyphens, underscores, digits)
    s10 = format_system_prompt_sections([
        _sec("my-org.plugin_service-v2.1", "Identifier with special allowed chars")
    ])
    prompts.append(f"prefix\n\n{s10}{std_footer}")

    # Case 11: Invalid ID: uppercase letters rejected
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: Weather.Display\n"
        f"<!-- hermes-plugin-section-chars:4 -->\n\ntest\n{end}{std_footer}"
    )

    # Case 12: Invalid ID: leading dot rejected
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: .weather\n"
        f"<!-- hermes-plugin-section-chars:4 -->\n\ntest\n{end}{std_footer}"
    )

    # Case 13: Invalid ID: disallowed character slash rejected
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: weather/plugin\n"
        f"<!-- hermes-plugin-section-chars:4 -->\n\ntest\n{end}{std_footer}"
    )

    # Case 14: Invalid ID: 129 characters exceeds maximum 128
    too_long_id = "x" * 129
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: {too_long_id}\n"
        f"<!-- hermes-plugin-section-chars:4 -->\n\ntest\n{end}{std_footer}"
    )

    # --- 4. Truncated & Wrong Framing ---
    # Case 15: Single newline after chars comment instead of double newline
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: valid.id\n"
        f"<!-- hermes-plugin-section-chars:4 -->\ntest\n{end}{std_footer}"
    )

    # Case 16: Extra space inside comment framing rejected
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: valid.id\n"
        f"<!-- hermes-plugin-section-chars: 4 -->\n\ntest\n{end}{std_footer}"
    )

    # Case 17: Single hash heading instead of double hash rejected
    prompts.append(
        f"prefix\n\n{start}\n# Plugin Context: valid.id\n"
        f"<!-- hermes-plugin-section-chars:4 -->\n\ntest\n{end}{std_footer}"
    )

    # Case 18: Truncated content: declared 10 chars but only 5 chars present
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: valid.id\n"
        f"<!-- hermes-plugin-section-chars:10 -->\n\nhello\n{end}{std_footer}"
    )

    # Case 19: Content longer than declared before end marker (declared 5, 10 present)
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: valid.id\n"
        f"<!-- hermes-plugin-section-chars:5 -->\n\nhelloworld\n{end}{std_footer}"
    )

    # Case 20: Truncated prompt ending abruptly before end marker
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: valid.id\n<!-- hermes-plugin-section-chars:4 -->\n\ntest\n"
    )

    # Case 21: Altered start marker (missing s)
    prompts.append(
        f"prefix\n\n<!-- hermes-plugin-section:start -->\n## Plugin Context: valid.id\n"
        f"<!-- hermes-plugin-section-chars:4 -->\n\ntest\n{end}{std_footer}"
    )

    # Case 22: Altered end marker (stop instead of end)
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: valid.id\n<!-- hermes-plugin-section-chars:4 -->\n\ntest\n"
        f"<!-- hermes-plugin-sections:stop -->{std_footer}"
    )

    # --- 5. Nested Headers & Lookalike Injection ---
    # Case 23: Nested header spoofing inside section content rejected
    spoof_content = (
        "Config:\n"
        "## Plugin Context: nested.spoof\n"
        "<!-- hermes-plugin-section-chars:4 -->\n\n"
        "evil"
    )
    s23 = format_system_prompt_sections([_sec("outer.plugin", spoof_content)])
    prompts.append(f"prefix\n\n{s23}{std_footer}")

    # Case 24: Nested end marker inside section content rejected
    nested_end_content = f"Example tag: {end}\nMore text"
    s24 = format_system_prompt_sections([_sec("outer.plugin", nested_end_content)])
    prompts.append(f"prefix\n\n{s24}{std_footer}")

    # --- 6. Duplicate Start Markers & Last-Start Rule ---
    valid_block = format_system_prompt_sections([_sec("recovered.plugin", "Recovered payload")])

    # Case 25: Stale duplicate start marker before valid block (accepted by last-start rule)
    prompts.append(f"{start}\ncorrupt old prompt\n\n{valid_block}{std_footer}")

    # Case 26: Trailing unclosed duplicate start marker after valid block rejected
    prompts.append(f"{valid_block}\n\n{start}{std_footer}")

    # Case 27: Two valid framed blocks: last-start rule selects second block
    valid_block_1 = format_system_prompt_sections([_sec("block1.plugin", "Payload one")])
    valid_block_2 = format_system_prompt_sections([_sec("block2.plugin", "Payload two")])
    prompts.append(f"{valid_block_1}\n\n{valid_block_2}{std_footer}")

    # --- 7. Timeless Footer Rejection ---
    # Case 28: Timeless footer starting with Timezone: rejected
    prompts.append(f"{valid_block}\n\nTimezone: America/New_York\nModel: gpt-4o")

    # Case 29: Timeless footer starting with Session ID: rejected
    prompts.append(f"{valid_block}\n\nSession ID: 550e8400-e29b-41d4-a716-446655440000")

    # Case 30: Single newline before Conversation started: rejected
    prompts.append(f"{valid_block}\nConversation started: Monday, September 07, 2026")

    # Case 31: Space after end marker before footer rejected
    prompts.append(f"{valid_block} \n\nConversation started: Monday, September 07, 2026")

    # --- 8. Length Limits (4000 chars) ---
    # Case 32: Exactly 4,000 characters content accepted
    s32 = format_system_prompt_sections([_sec("limit.exact4000", "x" * 4000)])
    prompts.append(f"prefix\n\n{s32}{std_footer}")

    # Case 33: 4,001 characters content rejected
    s33 = format_system_prompt_sections([_sec("limit.over4000", "x" * 4001)])
    prompts.append(f"prefix\n\n{s33}{std_footer}")

    # Case 34: 5-digit character count header rejected
    big_10000 = "z" * 10000
    prompts.append(
        f"prefix\n\n{start}\n## Plugin Context: limit.big\n"
        f"<!-- hermes-plugin-section-chars:10000 -->\n\n{big_10000}\n{end}{std_footer}"
    )

    # --- 9. Missing Footer ---
    # Case 35: Prompt ends immediately at end marker without footer rejected
    prompts.append(f"prefix\n\n{valid_block}")

    # Case 36: Completely empty prompt string rejected
    prompts.append("")

    # --- 10. Empty Sections & Containers ---
    # Case 37: Section with empty content accepted
    s37 = format_system_prompt_sections([_sec("empty.section", "")])
    prompts.append(f"prefix\n\n{s37}{std_footer}")

    # Case 38: Multiple sections including an empty section accepted
    s38 = format_system_prompt_sections([
        _sec("active.first", "first payload"),
        _sec("empty.middle", ""),
        _sec("active.last", "last payload"),
    ])
    prompts.append(f"prefix\n\n{s38}{std_footer}")

    # Case 39: Empty container without any sections rejected
    prompts.append(f"prefix\n\n{start}\n{end}{std_footer}")

    # Build authoritative oracle output
    cases: List[Dict[str, Any]] = []
    for prompt_str in prompts:
        restored = _restore_plugin_prompt_sections(prompt_str)
        expected = [{"id": s.id, "content": s.content} for s in restored]
        cases.append({
            "prompt": prompt_str,
            "expected": expected,
        })

    return cases


def generate_goldens_json() -> str:
    cases = build_oracle_cases()
    return json.dumps(cases, indent=2, ensure_ascii=False) + "\n"


def main():
    content = generate_goldens_json()
    cases = json.loads(content)

    if sys.argv[1:] == ["--check"]:
        if not GOLDENS_PATH.exists():
            raise SystemExit(f"Golden file not found at {GOLDENS_PATH}")
        existing = GOLDENS_PATH.read_text(encoding="utf-8")
        assert existing == content, "Goldens mismatch under --check"
        print(f"Verified {len(cases)} plugin prompt cases match {GOLDENS_PATH}")
    elif not sys.argv[1:]:
        GOLDENS_PATH.write_text(content, encoding="utf-8")
        print(f"Generated {len(cases)} plugin prompt cases to {GOLDENS_PATH}")
    else:
        raise SystemExit("usage: gen_plugin_prompt_goldens.py [--check]")


if __name__ == "__main__":
    main()
