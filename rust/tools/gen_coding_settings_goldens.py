#!/usr/bin/env python3
"""Execute the Python coding-mode and operator-instruction config helpers."""
import ast
import itertools
import json
import sys
from pathlib import Path
from typing import Any, Optional

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/coding_context.py").read_text())
nodes = [node for node in tree.body if isinstance(node, ast.FunctionDef)
         and node.name in {"_coding_mode", "_coding_instructions"}]
scope = dict(Any=Any, Optional=Optional)
exec(compile(ast.Module(body=nodes, type_ignores=[]), "agent/coding_context.py", "exec"), scope)
configs = [[], "bad", {"agent": None}, {"agent": True}, {"agent": "bad"}, {"agent": []}]
for mode, instructions in itertools.product(
    [None, False, True, 0, 1, 1.0, "focus", " STRICT\u001c", "lean", "always", "never", "auto", "invalid"],
    [None, False, 0, " text\u001c", [], [" a ", " ", None, False, 0, {"x": "y"}], {"x": "y"}]
):
    configs.append({"agent": {"coding_context": mode, "coding_instructions": instructions}})
cases = []
for config in configs:
    try:
        cases.append(dict(config=config, mode=scope["_coding_mode"](config), instructions=scope["_coding_instructions"](config)))
    except AttributeError:
        cases.append(dict(config=config, error=True))
path = root / "rust/tools/coding-settings-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_coding_settings_goldens.py [--check]")
print(f"Verified {len(cases)} coding settings cases")
