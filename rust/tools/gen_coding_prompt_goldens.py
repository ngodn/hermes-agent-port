#!/usr/bin/env python3
"""Generate golden test fixtures for bounded coding prompt rendering.

Extracts literal constants directly from `agent/coding_context.py` and runs the
actual Python `RuntimeMode.system_prompt_parts` oracle to generate byte-exact
test fixtures for `rust/crates/hermes-gateway/src/coding_prompt.rs`.

Usage:
    python3 rust/tools/gen_coding_prompt_goldens.py
    python3 rust/tools/gen_coding_prompt_goldens.py --check
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

import agent.coding_context as cc
from agent.coding_context import (
    CODING_AGENT_GUIDANCE,
    CODING_PROFILE,
    CODING_TOOLSET,
    GENERAL_PROFILE,
    ContextProfile,
    RuntimeMode,
    _EDIT_FORMAT_GUIDANCE,
    _NON_CODING_SKILL_CATEGORIES,
    _edit_format_line,
    _model_family,
    get_profile,
)

GOLDENS_PATH = REPO_ROOT / "rust/tools/coding-prompt-goldens.json"

TODO_TARGET = (
    "- Track multi-step work with `todo_list`. Reference code as "
    "`path:line` instead of pasting whole files."
)
TODO_REPLACEMENT = (
    "- Reference code as `path:line` instead of pasting whole files."
)
OPERATOR_INSTRUCTIONS_HEADER = "Operator instructions (from config):\n"


def run_oracle(
    posture="coding",
    model=None,
    valid_tool_names=None,
    instructions="",
    workspace_text="",
) -> dict[str, list[str]]:
    """Execute Python's RuntimeMode.system_prompt_parts oracle."""
    if isinstance(posture, str):
        profile = get_profile(posture)
    elif isinstance(posture, bool):
        profile = CODING_PROFILE if posture else GENERAL_PROFILE
    elif isinstance(posture, dict):
        profile = ContextProfile(**posture)
    else:
        profile = posture

    rm = RuntimeMode(
        profile=profile,
        surface="cli",
        cwd=Path("/unused"),
        config_mode="auto",
        model=model,
        instructions=instructions or "",
    )

    orig_ws = cc.build_coding_workspace_block
    try:
        cc.build_coding_workspace_block = lambda cwd: workspace_text or ""
        prefix, workspace, trailing = rm.system_prompt_parts(
            valid_tool_names=valid_tool_names
        )
        return {
            "prefix": prefix,
            "workspace": workspace,
            "trailing": trailing,
        }
    finally:
        cc.build_coding_workspace_block = orig_ws


