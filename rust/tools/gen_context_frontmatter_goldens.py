#!/usr/bin/env python3
"""Extract actual Python frontmatter behavior, including malformed delimiters."""
import ast
import itertools
import json
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/prompt_builder.py").read_text())
function = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
                and node.name == "_strip_yaml_frontmatter")
scope = {}
exec(compile(ast.Module(body=[function], type_ignores=[]), "agent/prompt_builder.py", "exec"), scope)
cases = []
for prefix, middle, suffix in itertools.product(
    ["", "\ufeff", "\ufeff\ufeff", " "], ["plain", "---", "---\nx: y", "---\r\nx: y", "----\n中文"],
    ["", "\n---", "\n---\n", "\n---\n\nbody", "\n----tail", "\n---\r\nbody"]
):
    text = prefix + middle + suffix
    cases.append(dict(input=text, expected=scope["_strip_yaml_frontmatter"](text)))
path = root / "rust/tools/context-frontmatter-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_context_frontmatter_goldens.py [--check]")
print(f"Verified {len(cases)} context frontmatter cases")
