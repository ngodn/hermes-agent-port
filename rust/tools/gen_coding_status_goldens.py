#!/usr/bin/env python3
"""Execute the Python workspace porcelain-v2 parser on valid and damaged input."""
import ast
import itertools
import json
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/coding_context.py").read_text())
fn = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "_parse_status")
scope = {}
exec(compile(ast.Module(body=[fn], type_ignores=[]), "agent/coding_context.py", "exec"), scope)
lines = ["", "# branch.head main", "# branch.head (detached)", "# branch.head name with spaces  ",
         "# branch.head", "# branch.upstream origin/main", "# branch.ab +2 -3", "# branch.ab ++2 --3",
         "# branch.ab", "1 MM ignored fields", "1 .M ignored fields", "2 R. rename", "1 .", "1 ",
         "u UU conflict", "? untracked", "! ignored", "1\tMM ignored", "1 \u00a0MM ignored"]
cases = []
for first, second, separator in itertools.product(lines, ["", "# branch.head replaced", "? another"], ["\n", "\r\n", "\u2028"]):
    text = first + separator + second
    try:
        branch, counts = scope["_parse_status"](text)
        cases.append(dict(input=text, expected=dict(branch=branch, counts=counts)))
    except IndexError:
        cases.append(dict(input=text, error=True))
path = root / "rust/tools/coding-status-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_coding_status_goldens.py [--check]")
print(f"Verified {len(cases)} workspace status cases")