def generate_goldens() -> dict:
    # 1. Constants directly extracted from Python
    constants = {
        "coding_toolset": CODING_TOOLSET,
        "coding_agent_guidance": CODING_AGENT_GUIDANCE,
        "non_coding_skill_categories": list(_NON_CODING_SKILL_CATEGORIES),
        "edit_format_guidance": {
            family: {
                "needles": list(needles),
                "line": line,
            }
            for family, (needles, line) in _EDIT_FORMAT_GUIDANCE.items()
        },
        "todo_replacement_target": TODO_TARGET,
        "todo_replacement_value": TODO_REPLACEMENT,
        "operator_instructions_header": OPERATOR_INSTRUCTIONS_HEADER,
    }

    # 2. Model family classification cases
    test_models = [
        # Patch models
        "gpt-4o",
        "openai/gpt-5-preview",
        "codex-rs",
        "openai/codex",
        "GPT-4",
        "CODEX",
        # Replace models
        "claude-3-5-sonnet-20241022",
        "anthropic/claude-opus-4.8",
        "claude-3-haiku",
        "google/gemini-2.5-flash",
        "gemini-pro",
        "google/gemma-3-27b-it",
        "gemma-2-9b",
        "deepseek/deepseek-chat",
        "deepseek-coder",
        "qwen/qwen-2.5-coder-32b",
        "moonshot/kimi-k1.5",
        "zhipu/glm-4-plus",
        "xai/grok-2",
        "nousresearch/hermes-3-llama-3.1-405b",
        "meta-llama/llama-3.3-70b-instruct",
        "mistralai/mistral-large",
        "mistralai/devstral-small",
        "minimax/minimax-text-01",
        "CLAUDE-3.5-SONNET",
        "GEMINI-1.5-PRO",
        "QWEN-CODER",
        # Precedence test: contains both gpt and claude -> patch wins
        "gpt-4-with-claude-fallback",
        # Unknown / empty / None models
        "custom-fine-tuned-model",
        "bert-base-uncased",
        "unrecognized-llm",
        "",
        "   ",
        None,
    ]

    model_family_cases = []
    for m in test_models:
        fam = _model_family(m)
        line = _edit_format_line(m)
        model_family_cases.append({
            "model": m,
            "expected_family": fam,
            "expected_line": line,
        })

    # 3. Prompt parts cases
    prompt_cases_spec = [
        {
            "id": "coding_standard_claude_all_fields",
            "posture": "coding",
            "model": "anthropic/claude-3-5-sonnet-20241022",
            "valid_tool_names": ["read_file", "write_file", "patch", "terminal", "todo_list"],
            "instructions": "Never commit directly to main. Run tests first.",
            "workspace_text": "Workspace (snapshot at session start — re-check with `git` before acting on it):\n- Root: /home/user/project\n- Branch: main\n- Status: clean",
        },
        {
            "id": "coding_claude_no_todo_tool",
            "posture": "coding",
            "model": "claude-3-haiku",
            "valid_tool_names": ["read_file", "write_file", "patch", "terminal"],
            "instructions": "Do not modify lockfiles.",
            "workspace_text": "Workspace (snapshot at session start — re-check with `git` before acting on it):\n- Root: /home/user/project\n- Branch: dev\n- Status: 1 modified",
        },
        {
            "id": "coding_gpt_patch_with_todo",
            "posture": "coding",
            "model": "openai/gpt-4o",
            "valid_tool_names": ["read_file", "write_file", "patch", "todo_list"],
            "instructions": "Follow Rust conventions.",
            "workspace_text": "Workspace (snapshot at session start — re-check with `git` before acting on it):\n- Root: /app\n- Branch: (detached HEAD)\n- Status: clean",
        },
        {
            "id": "coding_codex_tools_none",
            "posture": "coding",
            "model": "codex-rs",
            "valid_tool_names": None,
            "instructions": "",
            "workspace_text": "",
        },
        {
            "id": "coding_unknown_model",
            "posture": "coding",
            "model": "custom-fine-tuned-model",
            "valid_tool_names": ["read_file", "write_file"],
            "instructions": "Keep functions small.",
            "workspace_text": "Workspace (snapshot at session start — re-check with `git` before acting on it):\n- Root: /workspace",
        },
        {
            "id": "coding_no_model",
            "posture": "coding",
            "model": None,
            "valid_tool_names": ["read_file", "todo_list"],
            "instructions": "",
            "workspace_text": "",
        },
        {
            "id": "coding_empty_model_string",
            "posture": "coding",
            "model": "",
            "valid_tool_names": ["read_file"],
            "instructions": "Strict typing required.",
            "workspace_text": "Workspace (snapshot at session start — re-check with `git` before acting on it):\n- Root: /code",
        },
        {
            "id": "coding_empty_tools_list",
            "posture": "coding",
            "model": "gemini-2.5-flash",
            "valid_tool_names": [],
            "instructions": "",
            "workspace_text": "",
        },
        {
            "id": "coding_multiline_instructions_and_complex_workspace",
            "posture": "coding",
            "model": "deepseek-coder",
            "valid_tool_names": ["read_file", "patch"],
            "instructions": "1. Format before committing.\n2. Do not delete test data.\n3. Always add documentation.",
            "workspace_text": (
                "Workspace (snapshot at session start — re-check with `git` before acting on it):\n"
                "- Root: /srv/project\n"
                "- Branch: feature/auth → origin/feature/auth (ahead 1, behind 0)\n"
                "- Status: 2 staged, 1 modified, 3 untracked\n"
                "- Recent commits:\n"
                "    a1b2c3d Add login handler\n"
                "    e4f5g6h Fix session cookie\n"
                "- Project: Cargo.toml (cargo)\n"
                "- Verify: cargo test; cargo clippy\n"
                "- Context files: AGENTS.md, CLAUDE.md"
            ),
        },
        {
            "id": "general_posture_ignored_inputs",
            "posture": "general",
            "model": "claude-3-5-sonnet",
            "valid_tool_names": ["read_file", "write_file"],
            "instructions": "This should not be rendered.",
            "workspace_text": "Workspace:\n- Root: /repo",
        },
        {
            "id": "general_posture_empty_inputs",
            "posture": "general",
            "model": None,
            "valid_tool_names": None,
            "instructions": "",
            "workspace_text": "",
        },
        {
            "id": "coding_custom_profile_empty_guidance",
            "posture": {
                "name": "coding",
                "toolset": "coding",
                "guidance": "",
                "model_hint": "coding",
                "memory_policy": "project",
                "compact_skill_categories": [],
            },
            "model": "claude-3-5-sonnet",
            "valid_tool_names": ["read_file"],
            "instructions": "Some instructions",
            "workspace_text": "Workspace:\n- Root: /workspace",
        },
        {
            "id": "coding_custom_profile_custom_guidance",
            "posture": {
                "name": "coding",
                "toolset": "coding",
                "guidance": "Custom paired programming guidelines.",
                "model_hint": "coding",
                "memory_policy": "project",
                "compact_skill_categories": [],
            },
            "model": "gpt-4o",
            "valid_tool_names": None,
            "instructions": "Follow custom guide",
            "workspace_text": "Workspace:\n- Root: /workspace",
        },
        {
            "id": "custom_profile_non_coding_name",
            "posture": {
                "name": "review_mode",
                "toolset": None,
                "guidance": "Review all diffs carefully.",
                "model_hint": None,
                "memory_policy": "default",
                "compact_skill_categories": [],
            },
            "model": "claude-3-5-sonnet",
            "valid_tool_names": ["read_file"],
            "instructions": "Do not approve blindly",
            "workspace_text": "Workspace:\n- Root: /workspace",
        },
        {
            "id": "coding_qwen_coder_replace_family",
            "posture": "coding",
            "model": "qwen/qwen-2.5-coder-32b",
            "valid_tool_names": ["todo_list", "patch"],
            "instructions": "",
            "workspace_text": "Workspace:\n- Root: /qwen-repo",
        },
        {
            "id": "coding_llama_replace_family",
            "posture": "coding",
            "model": "meta-llama/llama-3.3-70b-instruct",
            "valid_tool_names": ["read_file"],
            "instructions": "Respect PEP 8",
            "workspace_text": "",
        },
        {
            "id": "coding_minimax_replace_family",
            "posture": "coding",
            "model": "minimax-text-01",
            "valid_tool_names": ["todo_list"],
            "instructions": "",
            "workspace_text": "",
        },
        {
            "id": "coding_devstral_replace_family",
            "posture": "coding",
            "model": "mistralai/devstral-small",
            "valid_tool_names": ["patch"],
            "instructions": "",
            "workspace_text": "",
        },
        {
            "id": "coding_kimi_replace_family",
            "posture": "coding",
            "model": "kimi-coding",
            "valid_tool_names": ["read_file"],
            "instructions": "",
            "workspace_text": "",
        },
    ]

    for posture in ["CODING", " coding ", "unknown"]:
        prompt_cases_spec.append(dict(id=f"exact_profile_{posture}", posture=posture,
                                      model="gpt-5", valid_tool_names=["todo_list"],
                                      instructions="must not leak", workspace_text="workspace"))
    prompt_parts_cases = []
    for spec in prompt_cases_spec:
        expected = run_oracle(
            posture=spec["posture"],
            model=spec["model"],
            valid_tool_names=spec["valid_tool_names"],
            instructions=spec["instructions"],
            workspace_text=spec["workspace_text"],
        )
        prompt_parts_cases.append({
            "id": spec["id"],
            "inputs": {
                "posture": spec["posture"],
                "model": spec["model"],
                "valid_tool_names": spec["valid_tool_names"],
                "instructions": spec["instructions"],
                "workspace_text": spec["workspace_text"],
            },
            "expected": expected,
        })

    return {
        "constants": constants,
        "model_family_cases": model_family_cases,
        "prompt_parts_cases": prompt_parts_cases,
    }


def main():
    goldens = generate_goldens()
    text = json.dumps(goldens, indent=2, ensure_ascii=False) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not GOLDENS_PATH.exists():
            raise SystemExit(f"Golden file not found at {GOLDENS_PATH}")
        existing = GOLDENS_PATH.read_text(encoding="utf-8")
        assert existing == text, "Goldens mismatch under --check"
        print(f"Verified {len(goldens['prompt_parts_cases'])} prompt parts cases and {len(goldens['model_family_cases'])} model cases match {GOLDENS_PATH}")
    elif not sys.argv[1:]:
        GOLDENS_PATH.write_text(text, encoding="utf-8")
        print(f"Generated {len(goldens['prompt_parts_cases'])} prompt parts cases and {len(goldens['model_family_cases'])} model cases to {GOLDENS_PATH}")
    else:
        raise SystemExit("usage: gen_coding_prompt_goldens.py [--check]")


if __name__ == "__main__":
    main()
