#!/usr/bin/env python3
"""Execute Python's embedder/config hint resolution block with typed values."""
import ast
import itertools
import json
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/prompt_builder.py").read_text())
function = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
                and node.name == "build_environment_hints")
start = next(i for i, node in enumerate(function.body) if isinstance(node, ast.Assign)
             and any(isinstance(t, ast.Name) and t.id == "extra" for t in node.targets))
code = compile(ast.Module(body=function.body[start:-1], type_ignores=[]), "agent/prompt_builder.py", "exec")
module = ModuleType("hermes_cli.config")
sys.modules["hermes_cli.config"] = module
configs = [None, [], "bad", {}, {"agent": True}, {"agent": []}, {"agent": None}]
configs.extend({"agent": {"environment_hint": value}} for value in
               [None, False, True, 0, 1.25, "", " text\u001c", [], ["a", None, True], {}, {"x": "y"}])
cases = []
for env, config in itertools.product([None, "", " \u001c", " env hint "], configs):
    module.load_config_readonly = lambda: config
    scope = dict(os=SimpleNamespace(getenv=lambda name: env),
                 logger=SimpleNamespace(debug=lambda *args: None), hints=[])
    exec(code, scope)
    cases.append(dict(env=env, config=config, expected=scope["hints"][0] if scope["hints"] else None))
path = root / "rust/tools/environment-extra-hint-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_environment_extra_hint_goldens.py [--check]")
print(f"Verified {len(cases)} environment extra hints")
