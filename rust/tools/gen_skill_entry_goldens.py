#!/usr/bin/env python3
"""Generate actual Python oracle golden cases for _build_snapshot_entry.

This script exercises agent.prompt_builder._build_snapshot_entry and its dependencies
against deterministic test cases in an isolated temporary skills directory.
"""

from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from agent.prompt_builder import _build_snapshot_entry

OUT_PATH = REPO_ROOT / "rust" / "tools" / "skill-entry-goldens.json"

RAW_CASES: list[dict[str, Any]] = [
    # 1. Path shapes: Root skills
    {
        "relative_path": "SKILL.md",
        "frontmatter": {},
        "description": "Root skill with default name",
        "provenance": None,
    },
    {
        "relative_path": "DESCRIPTION.md",
        "frontmatter": {"name": "root-desc"},
        "description": "Root skill with description file",
        "provenance": None,
    },
    {
        "relative_path": "custom.md",
        "frontmatter": {"name": "custom-root"},
        "description": "Root skill with custom filename",
        "provenance": None,
    },
    # 2. Path shapes: Single-level category (flat directory under root)
    {
        "relative_path": "my-skill/SKILL.md",
        "frontmatter": {},
        "description": "Flat single-level directory skill",
        "provenance": None,
    },
    {
        "relative_path": "general-helper/SKILL.md",
        "frontmatter": {"name": "helper"},
        "description": "Flat helper skill with custom name",
        "provenance": None,
    },
    # 3. Path shapes: Standard category + skill (2 directory levels)
    {
        "relative_path": "coding/git-helper/SKILL.md",
        "frontmatter": {"name": "git-helper"},
        "description": "Standard category and skill directory",
        "provenance": None,
    },
    {
        "relative_path": "web/search/SKILL.md",
        "frontmatter": {},
        "description": "Web search skill with inferred name",
        "provenance": None,
    },
    {
        "relative_path": "data/analyzer/SKILL.md",
        "frontmatter": {"name": "data-tool"},
        "description": "Data analysis skill",
        "provenance": None,
    },
    # 4. Path shapes: Multi-level nested categories
    {
        "relative_path": "tools/network/http/fetcher/SKILL.md",
        "frontmatter": {},
        "description": "Nested category path with 3 category levels",
        "provenance": None,
    },
    {
        "relative_path": "cloud/aws/s3/downloader/SKILL.md",
        "frontmatter": {"name": "s3-get"},
        "description": "Deeply nested cloud tool",
        "provenance": None,
    },
    {
        "relative_path": "a/b/c/d/e/deep-skill/SKILL.md",
        "frontmatter": {},
        "description": "Five-level deep category path",
        "provenance": None,
    },
    {
        "relative_path": "my category/skill name/SKILL.md",
        "frontmatter": {},
        "description": "Path components with spaces",
        "provenance": None,
    },
    # 5. Path shapes: Non-org paths with _org in name or parts
    {
        "relative_path": "_org/SKILL.md",
        "frontmatter": {},
        "description": "Direct child of _org directory without org_id",
        "provenance": None,
    },
    {
        "relative_path": "category/_org/sub/SKILL.md",
        "frontmatter": {},
        "description": "Path with _org as a subcategory component",
        "provenance": None,
    },
    {
        "relative_path": "not_org/org_1/my-skill/SKILL.md",
        "frontmatter": {},
        "description": "Category containing org in directory name",
        "provenance": None,
    },
    # 6. Path shapes: Org mirror paths
    {
        "relative_path": "_org/acme-corp/SKILL.md",
        "frontmatter": {},
        "description": "Org root skill",
        "provenance": None,
    },
    {
        "relative_path": "_org/acme-corp/team-tool/SKILL.md",
        "frontmatter": {},
        "description": "Org flat skill",
        "provenance": None,
    },
    {
        "relative_path": "_org/acme-corp/devops/deployer/SKILL.md",
        "frontmatter": {},
        "description": "Org category skill",
        "provenance": None,
    },
    {
        "relative_path": "_org/acme-corp/infra/cloud/aws/backup/SKILL.md",
        "frontmatter": {},
        "description": "Org nested category skill",
        "provenance": None,
    },
    {
        "relative_path": "_org/company-xyz/dept/team/project/service/SKILL.md",
        "frontmatter": {},
        "description": "Org deeply nested service skill",
        "provenance": None,
    },
    {
        "relative_path": "_org/my-org.team_1/tools/SKILL.md",
        "frontmatter": {},
        "description": "Org ID with hyphen, dot, and underscore",
        "provenance": json.dumps({"author_device": "box-1"}),
    },
    # 7. Org provenance: Absent provenance (null)
    {
        "relative_path": "_org/prov-org/tools/runner/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with absent provenance file",
        "provenance": None,
    },
    # 8. Org provenance: Malformed provenance
    {
        "relative_path": "_org/prov-org/tools/p1/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with empty provenance file",
        "provenance": "",
    },
    {
        "relative_path": "_org/prov-org/tools/p2/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with broken JSON syntax",
        "provenance": "{invalid json syntax",
    },
    {
        "relative_path": "_org/prov-org/tools/p3/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with JSON array provenance",
        "provenance": "[1, 2, 3]",
    },
    {
        "relative_path": "_org/prov-org/tools/p4/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with JSON integer provenance",
        "provenance": "12345",
    },
    {
        "relative_path": "_org/prov-org/tools/p5/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with JSON string provenance",
        "provenance": '"author_device"',
    },
    {
        "relative_path": "_org/prov-org/tools/p6/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with JSON boolean provenance",
        "provenance": "true",
    },
    # 9. Org provenance: Valid provenance
    {
        "relative_path": "_org/prov-org/tools/p7/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with empty object provenance",
        "provenance": "{}",
    },
    {
        "relative_path": "_org/prov-org/tools/p8/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with author_device only",
        "provenance": json.dumps({"author_device": "macbook-pro"}),
    },
    {
        "relative_path": "_org/prov-org/tools/p9/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with author_user_id only",
        "provenance": json.dumps({"author_user_id": "usr_alice"}),
    },
    {
        "relative_path": "_org/prov-org/tools/p10/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with both author_device and author_user_id",
        "provenance": json.dumps({"author_device": "dev-box-9", "author_user_id": "usr_bob"}),
    },
    {
        "relative_path": "_org/prov-org/tools/p11/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with empty author_device and valid author_user_id",
        "provenance": json.dumps({"author_device": "", "author_user_id": "usr_carol"}),
    },
    {
        "relative_path": "_org/prov-org/tools/p12/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with null author_device and valid author_user_id",
        "provenance": json.dumps({"author_device": None, "author_user_id": "usr_dave"}),
    },
    {
        "relative_path": "_org/prov-org/tools/p13/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with null author_device and null author_user_id",
        "provenance": json.dumps({"author_device": None, "author_user_id": None}),
    },
    {
        "relative_path": "_org/prov-org/tools/p14/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with integer author_device and author_user_id",
        "provenance": json.dumps({"author_device": 101, "author_user_id": 202}),
    },
    {
        "relative_path": "_org/prov-org/tools/p15/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with boolean author_device and author_user_id",
        "provenance": json.dumps({"author_device": False, "author_user_id": True}),
    },
    {
        "relative_path": "_org/prov-org/tools/p16/SKILL.md",
        "frontmatter": {},
        "description": "Org skill with extra provenance metadata fields",
        "provenance": json.dumps({"author_device": "ci-runner-1", "author_user_id": "usr_ci", "version": 2}),
    },
    # 10. Non-org path with provenance provided (ignored)
    {
        "relative_path": "tools/search/SKILL.md",
        "frontmatter": {},
        "description": "Non-org skill with provenance file present in mirror",
        "provenance": json.dumps({"author_device": "ignored-box"}),
    },
    # 11. Raw frontmatter name: String and scalar values
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {},
        "description": "Frontmatter without name key",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": "named-skill"},
        "description": "Frontmatter name matching directory name",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": "custom-display-name"},
        "description": "Frontmatter name differing from directory name",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": ""},
        "description": "Frontmatter name as empty string",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": "  padded name  "},
        "description": "Frontmatter name with whitespace padding",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": 42},
        "description": "Frontmatter name as integer scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": 3.1415},
        "description": "Frontmatter name as float scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": True},
        "description": "Frontmatter name as boolean True",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": False},
        "description": "Frontmatter name as boolean False",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": None},
        "description": "Frontmatter name explicitly None",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": ["item1", "item2"]},
        "description": "Frontmatter name as list",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": {"key": "value"}},
        "description": "Frontmatter name as dict",
        "provenance": None,
    },
    {
        "relative_path": "general/named-skill/SKILL.md",
        "frontmatter": {"name": "日本語スキル 🚀"},
        "description": "Frontmatter name with unicode characters",
        "provenance": None,
    },
    # 12. Raw frontmatter platforms: Valid and scalar values
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {},
        "description": "Platforms field omitted",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": None},
        "description": "Platforms field explicitly None",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": []},
        "description": "Platforms field as empty list",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": "linux"},
        "description": "Platforms field as single string scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": "  macos  "},
        "description": "Platforms field as single string with whitespace",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": ["linux", "darwin", "win32"]},
        "description": "Platforms field as list of platform strings",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": [" linux ", "", "  ", "darwin", "	"]},
        "description": "Platforms list with empty and whitespace elements",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": ["linux", 1, True, None, 2.5, ""]},
        "description": "Platforms list with mixed scalar types",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": 0},
        "description": "Platforms field as falsy integer 0",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": False},
        "description": "Platforms field as falsy boolean False",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": ""},
        "description": "Platforms field as falsy empty string",
        "provenance": None,
    },
    {
        "relative_path": "general/plat-skill/SKILL.md",
        "frontmatter": {"platforms": {"linux": True, "win32": False}},
        "description": "Platforms field as dict",
        "provenance": None,
    },
    # 13. Condition metadata variations
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {},
        "description": "Frontmatter without metadata",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": None},
        "description": "Metadata field explicitly None",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": "malformed string"},
        "description": "Metadata field as string scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": 123},
        "description": "Metadata field as integer scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": []},
        "description": "Metadata field as list",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": {}},
        "description": "Metadata field as empty dict",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": {"hermes": None}},
        "description": "Hermes section in metadata explicitly None",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": {"hermes": "bad string"}},
        "description": "Hermes section in metadata as string",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {"metadata": {"hermes": {}}},
        "description": "Hermes section in metadata as empty dict",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {
            "metadata": {
                "hermes": {
                    "fallback_for_toolsets": ["legacy_browser"],
                    "requires_toolsets": ["terminal", "developer"],
                    "fallback_for_tools": ["old_patch"],
                    "requires_tools": ["git", "curl"],
                    "session_platforms": ["telegram", "discord"],
                }
            }
        },
        "description": "Full conditions defined in hermes metadata",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {
            "metadata": {
                "hermes": {
                    "requires_tools": ["git"],
                }
            }
        },
        "description": "Partial conditions with requires_tools only",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {
            "metadata": {
                "hermes": {
                    "fallback_for_toolsets": ["browser"],
                }
            }
        },
        "description": "Partial conditions with fallback_for_toolsets only",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {
            "metadata": {
                "hermes": {
                    "session_platforms": ["msteams", "slack"],
                }
            }
        },
        "description": "Partial conditions with session_platforms only",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {
            "metadata": {
                "hermes": {
                    "requires_tools": "terminal",
                    "fallback_for_toolsets": 42,
                    "requires_toolsets": None,
                    "session_platforms": True,
                }
            }
        },
        "description": "Hermes condition fields with non-list scalar values",
        "provenance": None,
    },
    {
        "relative_path": "general/cond-skill/SKILL.md",
        "frontmatter": {
            "metadata": {
                "author": "alice",
                "version": "1.0",
                "hermes": {
                    "requires_tools": ["docker"],
                },
            }
        },
        "description": "Metadata containing extra non-hermes keys",
        "provenance": None,
    },
    {
        "relative_path": "general/extra-keys/SKILL.md",
        "frontmatter": {
            "name": "clean-skill",
            "author": "someone",
            "version": "2.0",
            "tags": ["search"],
        },
        "description": "Skill with extra frontmatter keys",
        "provenance": None,
    },
    # 14. Description variations
    {
        "relative_path": "general/desc-skill/SKILL.md",
        "frontmatter": {},
        "description": "Standard single-line description",
        "provenance": None,
    },
    {
        "relative_path": "general/desc-skill/SKILL.md",
        "frontmatter": {},
        "description": "",
        "provenance": None,
    },
    {
        "relative_path": "general/desc-skill/SKILL.md",
        "frontmatter": {},
        "description": "   ",
        "provenance": None,
    },
    {
        "relative_path": "general/desc-skill/SKILL.md",
        "frontmatter": {},
        "description": "Line 1\nLine 2\nLine 3",
        "provenance": None,
    },
    {
        "relative_path": "general/desc-skill/SKILL.md",
        "frontmatter": {},
        "description": "日本語の説明 🚀 Café résumé",
        "provenance": None,
    },
    {
        "relative_path": "general/desc-skill/SKILL.md",
        "frontmatter": {},
        "description": None,
        "provenance": None,
    },
    {
        "relative_path": "general/desc-skill/SKILL.md",
        "frontmatter": {},
        "description": True,
        "provenance": None,
    },
    # 15. Realistic combined cases
    {
        "relative_path": "_org/prod-org/deploy/kubernetes/SKILL.md",
        "frontmatter": {
            "name": "k8s-deploy",
            "platforms": ["linux", "darwin"],
            "metadata": {
                "hermes": {
                    "requires_tools": ["kubectl", "helm"],
                    "session_platforms": ["cli"],
                }
            },
        },
        "description": "Production Kubernetes deployment automation",
        "provenance": json.dumps({"author_device": "ci-bot-01", "author_user_id": "usr_ci"}),
    },
    {
        "relative_path": "SKILL.md",
        "frontmatter": {
            "name": "root-override",
            "platforms": ["win32"],
            "metadata": {
                "hermes": {
                    "fallback_for_tools": ["old_tool"],
                }
            },
        },
        "description": "Root level skill with complete metadata",
        "provenance": None,
    },
    {
        "relative_path": "_org/acme-corp/SKILL.md",
        "frontmatter": {
            "name": "org-entrypoint",
        },
        "description": "Org root skill with broken provenance",
        "provenance": "{unclosed json",
    },
    {
        "relative_path": "dev/tools/search/engine/SKILL.md",
        "frontmatter": {
            "name": "エンジン",
            "platforms": ["linux"],
            "metadata": {
                "author": "developer",
                "hermes": {
                    "session_platforms": ["cli", "telegram"],
                },
            },
        },
        "description": "Multi-level search engine skill with unicode name",
        "provenance": None,
    },
    # 16. Error cases: Non-dict frontmatter (AttributeError)
    {
        "relative_path": "general/err-skill/SKILL.md",
        "frontmatter": None,
        "description": "Frontmatter is None",
        "provenance": None,
    },
    {
        "relative_path": "general/err-skill/SKILL.md",
        "frontmatter": "not a dict",
        "description": "Frontmatter is string scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/err-skill/SKILL.md",
        "frontmatter": 999,
        "description": "Frontmatter is integer scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/err-skill/SKILL.md",
        "frontmatter": [1, 2, 3],
        "description": "Frontmatter is list",
        "provenance": None,
    },
    # 17. Error cases: Invalid scalar platforms (TypeError)
    {
        "relative_path": "general/err-skill/SKILL.md",
        "frontmatter": {"platforms": 123},
        "description": "Platforms is truthy integer scalar",
        "provenance": None,
    },
    {
        "relative_path": "general/err-skill/SKILL.md",
        "frontmatter": {"platforms": True},
        "description": "Platforms is boolean True",
        "provenance": None,
    },
    {
        "relative_path": "general/err-skill/SKILL.md",
        "frontmatter": {"platforms": 3.14},
        "description": "Platforms is float scalar",
        "provenance": None,
    },
    # 18. Error cases: Path outside skills_dir (ValueError)
    {
        "relative_path": "/outside/SKILL.md",
        "frontmatter": {},
        "description": "Absolute path outside skills_dir",
        "provenance": None,
    },
    {
        "relative_path": "/tmp/abs/nested/SKILL.md",
        "frontmatter": {},
        "description": "Another absolute path outside skills_dir",
        "provenance": None,
    },
]


