#!/usr/bin/env python3
"""Generate actual Python oracle golden fixtures for skill merge and rendering.

This generator extracts the merge and rendering AST blocks directly from
agent.prompt_builder._build_skills_system_prompt_inner without reimplementing
the oracle logic.
"""

from __future__ import annotations

import ast
import json
from pathlib import Path
import sys
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_PATH = REPO_ROOT / "rust" / "tools" / "skill-merge-goldens.json"


def build_oracles():
    """Extract AST merge and render blocks from agent/prompt_builder.py."""
    source_path = REPO_ROOT / "agent" / "prompt_builder.py"
    tree = ast.parse(source_path.read_text(encoding="utf-8"))
    fn = next(
        node
        for node in tree.body
        if isinstance(node, ast.FunctionDef)
        and node.name == "_build_skills_system_prompt_inner"
    )

    stmt_proj_names = next(
        node
        for node in fn.body
        if isinstance(node, ast.AnnAssign)
        and getattr(node.target, "id", None) == "project_names"
    )
    stmt_proj_filter = next(
        node
        for node in fn.body
        if isinstance(node, ast.If)
        and getattr(node.test, "id", None) == "project_names"
    )
    stmt_name_owners = next(
        node
        for node in fn.body
        if isinstance(node, ast.AnnAssign)
        and getattr(node.target, "id", None) == "name_owners"
    )

    vis_loops = [
        node
        for node in fn.body
        if isinstance(node, ast.For)
        and getattr(node.iter, "id", None) == "visible_entries"
    ]
    assert len(vis_loops) == 2
    stmt_owner_loop = vis_loops[0]
    stmt_label_loop = vis_loops[1]

    stmt_seen_names = next(
        node
        for node in fn.body
        if isinstance(node, ast.AnnAssign)
        and getattr(node.target, "id", None) == "seen_skill_names"
    )
    stmt_seen_loop = next(
        node
        for node in fn.body
        if isinstance(node, ast.For)
        and getattr(node.target, "id", None) == "cat_skills"
    )

    stmt12 = fn.body[12]
    try_block12 = stmt12.body[1].body[1].body[0]
    proj_check = try_block12.body[4]
    proj_add = try_block12.body[7]
    proj_append = try_block12.body[8]

    stmt20 = fn.body[20]
    try_block20 = stmt20.body[1].body[0]
    ext_check = try_block20.body[5]
    ext_add = try_block20.body[8]
    ext_append = try_block20.body[9]

    start = next(
        i
        for i, node in enumerate(fn.body)
        if isinstance(node, ast.Assign)
        and any(getattr(t, "id", None) == "demoted" for t in node.targets)
    )
    end = next(
        i for i in range(start, len(fn.body)) if isinstance(fn.body[i], ast.With)
    )
    render_stmts = fn.body[start:end]

    proj_for_stmt = ast.For(
        target=ast.Name(id="entry", ctx=ast.Store()),
        iter=ast.parse("(project_entries or [])").body[0].value,
        body=[
            *ast.parse("""
entry = dict(entry)
if "category" not in entry or not entry["category"]:
    entry["category"] = "general"
if "description" not in entry:
    entry["description"] = ""
fm_name = entry.get("frontmatter_name") or entry.get("skill_name") or ""
entry["frontmatter_name"] = fm_name
entry["skill_name"] = entry.get("skill_name") or fm_name
""").body,
            proj_check,
            proj_add,
            proj_append,
        ],
        orelse=[],
    )

    ext_for_stmt = ast.For(
        target=ast.Name(id="entry", ctx=ast.Store()),
        iter=ast.parse("(external_entries or [])").body[0].value,
        body=[
            *ast.parse("""
entry = dict(entry)
if "category" not in entry or not entry["category"]:
    entry["category"] = "general"
if "description" not in entry:
    entry["description"] = ""
frontmatter_name = entry.get("frontmatter_name") or entry.get("skill_name") or ""
entry["frontmatter_name"] = frontmatter_name
entry["skill_name"] = entry.get("skill_name") or frontmatter_name
""").body,
            ext_check,
            ext_add,
            ext_append,
        ],
        orelse=[],
    )

    merge_body = [
        ast.parse("visible_entries = list(visible_entries or [])").body[0],
        ast.Assign(
            targets=[ast.Name(id="skills_by_category", ctx=ast.Store())],
            value=ast.Dict(keys=[], values=[]),
        ),
        stmt_proj_names,
        proj_for_stmt,
        stmt_proj_filter,
        stmt_name_owners,
        stmt_owner_loop,
        stmt_label_loop,
        stmt_seen_names,
        stmt_seen_loop,
        ext_for_stmt,
        ast.Return(value=ast.Name(id="skills_by_category", ctx=ast.Load())),
    ]

    render_setup = [
        ast.parse("category_descriptions = category_descriptions or {}").body[0],
        ast.parse("compact_categories = frozenset(compact_categories or ())").body[0],
        ast.parse(
            "available_tools = set(available_tools) if available_tools is not None else None"
        ).body[0],
    ]
    render_body = render_setup + render_stmts + [
        ast.Return(value=ast.Name(id="result", ctx=ast.Load()))
    ]

    mod = ast.Module(
        body=[
            ast.FunctionDef(
                name="merge_skills",
                args=ast.arguments(
                    posonlyargs=[],
                    args=[
                        ast.arg(arg="visible_entries"),
                        ast.arg(arg="project_entries"),
                        ast.arg(arg="external_entries"),
                    ],
                    kwonlyargs=[],
                    kw_defaults=[],
                    defaults=[ast.Constant(value=None), ast.Constant(value=None)],
                ),
                body=merge_body,
                decorator_list=[],
            ),
            ast.FunctionDef(
                name="render_skills",
                args=ast.arguments(
                    posonlyargs=[],
                    args=[
                        ast.arg(arg="skills_by_category"),
                        ast.arg(arg="category_descriptions"),
                        ast.arg(arg="compact_categories"),
                        ast.arg(arg="available_tools"),
                    ],
                    kwonlyargs=[],
                    kw_defaults=[],
                    defaults=[
                        ast.Constant(value=None),
                        ast.Constant(value=None),
                        ast.Constant(value=None),
                    ],
                ),
                body=render_body,
                decorator_list=[],
            ),
        ],
        type_ignores=[],
    )

    ast.fix_missing_locations(mod)
    scope: dict[str, Any] = {}
    exec(compile(mod, "agent/prompt_builder.py", "exec"), scope)
    return scope["merge_skills"], scope["render_skills"]


