#!/usr/bin/env python3
"""Generate goldens for skill directory walking and active org resolution.

This script exercises the actual Python implementation in agent.skill_utils:
- iter_skill_index_files
- read_active_org_id

It materializes isolated temporary directory fixtures and executes the real
Python filesystem operations against them, capturing the discovered relative
paths for each scenario.

Coverage:
1. Excluded directories (.git, .github, .venv, venv, node_modules, __pycache__,
   .archive, .curator_backups, site-packages, .pytest_cache, .mypy_cache,
   .ruff_cache, .tox, .nox, .hub)
2. Support directories (references, templates, assets, scripts) pruned only when
   parent contains SKILL.md
3. Ordinary multi-level nested directories
4. Hidden non-excluded directories (.visible, .custom_skills, etc.)
5. _org marker states: absent, empty, whitespace-only, active, stale
6. Regular directory symlinks (top-level and nested)
7. Broken file links (which os.walk classifies as files) and broken dir links
8. SKILL.md as directory vs file, including nested SKILL.md inside SKILL.md dir
9. Alternative index filenames such as DESCRIPTION.md
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from agent.skill_utils import iter_skill_index_files, read_active_org_id

OUT_PATH = REPO_ROOT / "rust" / "tools" / "skill-walk-goldens.json"

RAW_CASES: list[dict[str, Any]] = [
    # 1. Standard flat skills directory with multiple skills
    {
        "filename": "SKILL.md",
        "files": {
            "skill_alpha/SKILL.md": "Alpha skill",
            "skill_beta/SKILL.md": "Beta skill",
            "skill_gamma/SKILL.md": "Gamma skill",
        },
        "symlinks": {},
    },
    # 2. Excluded directories at root level
    {
        "filename": "SKILL.md",
        "files": {
            "valid_skill/SKILL.md": "valid",
            ".git/SKILL.md": "git",
            ".git/hooks/SKILL.md": "git hook",
            ".github/workflows/SKILL.md": "github",
            ".venv/bin/SKILL.md": "venv",
            "venv/lib/SKILL.md": "venv",
            "node_modules/pkg/SKILL.md": "node",
            "__pycache__/SKILL.md": "pycache",
        },
        "symlinks": {},
    },
    # 3. Excluded directories nested deep within a skill package
    {
        "filename": "SKILL.md",
        "files": {
            "main_skill/SKILL.md": "main",
            "main_skill/.archive/old/SKILL.md": "archive",
            "main_skill/.curator_backups/bak/SKILL.md": "curator",
            "main_skill/site-packages/pkg/SKILL.md": "site-packages",
            "main_skill/.pytest_cache/v/SKILL.md": "pytest",
            "main_skill/.mypy_cache/cache/SKILL.md": "mypy",
            "main_skill/.ruff_cache/cache/SKILL.md": "ruff",
            "main_skill/.tox/env/SKILL.md": "tox",
            "main_skill/.nox/env/SKILL.md": "nox",
            "main_skill/.hub/SKILL.md": "hub",
        },
        "symlinks": {},
    },
    # 4. Support directories (all 4) pruned when parent has SKILL.md
    {
        "filename": "SKILL.md",
        "files": {
            "my_skill/SKILL.md": "my skill",
            "my_skill/references/ref_skill/SKILL.md": "ref",
            "my_skill/templates/tmpl_skill/SKILL.md": "tmpl",
            "my_skill/assets/asset_skill/SKILL.md": "asset",
            "my_skill/scripts/script_skill/SKILL.md": "script",
        },
        "symlinks": {},
    },
    # 5. Support directories NOT pruned when parent does NOT have SKILL.md
    {
        "filename": "SKILL.md",
        "files": {
            "category/references/ref_skill/SKILL.md": "ref",
            "category/templates/tmpl_skill/SKILL.md": "tmpl",
            "category/assets/asset_skill/SKILL.md": "asset",
            "category/scripts/script_skill/SKILL.md": "script",
            "category/README.md": "category readme without SKILL.md",
        },
        "symlinks": {},
    },
    # 6. Parent has SKILL.md: support dirs pruned, but non-support subdirs walked
    {
        "filename": "SKILL.md",
        "files": {
            "composite_skill/SKILL.md": "composite",
            "composite_skill/references/archived/SKILL.md": "archived",
            "composite_skill/sub_skills/helper/SKILL.md": "helper",
            "composite_skill/nested_category/tool/SKILL.md": "tool",
        },
        "symlinks": {},
    },
    # 7. Deep nested support dir under a non-skill intermediate directory
    {
        "filename": "SKILL.md",
        "files": {
            "top_skill/SKILL.md": "top",
            "top_skill/docs/references/nested_doc_skill/SKILL.md": "nested",
            "top_skill/references/direct_ref/SKILL.md": "direct",
        },
        "symlinks": {},
    },
    # 8. Ordinary deeply nested directories
    {
        "filename": "SKILL.md",
        "files": {
            "a/b/c/d/deep_skill/SKILL.md": "deep",
            "x/y/another_skill/SKILL.md": "another",
        },
        "symlinks": {},
    },
    # 9. Empty directories and non-matching files ignored
    {
        "filename": "SKILL.md",
        "files": {
            "empty_skill_dir/placeholder.txt": "placeholder",
            "docs/guide.md": "not a skill",
            "valid/SKILL.md": "valid",
        },
        "symlinks": {},
    },
    # 10. Hidden non-excluded directories at root level (.visible, .custom_skills, etc.)
    {
        "filename": "SKILL.md",
        "files": {
            ".visible/SKILL.md": "visible",
            ".custom_skills/my_custom/SKILL.md": "custom",
            ".experiments/exp_one/SKILL.md": "exp",
            ".git/hidden/SKILL.md": "excluded git",
        },
        "symlinks": {},
    },
    # 11. Nested hidden non-excluded directory inside a skill
    {
        "filename": "SKILL.md",
        "files": {
            "parent_skill/SKILL.md": "parent",
            "parent_skill/.private_extension/SKILL.md": "extension",
        },
        "symlinks": {},
    },
    # 12. _org marker absent -> _org directory completely ignored
    {
        "filename": "SKILL.md",
        "files": {
            "_org/org_one/skill_a/SKILL.md": "org a",
            "_org/org_two/skill_b/SKILL.md": "org b",
            "personal/skill_c/SKILL.md": "personal c",
        },
        "symlinks": {},
    },
    # 13. _org marker empty -> _org directory completely ignored
    {
        "filename": "SKILL.md",
        "files": {
            "_org/.active_org": "",
            "_org/org_one/skill_a/SKILL.md": "org a",
            "personal/skill_c/SKILL.md": "personal c",
        },
        "symlinks": {},
    },
    # 14. _org marker whitespace only -> _org directory completely ignored
    {
        "filename": "SKILL.md",
        "files": {
            "_org/.active_org": "   \n\t  ",
            "_org/org_one/skill_a/SKILL.md": "org a",
            "personal/skill_c/SKILL.md": "personal c",
        },
        "symlinks": {},
    },
    # 15. _org marker active -> only active org mirror descended into, other pruned
    {
        "filename": "SKILL.md",
        "files": {
            "_org/.active_org": "active_team\n",
            "_org/active_team/team_skill/SKILL.md": "active",
            "_org/other_team/other_skill/SKILL.md": "inactive",
            "local/local_skill/SKILL.md": "local",
        },
        "symlinks": {},
    },
    # 16. _org marker active with leading/trailing spaces stripped
    {
        "filename": "SKILL.md",
        "files": {
            "_org/.active_org": "  org_trimmed  \n",
            "_org/org_trimmed/skill_t/SKILL.md": "trimmed",
            "_org/org_other/skill_o/SKILL.md": "other",
        },
        "symlinks": {},
    },
    # 17. _org marker stale (points to non-existent org in mirrors)
    {
        "filename": "SKILL.md",
        "files": {
            "_org/.active_org": "stale_org_99\n",
            "_org/current_org/skill_curr/SKILL.md": "current",
            "_org/old_org/skill_old/SKILL.md": "old",
            "shared/skill_s/SKILL.md": "shared",
        },
        "symlinks": {},
    },
    # 18. _org active org containing support directories pruned by SKILL.md
    {
        "filename": "SKILL.md",
        "files": {
            "_org/.active_org": "corp_org",
            "_org/corp_org/skill_main/SKILL.md": "main",
            "_org/corp_org/skill_main/references/archived/SKILL.md": "ref",
            "_org/corp_org/skill_main/scripts/helper/SKILL.md": "script",
            "_org/corp_org/category_no_skill/references/good/SKILL.md": "good",
        },
        "symlinks": {},
    },
    # 19. Regular directory symlinks (at top level)
    {
        "filename": "SKILL.md",
        "files": {
            "real_dir/actual_skill/SKILL.md": "actual",
        },
        "symlinks": {
            "linked_dir": "real_dir",
        },
    },
    # 20. Regular directory symlinks nested inside a subdirectory
    {
        "filename": "SKILL.md",
        "files": {
            "shared_packages/tool_skill/SKILL.md": "shared",
        },
        "symlinks": {
            "collections/dev/sym_tool": "../../shared_packages/tool_skill",
        },
    },
    # 21. Symlink with excluded directory name is pruned
    {
        "filename": "SKILL.md",
        "files": {
            "real_store/pkg/SKILL.md": "real",
        },
        "symlinks": {
            "my_skill/node_modules": "../real_store",
        },
    },
    # 22. Broken file link named SKILL.md is matched as a file
    {
        "filename": "SKILL.md",
        "files": {
            "normal_skill/SKILL.md": "normal",
        },
        "symlinks": {
            "broken_skill/SKILL.md": "nonexistent_target_file.md",
        },
    },
    # 23. Broken file link with non-matching name and broken directory link
    {
        "filename": "SKILL.md",
        "files": {
            "valid_skill/SKILL.md": "valid",
        },
        "symlinks": {
            "broken_file_skill/OTHER.md": "nonexistent.md",
            "broken_dir_parent/broken_dir": "nonexistent_dir",
        },
    },
    # 24. SKILL.md is a directory, not a file
    {
        "filename": "SKILL.md",
        "files": {
            "pseudo_skill/SKILL.md/documentation.txt": "not a skill markdown file",
            "real_skill/SKILL.md": "real",
        },
        "symlinks": {},
    },
    # 25. SKILL.md directory contains a real SKILL.md file inside it
    {
        "filename": "SKILL.md",
        "files": {
            "container/SKILL.md/SKILL.md": "nested skill file",
            "container/other/SKILL.md": "other",
        },
        "symlinks": {},
    },
    # 26. SKILL.md directory does NOT cause sibling support directories to be pruned
    {
        "filename": "SKILL.md",
        "files": {
            "pkg_dir/SKILL.md/readme.txt": "readme",
            "pkg_dir/references/ref_skill/SKILL.md": "ref",
            "pkg_dir/scripts/scr_skill/SKILL.md": "scr",
        },
        "symlinks": {},
    },
    # 27. Searching for DESCRIPTION.md when both SKILL.md and DESCRIPTION.md exist
    {
        "filename": "DESCRIPTION.md",
        "files": {
            "skill_x/SKILL.md": "skill content",
            "skill_x/DESCRIPTION.md": "description content",
            "skill_y/DESCRIPTION.md": "y description",
            "skill_z/SKILL.md": "z skill without description",
        },
        "symlinks": {},
    },
    # 28. Searching for DESCRIPTION.md with support directories pruned by SKILL.md
    {
        "filename": "DESCRIPTION.md",
        "files": {
            "skill_with_both/SKILL.md": "skill",
            "skill_with_both/DESCRIPTION.md": "desc",
            "skill_with_both/references/DESCRIPTION.md": "pruned ref desc",
            "skill_with_both/assets/DESCRIPTION.md": "pruned asset desc",
        },
        "symlinks": {},
    },
    # 29. Searching for DESCRIPTION.md where parent has DESCRIPTION.md but NO SKILL.md
    {
        "filename": "DESCRIPTION.md",
        "files": {
            "category_pkg/DESCRIPTION.md": "cat desc",
            "category_pkg/references/DESCRIPTION.md": "ref desc not pruned",
            "category_pkg/templates/DESCRIPTION.md": "tmpl desc not pruned",
        },
        "symlinks": {},
    },
    # 30. DESCRIPTION.md search with active org mirror and broken link
    {
        "filename": "DESCRIPTION.md",
        "files": {
            "_org/.active_org": "my_org\n",
            "_org/my_org/skill_one/DESCRIPTION.md": "org desc",
            "_org/stale_org/skill_two/DESCRIPTION.md": "stale org desc",
            "local/DESCRIPTION.md": "local desc",
        },
        "symlinks": {
            "local/broken_link_dir/DESCRIPTION.md": "missing_target.md",
        },
    },
    # 31. Comprehensive composite scenario
    {
        "filename": "SKILL.md",
        "files": {
            "skills/coding/python/SKILL.md": "python skill",
            "skills/coding/python/references/old/SKILL.md": "pruned ref",
            "skills/coding/python/.git/SKILL.md": "pruned git",
            "skills/coding/python/.hidden_nonexcluded/SKILL.md": "hidden nonexcluded",
            "skills/devops/SKILL.md": "devops skill",
            "skills/devops/templates/starter/SKILL.md": "pruned template",
            "skills/devops/submodules/extra/SKILL.md": "submodule skill",
            "_org/.active_org": "acme\n",
            "_org/acme/cloud_tool/SKILL.md": "acme skill",
            "_org/acme/cloud_tool/scripts/nested/SKILL.md": "pruned script",
            "_org/legacy/old_tool/SKILL.md": "stale org pruned",
        },
        "symlinks": {
            "skills/shortcuts/py": "../coding/python",
            "skills/broken_link/SKILL.md": "missing.md",
        },
    },
    # 32. Empty directory with no files or symlinks
    {
        "filename": "SKILL.md",
        "files": {},
        "symlinks": {},
    },
]


def materialize_and_evaluate(case_def: dict[str, Any]) -> dict[str, Any]:
    """Materialize a temporary directory for case_def and execute Python oracle."""
    filename = case_def["filename"]
    files = case_def.get("files", {})
    symlinks = case_def.get("symlinks", {})

    with tempfile.TemporaryDirectory(prefix="skill-walk-") as temp_dir:
        root = Path(temp_dir)

        # 1. Write files
        for rel_path, content in files.items():
            file_path = root / rel_path
            file_path.parent.mkdir(parents=True, exist_ok=True)
            file_path.write_text(content, encoding="utf-8")

        # 2. Create symlinks
        for rel_path, target in symlinks.items():
            link_path = root / rel_path
            link_path.parent.mkdir(parents=True, exist_ok=True)
            os.symlink(target, str(link_path))

        # Exercise read_active_org_id on root
        _ = read_active_org_id(root)

        # Execute actual Python iter_skill_index_files
        matches = list(iter_skill_index_files(root, filename))
        expected = sorted([p.relative_to(root).as_posix() for p in matches])

        return {
            "files": files,
            "symlinks": symlinks,
            "filename": filename,
            "expected": expected,
        }


def generate_cases() -> list[dict[str, Any]]:
    """Generate all test cases by executing them against the Python oracle."""
    return [materialize_and_evaluate(c) for c in RAW_CASES]


def main() -> None:
    cases = generate_cases()
    content = json.dumps(cases, indent=2) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not OUT_PATH.exists():
            raise SystemExit(f"Golden file does not exist: {OUT_PATH}")
        existing = OUT_PATH.read_text(encoding="utf-8")
        if existing != content:
            raise SystemExit(f"Golden file differs from Python oracle: {OUT_PATH}")
        print(f"Verified {len(cases)} skill walk cases match Python oracle")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_skill_walk_goldens.py [--check]")
    else:
        OUT_PATH.write_text(content, encoding="utf-8")
        print(f"Generated {len(cases)} skill walk cases to {OUT_PATH}")


if __name__ == "__main__":
    main()