def evaluate_case(case_def: dict[str, Any], skills_dir: Path) -> dict[str, Any]:
    """Execute Python oracle against an isolated skills directory."""
    rel_path_str = case_def["relative_path"]
    frontmatter = case_def["frontmatter"]
    description = case_def["description"]
    provenance = case_def.get("provenance")

    if rel_path_str.startswith("/"):
        skill_file = Path(rel_path_str)
    else:
        skill_file = skills_dir / rel_path_str
        skill_file.parent.mkdir(parents=True, exist_ok=True)
        skill_file.touch()

    if provenance is not None:
        parts = Path(rel_path_str).parts
        if len(parts) >= 3 and parts[0] == "_org":
            org_id = parts[1]
            prov_file = skills_dir / "_org" / org_id / ".org-provenance.json"
            prov_file.parent.mkdir(parents=True, exist_ok=True)
            prov_file.write_text(provenance, encoding="utf-8")
        else:
            prov_file = skills_dir / "_org" / "dummy" / ".org-provenance.json"
            prov_file.parent.mkdir(parents=True, exist_ok=True)
            prov_file.write_text(provenance, encoding="utf-8")

    expected = None
    error = None
    try:
        expected = _build_snapshot_entry(skill_file, skills_dir, frontmatter, description)
    except Exception as exc:
        error = type(exc).__name__

    return {
        "relative_path": rel_path_str,
        "frontmatter": frontmatter,
        "description": description,
        "provenance": provenance,
        "expected": expected,
        "error": error,
    }


def generate_cases() -> list[dict[str, Any]]:
    """Generate all oracle evaluated cases using isolated temporary skills directories."""
    evaluated_cases: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="skill-entry-") as temp_dir:
        base_dir = Path(temp_dir)
        for idx, raw_case in enumerate(RAW_CASES):
            case_skills_dir = base_dir / f"case_{idx}" / "skills"
            case_skills_dir.mkdir(parents=True, exist_ok=True)
            evaluated = evaluate_case(raw_case, case_skills_dir)
            evaluated_cases.append(evaluated)
    return evaluated_cases


def main() -> None:
    cases = generate_cases()
    content = json.dumps(cases, indent=2) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not OUT_PATH.exists():
            raise SystemExit(f"Golden file does not exist: {OUT_PATH}")
        existing = OUT_PATH.read_text(encoding="utf-8")
        if existing != content:
            raise SystemExit(f"Golden file differs from Python oracle: {OUT_PATH}")
        print(f"Verified {len(cases)} skill entry cases match Python oracle")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_skill_entry_goldens.py [--check]")
    else:
        OUT_PATH.write_text(content, encoding="utf-8")
        print(f"Generated {len(cases)} skill entry cases to {OUT_PATH}")


if __name__ == "__main__":
    main()
