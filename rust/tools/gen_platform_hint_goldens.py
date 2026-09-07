#!/usr/bin/env python3
"""Execute the Python platform override resolver without importing the agent."""
import ast
import itertools
import json
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/system_prompt.py").read_text())
function = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
                and node.name == "_resolve_platform_hint")
scope = {"Any": Any}
exec(compile(ast.Module(body=[function], type_ignores=[]), "agent/system_prompt.py", "exec"), scope)
values = [None, False, 0, [], {}, "", " \u001c ", " extra ",
          {"replace": " replacement ", "append": " extra "},
          {"replace": "", "append": " extra "},
          {"replace": False, "append": []},
          {"replace": " replacement ", "append": "\u00a0"}]
overrides = [None, False, [], "bad", {}]
overrides.extend({"telegram": value, "": value} for value in values)
cases = []
for platform, default, override in itertools.product(
    ["", "telegram", "discord"], ["", " default ", "\u001c default\n"], overrides
):
    expected = scope["_resolve_platform_hint"](
        SimpleNamespace(_platform_hint_overrides=override), platform, default
    )
    cases.append(dict(platform=platform, default=default, overrides=override, expected=expected))
path = root / "rust/tools/platform-hint-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_platform_hint_goldens.py [--check]")
print(f"Verified {len(cases)} platform override cases")

# Execute the real selection block with only external loaders substituted.
builder = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
               and node.name == "build_system_prompt_parts")
start = next(i for i, node in enumerate(builder.body) if isinstance(node, ast.Assign)
             and any(isinstance(t, ast.Name) and t.id == "platform_key" for t in node.targets))
end = next(i for i, node in enumerate(builder.body) if isinstance(node, ast.AnnAssign)
           and isinstance(node.target, ast.Name) and node.target.id == "context_parts")
code = compile(ast.Module(body=builder.body[start:end], type_ignores=[]), "agent/system_prompt.py", "exec")
scope.update(json.loads((root / "rust/tools/system-prompt-guidance.json").read_text()))
utils_tree = ast.parse((root / "utils.py").read_text())
truthy_nodes = [node for node in utils_tree.body
               if (isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "TRUTHY_STRINGS" for t in node.targets))
               or (isinstance(node, ast.FunctionDef) and node.name == "is_truthy_value")]
exec(compile(ast.Module(body=truthy_nodes, type_ignores=[]), "utils.py", "exec"), scope)
pane_nodes = [node for node in tree.body
              if (isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "_TUI_EMBEDDED_PANE_CLARIFIER" for t in node.targets))
              or (isinstance(node, ast.FunctionDef) and node.name == "_tui_embedded_pane_clarifier")]
exec(compile(ast.Module(body=pane_nodes, type_ignores=[]), "agent/system_prompt.py", "exec"), scope)
config_module = ModuleType("hermes_cli.config")
registry_module = ModuleType("gateway.platform_registry")
registry_module.platform_registry = SimpleNamespace(get=lambda key: SimpleNamespace(platform_hint=" plugin hint "))
sys.modules["hermes_cli.config"] = config_module
sys.modules["gateway.platform_registry"] = registry_module
configs = [{}, {"gateway": {"platforms": {"telegram": {"extra": {"rich_messages": True}}}}},
           {"gateway": 1, "platforms": {"telegram": {"extra": {"rich_messages": True}}}}]
for gateway, top in itertools.product([None, [], {"rich_messages": True}],
                                      [None, [], {}, {"rich_messages": False}, {"rich_messages": "yes"}]):
    configs.append({"gateway": {"platforms": {"telegram": {"extra": gateway}}},
                    "platforms": {"telegram": {"extra": top}}})
selections = []
for platform, config, override in itertools.product(
    ["", " TELEGRAM\u001c", "discord", "custom", "tui"], configs,
    [{}, {"telegram": {"replace": "replacement", "append": "tail"}}, {"custom": " extra "}]
):
    config_module.load_config_readonly = lambda: config
    scope.update(agent=SimpleNamespace(platform=platform, _platform_hint_overrides=override), post_workspace_parts=[])
    scope["os"] = SimpleNamespace(getenv=lambda name: None)
    exec(code, scope)
    selections.append(dict(platform=platform, config=config, overrides=override,
                           expected=scope["_effective_hint"]))
for platform, flag, text in itertools.product(
    ["tui", " TUI ", "cli"], [None, "", "0", "1", " TRUE\u001c", "yes", "on", "false", "enabled"],
    ["pane", " ", "pane" + scope["_TUI_EMBEDDED_PANE_CLARIFIER"]]
):
    override = {platform.strip().lower(): {"replace": text}}
    config_module.load_config_readonly = lambda: {}
    scope.update(agent=SimpleNamespace(platform=platform, _platform_hint_overrides=override),
                 post_workspace_parts=[], os=SimpleNamespace(getenv=lambda name: flag))
    exec(code, scope)
    selections.append(dict(platform=platform, config={}, overrides=override,
                           desktop_terminal=flag, expected=scope["_effective_hint"]))
path = root / "rust/tools/platform-hint-selection-goldens.json"
text = json.dumps(selections, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f"Verified {len(selections)} platform selection cases")
