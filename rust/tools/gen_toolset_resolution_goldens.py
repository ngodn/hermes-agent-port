#!/usr/bin/env python3
"""Execute actual toolsets.resolve_toolset to generate toolset resolution goldens."""

import json
from pathlib import Path
import sys
import types
from typing import Any, Dict, List, Optional

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import toolsets  # noqa: E402

OUT = ROOT / "rust/tools/toolset-resolution-goldens.json"


def build_cases() -> List[Dict[str, Any]]:
    """Define test cases covering all toolset resolution specifications."""
    return [
        # 1. static leaf
        {
            "static": {
                "terminal": {
                    "tools": ["terminal", "process_manage"],
                    "includes": [],
                },
            },
            "registry": {},
            "aliases": [],
            "platforms": [],
            "core": ["read_file"],
            "name": "terminal",
            "include_registry": True,
        },
        # 2. static composite
        {
            "static": {
                "debugging": {
                    "tools": ["gdb"],
                    "includes": ["file_tools", "web_tools"],
                },
                "file_tools": {
                    "tools": ["read_file", "write_file"],
                    "includes": [],
                },
                "web_tools": {
                    "tools": ["web_search"],
                    "includes": [],
                },
            },
            "registry": {},
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "debugging",
            "include_registry": True,
        },
        # 3. 2-node cycle
        {
            "static": {
                "cycle_a": {
                    "tools": ["tool_a"],
                    "includes": ["cycle_b"],
                },
                "cycle_b": {
                    "tools": ["tool_b"],
                    "includes": ["cycle_a"],
                },
            },
            "registry": {},
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "cycle_a",
            "include_registry": True,
        },
        # 4. 3-node cycle with attached leaf
        {
            "static": {
                "cycle_x": {
                    "tools": ["tool_x"],
                    "includes": ["cycle_y"],
                },
                "cycle_y": {
                    "tools": ["tool_y"],
                    "includes": ["cycle_z"],
                },
                "cycle_z": {
                    "tools": ["tool_z"],
                    "includes": ["cycle_x", "leaf_d"],
                },
                "leaf_d": {
                    "tools": ["tool_d"],
                    "includes": [],
                },
            },
            "registry": {},
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "cycle_x",
            "include_registry": True,
        },
        # 5. self loop
        {
            "static": {
                "self_loop": {
                    "tools": ["tool_self"],
                    "includes": ["self_loop"],
                },
            },
            "registry": {},
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "self_loop",
            "include_registry": True,
        },
        # 6. classic diamond
        {
            "static": {
                "top": {
                    "tools": ["t_top"],
                    "includes": ["left", "right"],
                },
                "left": {
                    "tools": ["t_left"],
                    "includes": ["bottom"],
                },
                "right": {
                    "tools": ["t_right"],
                    "includes": ["bottom"],
                },
                "bottom": {
                    "tools": ["t_bottom"],
                    "includes": [],
                },
            },
            "registry": {},
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "top",
            "include_registry": True,
        },
        # 7. diamond with registry overlays
        {
            "static": {
                "diamond_root": {
                    "tools": ["t_root"],
                    "includes": ["branch_a", "branch_b"],
                },
                "branch_a": {
                    "tools": ["t_a"],
                    "includes": ["base"],
                },
                "branch_b": {
                    "tools": ["t_b"],
                    "includes": ["base"],
                },
                "base": {
                    "tools": ["t_base"],
                    "includes": [],
                },
            },
            "registry": {
                "base": ["reg_base_extra"],
                "branch_a": ["reg_a_extra"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "diamond_root",
            "include_registry": True,
        },
        # 8. all wildcard
        {
            "static": {
                "ts_alpha": {
                    "tools": ["alpha_1", "alpha_2"],
                    "includes": [],
                },
                "ts_beta": {
                    "tools": ["beta_1"],
                    "includes": [],
                },
            },
            "registry": {
                "plugin_gamma": ["gamma_tool"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "all",
            "include_registry": True,
        },
        # 9. * wildcard
        {
            "static": {
                "ts_alpha": {
                    "tools": ["alpha_1", "alpha_2"],
                    "includes": [],
                },
                "ts_beta": {
                    "tools": ["beta_1"],
                    "includes": [],
                },
            },
            "registry": {
                "plugin_gamma": ["gamma_tool"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "*",
            "include_registry": True,
        },
        # 10. all wildcard static only
        {
            "static": {
                "ts_static1": {
                    "tools": ["s1_tool"],
                    "includes": [],
                },
                "ts_static2": {
                    "tools": ["s2_tool"],
                    "includes": [],
                },
            },
            "registry": {
                "ts_static1": ["reg_s1_tool"],
                "plugin_extra": ["plug_tool"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "all",
            "include_registry": False,
        },
        # 11. registry overlays basic
        {
            "static": {
                "code": {
                    "tools": ["format", "lint"],
                    "includes": [],
                },
            },
            "registry": {
                "code": ["typecheck", "refactor"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "code",
            "include_registry": True,
        },
        # 12. registry overlays static only
        {
            "static": {
                "code": {
                    "tools": ["format", "lint"],
                    "includes": [],
                },
            },
            "registry": {
                "code": ["typecheck", "refactor"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "code",
            "include_registry": False,
        },
        # 13. plugin toolset direct
        {
            "static": {},
            "registry": {
                "docker_tools": ["docker_build", "docker_run"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "docker_tools",
            "include_registry": True,
        },
        # 14. plugin toolset via alias
        {
            "static": {},
            "registry": {
                "mcp_github": ["gh_issue_list", "gh_pr_create"],
            },
            "aliases": [
                ["github", "mcp_github"],
            ],
            "platforms": [],
            "core": [],
            "name": "github",
            "include_registry": True,
        },
        # 15. alias static collision
        {
            "static": {
                "git": {
                    "tools": ["git_commit"],
                    "includes": [],
                },
            },
            "registry": {
                "mcp_git_plugin": ["mcp_push"],
            },
            "aliases": [
                ["git", "mcp_git_plugin"],
            ],
            "platforms": [],
            "core": [],
            "name": "git",
            "include_registry": True,
        },
        # 16. alias static collision in all
        {
            "static": {
                "git": {
                    "tools": ["git_commit"],
                    "includes": [],
                },
            },
            "registry": {
                "mcp_git_plugin": ["mcp_push"],
            },
            "aliases": [
                ["git", "mcp_git_plugin"],
            ],
            "platforms": [],
            "core": [],
            "name": "all",
            "include_registry": True,
        },
        # 17. alias first-choice listing
        {
            "static": {},
            "registry": {
                "postgres_canonical": ["pg_query", "pg_schema"],
            },
            "aliases": [
                ["pg_primary", "postgres_canonical"],
                ["pg_secondary", "postgres_canonical"],
            ],
            "platforms": [],
            "core": [],
            "name": "all",
            "include_registry": True,
        },
        # 18. alias first-choice listing with first alias colliding
        {
            "static": {
                "db_alias1": {
                    "tools": ["static_db_tool"],
                    "includes": [],
                },
            },
            "registry": {
                "db_canonical": ["pg_query"],
            },
            "aliases": [
                ["db_alias1", "db_canonical"],
                ["db_alias2", "db_canonical"],
            ],
            "platforms": [],
            "core": [],
            "name": "all",
            "include_registry": True,
        },
        # 19. plugin platform fallback
        {
            "static": {},
            "registry": {
                "slackbot": ["slack_post_msg", "slack_upload"],
            },
            "aliases": [],
            "platforms": ["slackbot"],
            "core": ["terminal", "web_search"],
            "name": "hermes-slackbot",
            "include_registry": True,
        },
        # 20. plugin platform fallback static only
        {
            "static": {},
            "registry": {
                "slackbot": ["slack_post_msg"],
            },
            "aliases": [],
            "platforms": ["slackbot"],
            "core": ["terminal", "web_search"],
            "name": "hermes-slackbot",
            "include_registry": False,
        },
        # 21. plugin platform not registered
        {
            "static": {},
            "registry": {},
            "aliases": [],
            "platforms": ["other_plat"],
            "core": ["core_tool"],
            "name": "hermes-unregistered",
            "include_registry": True,
        },
        # 22. registry unavailable null on static composite
        {
            "static": {
                "bundle": {
                    "tools": ["b_tool"],
                    "includes": ["sub_bundle"],
                },
                "sub_bundle": {
                    "tools": ["sub_tool"],
                    "includes": [],
                },
            },
            "registry": None,
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "bundle",
            "include_registry": True,
        },
        # 23. registry unavailable null on plugin platform
        {
            "static": {},
            "registry": None,
            "aliases": [],
            "platforms": ["customplat"],
            "core": ["core_files", "core_terminal"],
            "name": "hermes-customplat",
            "include_registry": True,
        },
        # 24. static only on plugin-only toolset
        {
            "static": {},
            "registry": {
                "plugin_ts": ["p_tool"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "plugin_ts",
            "include_registry": False,
        },
        # 25. deep nested composite with overlays
        {
            "static": {
                "lvl1": {
                    "tools": ["t1"],
                    "includes": ["lvl2"],
                },
                "lvl2": {
                    "tools": ["t2"],
                    "includes": ["lvl3"],
                },
                "lvl3": {
                    "tools": ["t3"],
                    "includes": ["lvl4"],
                },
                "lvl4": {
                    "tools": ["t4"],
                    "includes": [],
                },
            },
            "registry": {
                "lvl2": ["t2_reg"],
                "lvl4": ["t4_reg"],
            },
            "aliases": [],
            "platforms": [],
            "core": [],
            "name": "lvl1",
            "include_registry": True,
        },
    ]


def execute_case(case: Dict[str, Any]) -> List[str]:
    """Execute actual toolsets.resolve_toolset with patched environment."""
    toolsets._resolve_toolset_memo.clear()
    toolsets.TOOLSETS = {
        name: {
            "tools": list(defn.get("tools", [])),
            "includes": list(defn.get("includes", [])),
        }
        for name, defn in case["static"].items()
    }
    toolsets._HERMES_CORE_TOOLS = list(case["core"])

    sys.modules.setdefault("tools", types.ModuleType("tools"))
    if case["registry"] is None:
        mod_reg = types.ModuleType("tools.registry")
        sys.modules["tools.registry"] = mod_reg
    else:
        entries = []
        for ts, tool_names in case["registry"].items():
            for tool_name in tool_names:
                entries.append(types.SimpleNamespace(name=tool_name, toolset=ts))
        alias_dict = dict(case["aliases"])
        fake_reg = types.SimpleNamespace(
            get_tool_names_for_toolset=lambda ts, m=case["registry"]: sorted(m.get(ts, [])),
            get_toolset_alias_target=lambda a, d=alias_dict: d.get(a),
            get_registered_toolset_names=lambda m=case["registry"]: sorted(m.keys()),
            get_registered_toolset_aliases=lambda d=alias_dict: dict(d),
            get_all_entries=lambda e=entries: list(e),
            _generation=1,
        )
        mod_reg = types.ModuleType("tools.registry")
        mod_reg.registry = fake_reg
        sys.modules["tools.registry"] = mod_reg

    sys.modules.setdefault("gateway", types.ModuleType("gateway"))
    mod_plat = types.ModuleType("gateway.platform_registry")
    plat_set = set(case["platforms"])
    mod_plat.platform_registry = types.SimpleNamespace(
        is_registered=lambda p, s=plat_set: p in s
    )
    sys.modules["gateway.platform_registry"] = mod_plat

    result = toolsets.resolve_toolset(
        case["name"],
        include_registry=case["include_registry"],
    )
    return sorted(result)


def generate() -> str:
    cases = build_cases()
    orig_toolsets = toolsets.TOOLSETS
    orig_core = toolsets._HERMES_CORE_TOOLS
    orig_tools_reg = sys.modules.get("tools.registry")
    orig_plat_reg = sys.modules.get("gateway.platform_registry")

    try:
        for case in cases:
            case["expected"] = execute_case(case)
    finally:
        toolsets._resolve_toolset_memo.clear()
        toolsets.TOOLSETS = orig_toolsets
        toolsets._HERMES_CORE_TOOLS = orig_core
        if orig_tools_reg is not None:
            sys.modules["tools.registry"] = orig_tools_reg
        else:
            sys.modules.pop("tools.registry", None)
        if orig_plat_reg is not None:
            sys.modules["gateway.platform_registry"] = orig_plat_reg
        else:
            sys.modules.pop("gateway.platform_registry", None)

    return json.dumps(cases, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if not OUT.exists():
            raise SystemExit(f"{OUT} does not exist")
        if OUT.read_text() != content:
            raise SystemExit("Toolset resolution goldens differ from generated output")
    elif not sys.argv[1:]:
        OUT.write_text(content)
    else:
        raise SystemExit("usage: gen_toolset_resolution_goldens.py [--check]")
    print(f"Verified {len(json.loads(content))} toolset resolution cases")