RAW_CASES: list[dict[str, Any]] = [
    # 1. Project overrides before org collision labeling
    {
        "name": "project_overrides_personal_and_org_collision",
        "visible_entries": [
            {"frontmatter_name": "deploy", "description": "Personal deploy", "category": "ops"},
            {"frontmatter_name": "deploy", "description": "Org deploy", "category": "ops", "org_id": "corp", "org_author": "alice"},
        ],
        "project_entries": [
            {"frontmatter_name": "deploy", "description": "Repo deploy", "category": "ops"}
        ],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "project_overrides_personal_only",
        "visible_entries": [
            {"frontmatter_name": "build", "description": "Local build", "category": "dev"}
        ],
        "project_entries": [
            {"frontmatter_name": "build", "description": "Repo build", "category": "dev"}
        ],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "project_overrides_org_only",
        "visible_entries": [
            {"frontmatter_name": "lint", "description": "Org lint", "org_id": "corp", "org_author": "carol"}
        ],
        "project_entries": [
            {"frontmatter_name": "lint", "description": "Repo lint", "category": "dev"}
        ],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "project_overrides_partial_mixed",
        "visible_entries": [
            {"frontmatter_name": "review", "description": "Personal review", "category": "qa"},
            {"frontmatter_name": "review", "description": "Org review", "org_id": "team", "org_author": "alice"},
            {"frontmatter_name": "test", "description": "Personal test", "category": "qa"},
            {"frontmatter_name": "test", "description": "Org test", "org_id": "team", "org_author": "bob"},
        ],
        "project_entries": [
            {"frontmatter_name": "review", "description": "Project review", "category": "qa"}
        ],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },

    # 2. Exact collision text and tag ordering
    {
        "name": "exact_collision_text_both_with_descriptions",
        "visible_entries": [
            {"frontmatter_name": "format", "description": "Format python files", "category": "coding"},
            {"frontmatter_name": "format", "description": "Company format rules", "org_id": "acme", "org_author": "dave"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "exact_collision_text_empty_descriptions",
        "visible_entries": [
            {"frontmatter_name": "blank", "description": "", "category": "general"},
            {"frontmatter_name": "blank", "description": "", "org_id": "acme", "org_author": ""},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "exact_collision_text_none_descriptions",
        "visible_entries": [
            {"frontmatter_name": "none_desc", "description": None, "category": "general"},
            {"frontmatter_name": "none_desc", "description": None, "org_id": "acme", "org_author": None},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },

    # 3. Org author tags
    {
        "name": "org_author_tag_present",
        "visible_entries": [
            {"frontmatter_name": "deploy_prod", "description": "Production deploy", "org_id": "infra", "org_author": "sre-lead"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "org_author_tag_empty_string",
        "visible_entries": [
            {"frontmatter_name": "deploy_staging", "description": "Staging deploy", "org_id": "infra", "org_author": ""}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "org_author_tag_none",
        "visible_entries": [
            {"frontmatter_name": "deploy_dev", "description": "Dev deploy", "org_id": "infra", "org_author": None}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "org_author_tag_missing_key",
        "visible_entries": [
            {"frontmatter_name": "audit", "description": "Audit tool", "org_id": "security"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "multiple_orgs_same_skill_name_no_collision",
        "visible_entries": [
            {"frontmatter_name": "playbook", "description": "Org A playbook", "org_id": "orgA", "org_author": "alice"},
            {"frontmatter_name": "playbook", "description": "Org B playbook", "org_id": "orgB", "org_author": "bob"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },

    # 4. Duplicate names
    {
        "name": "duplicate_personal_same_category",
        "visible_entries": [
            {"frontmatter_name": "helper", "description": "First helper", "category": "util"},
            {"frontmatter_name": "helper", "description": "Second helper", "category": "util"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "duplicate_personal_different_categories",
        "visible_entries": [
            {"frontmatter_name": "clean", "description": "Clean files", "category": "filesystem"},
            {"frontmatter_name": "clean", "description": "Clean git repo", "category": "git"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "duplicate_project_entries_first_wins",
        "visible_entries": [],
        "project_entries": [
            {"frontmatter_name": "migrate", "description": "Primary migration", "category": "db"},
            {"frontmatter_name": "migrate", "description": "Secondary migration", "category": "db"},
        ],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "duplicate_across_project_and_external",
        "visible_entries": [],
        "project_entries": [
            {"frontmatter_name": "bundle", "description": "Project bundle", "category": "build"}
        ],
        "external_entries": [
            {"frontmatter_name": "bundle", "description": "External bundle", "category": "build"}
        ],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "duplicate_across_personal_and_external",
        "visible_entries": [
            {"frontmatter_name": "search", "description": "Personal search", "category": "web"}
        ],
        "project_entries": [],
        "external_entries": [
            {"frontmatter_name": "search", "description": "External search", "category": "web"}
        ],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "duplicate_across_org_and_external",
        "visible_entries": [
            {"frontmatter_name": "telemetry", "description": "Org telemetry", "org_id": "ops", "org_author": "eva"}
        ],
        "project_entries": [],
        "external_entries": [
            {"frontmatter_name": "telemetry", "description": "External telemetry", "category": "monitoring"}
        ],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },

    # 5. External first-wins behavior
    {
        "name": "external_first_wins_multiple_externals",
        "visible_entries": [],
        "project_entries": [],
        "external_entries": [
            {"frontmatter_name": "ext_helper", "description": "First external helper", "category": "ext"},
            {"frontmatter_name": "ext_helper", "description": "Second external helper", "category": "ext"},
        ],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "external_unique_skills_added",
        "visible_entries": [],
        "project_entries": [],
        "external_entries": [
            {"frontmatter_name": "ext_one", "description": "First external", "category": "ext1"},
            {"frontmatter_name": "ext_two", "description": "Second external", "category": "ext2"},
        ],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },

    # 6. Missing and falsy metadata fields
    {
        "name": "missing_frontmatter_name_fallback_to_skill_name",
        "visible_entries": [
            {"skill_name": "fallback_tool", "description": "Inferred name", "category": "util"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "empty_frontmatter_name_fallback_to_skill_name",
        "visible_entries": [
            {"frontmatter_name": "", "skill_name": "backup_tool", "description": "Backup tool desc", "category": "util"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "missing_category_defaults_to_general",
        "visible_entries": [
            {"frontmatter_name": "uncategorized", "description": "No category"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "empty_category_defaults_to_general",
        "visible_entries": [
            {"frontmatter_name": "empty_cat", "description": "Empty cat", "category": ""}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "missing_description_in_personal",
        "visible_entries": [
            {"frontmatter_name": "nodesc", "category": "general"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "falsy_org_id_treated_as_personal",
        "visible_entries": [
            {"frontmatter_name": "local_thing", "description": "Not org", "org_id": "", "org_author": "someone", "category": "misc"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "empty_entry_dict",
        "visible_entries": [
            {}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },

    # 7. Rendering variations (demoted, available tools, category descriptions)
    {
        "name": "demoted_compact_categories_rendering",
        "visible_entries": [
            {"frontmatter_name": "music_play", "description": "Play track", "category": "music"},
            {"frontmatter_name": "code_edit", "description": "Edit code", "category": "coding"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": ["music"],
        "available_tools": None,
    },
    {
        "name": "hierarchical_compact_categories_rendering",
        "visible_entries": [
            {"frontmatter_name": "tweet", "description": "Post tweet", "category": "social/twitter"},
            {"frontmatter_name": "debug", "description": "Debug code", "category": "coding"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": ["social"],
        "available_tools": None,
    },
    {
        "name": "available_tools_without_web_search",
        "visible_entries": [
            {"frontmatter_name": "sh", "description": "Shell helper", "category": "general"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": ["terminal"],
    },
    {
        "name": "available_tools_with_web_search",
        "visible_entries": [
            {"frontmatter_name": "sh", "description": "Shell helper", "category": "general"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": ["terminal", "web_search"],
    },
    {
        "name": "available_tools_none",
        "visible_entries": [
            {"frontmatter_name": "sh", "description": "Shell helper", "category": "general"}
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "category_descriptions_rendering",
        "visible_entries": [
            {"frontmatter_name": "git_commit", "description": "Commit changes", "category": "git"},
            {"frontmatter_name": "misc_tool", "description": "Misc tool", "category": "misc"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {"git": "Git version control operations"},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "empty_everything_renders_empty_string",
        "visible_entries": [],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "unicode_skill_name_and_category",
        "visible_entries": [
            {"frontmatter_name": "git_commit_crab", "description": "Unicode test Cafe", "category": "tools/cafe"},
        ],
        "project_entries": [],
        "external_entries": [],
        "category_descriptions": {"tools/cafe": "Unicode Cafe tools"},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "whitespace_handling_in_descriptions",
        "visible_entries": [
            {"frontmatter_name": "padded", "description": "   spaced out   ", "category": "general"},
            {"frontmatter_name": "org_padded", "description": "   org spaced   ", "org_id": "corp", "org_author": "alice"},
        ],
        "project_entries": [
            {"frontmatter_name": "proj_padded", "description": "   proj spaced   ", "category": "general"}
        ],
        "external_entries": [],
        "category_descriptions": {},
        "compact_categories": [],
        "available_tools": None,
    },
    {
        "name": "comprehensive_all_sources_combined",
        "visible_entries": [
            {"frontmatter_name": "deploy", "description": "Personal deploy", "category": "ops"},
            {"frontmatter_name": "monitor", "description": "Personal monitor", "category": "monitoring"},
            {"frontmatter_name": "monitor", "description": "Org monitor", "org_id": "corp", "org_author": "sre"},
            {"frontmatter_name": "policy", "description": "Security policy", "org_id": "sec"},
            {"frontmatter_name": "notes", "description": "Take notes", "category": "note-taking"},
        ],
        "project_entries": [
            {"frontmatter_name": "deploy", "description": "Project deploy", "category": "ops"}
        ],
        "external_entries": [
            {"frontmatter_name": "notes", "description": "External notes (should be skipped)", "category": "note-taking"},
            {"frontmatter_name": "ext_scanner", "description": "External scanner", "category": "sec_tools"},
            {"frontmatter_name": "ext_scanner", "description": "External scanner dupe (should be skipped)", "category": "sec_tools"},
        ],
        "category_descriptions": {
            "ops": "Operations and deployments",
            "sec_tools": "External security tools",
        },
        "compact_categories": ["note-taking"],
        "available_tools": ["terminal"],
    },
]


def generate_cases() -> list[dict[str, Any]]:
    """Execute extracted Python AST oracles to evaluate all test cases."""
    merge_fn, render_fn = build_oracles()
    evaluated: list[dict[str, Any]] = []

    for raw in RAW_CASES:
        skills_by_category = merge_fn(
            visible_entries=raw["visible_entries"],
            project_entries=raw["project_entries"],
            external_entries=raw["external_entries"],
        )
        rendered = render_fn(
            skills_by_category=skills_by_category,
            category_descriptions=raw["category_descriptions"],
            compact_categories=raw["compact_categories"],
            available_tools=raw["available_tools"],
        )

        case_record = {
            "name": raw["name"],
            "project_entries": raw["project_entries"],
            "visible_entries": raw["visible_entries"],
            "external_entries": raw["external_entries"],
            "category_descriptions": raw["category_descriptions"],
            "compact_categories": raw["compact_categories"],
            "available_tools": raw["available_tools"],
            "skills_by_category": skills_by_category,
            "rendered": rendered,
        }
        evaluated.append(case_record)

    return evaluated


def main() -> None:
    if sys.argv[1:] not in ([], ["--check"]):
        raise SystemExit("usage: gen_skill_merge_goldens.py [--check]")

    cases = generate_cases()
    content = json.dumps(cases, indent=2) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not OUT_PATH.exists():
            raise SystemExit(f"Golden file does not exist: {OUT_PATH}")
        existing = OUT_PATH.read_text(encoding="utf-8")
        if existing != content:
            raise SystemExit(f"{OUT_PATH.name} differs from Python oracle")
        print(f"Verified {len(cases)} actual Python skill-merge cases")
    else:
        OUT_PATH.write_text(content, encoding="utf-8")
        print(f"Generated {len(cases)} actual Python skill-merge cases to {OUT_PATH}")


if __name__ == "__main__":
    main()
