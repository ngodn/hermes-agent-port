#!/usr/bin/env python3
"""Execute Python's focus selection with its raw-config MCP enabled parser."""
import ast
import itertools
import json
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any, Optional

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/coding_context.py").read_text())
enabled = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "_enabled_mcp_servers")
runtime = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "RuntimeMode")
selection = next(n for n in runtime.body if isinstance(n, ast.FunctionDef) and n.name == "toolset_selection")
tools_tree = ast.parse((root / "hermes_cli/tools_config.py").read_text())
parser = next(n for n in tools_tree.body if isinstance(n, ast.FunctionDef) and n.name == "_parse_enabled_flag")
scope = dict(Any=Any, Optional=Optional)
exec(compile(ast.Module(body=[parser, enabled, selection], type_ignores=[]), "coding-focus-oracle", "exec"), scope)
config_module = ModuleType("hermes_cli.config")
tools_module = ModuleType("hermes_cli.tools_config")
tools_module._parse_enabled_flag = scope["_parse_enabled_flag"]
sys.modules["hermes_cli.config"] = config_module
sys.modules["hermes_cli.tools_config"] = tools_module
raw_configs = [None, [], {}, {"mcp_servers": "invalid"}]
for flag in [None, False, True, 0, -1, 0.0, 1.5, "FALSE", " off\u001c", "yes", "unknown", [], {}]:
    raw_configs.append({"mcp_servers": {"z_first": {}, "candidate": {"enabled": flag}, "bad": False, "coding": {}}})
cases = []
for mode, toolset, raw in itertools.product(["auto", "focus", "on"], [None, "coding", "", "custom"], raw_configs):
    config_module.read_raw_config = lambda: raw
    agent = SimpleNamespace(config_mode=mode, profile=SimpleNamespace(toolset=toolset))
    # Conflicting passed config proves the reference consults raw config.
    expected = scope["toolset_selection"](agent, {"mcp_servers": {"wrong": {}}})
    cases.append(dict(mode=mode, toolset=toolset, raw_config=raw, expected=expected))
path = root / "rust/tools/focus-toolsets-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_focus_toolsets_goldens.py [--check]")
print(f"Verified {len(cases)} focus toolset cases")
