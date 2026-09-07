#!/usr/bin/env python3
"""Extract the Python coding detector's marker, surface and scan constants."""
import ast
import json
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[2]
names = {"INTERACTIVE_CODING_PLATFORMS", "_PROJECT_MARKERS", "_CODE_EXTENSIONS",
         "_CODE_SCAN_SKIP_DIRS", "_CODE_SCAN_MAX_ENTRIES"}
tree = ast.parse((root / "agent/coding_context.py").read_text())
scope = {}
for node in tree.body:
    if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in names for t in node.targets):
        exec(compile(ast.Module(body=[node], type_ignores=[]), "agent/coding_context.py", "exec"), scope)
data = {key: sorted(value) if isinstance(value, (set, frozenset)) else value
        for key, value in scope.items() if key in names}
text = json.dumps(data, indent=2) + "\n"
path = root / "rust/tools/coding-detection-constants.json"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_coding_detection_constants.py [--check]")
print(f"Verified {len(data)} coding detection constant groups")
