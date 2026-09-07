#!/usr/bin/env python3
"""Run Python's context-file truncator and capture exact text and warnings."""
import ast
import itertools
import json
import sys
from pathlib import Path
from types import SimpleNamespace
from typing import Optional

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/prompt_builder.py").read_text())
function = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
                and node.name == "_truncate_content")
scope = dict(Optional=Optional, logger=SimpleNamespace(warning=lambda msg: None))
for node in tree.body:
    if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in {
        "CONTEXT_TRUNCATE_HEAD_RATIO", "CONTEXT_TRUNCATE_TAIL_RATIO"
    } for t in node.targets):
        exec(compile(ast.Module(body=[node], type_ignores=[]), "agent/prompt_builder.py", "exec"), scope)
exec(compile(ast.Module(body=[function], type_ignores=[]), "agent/prompt_builder.py", "exec"), scope)
cases = []
for content, limit, path in itertools.product(
    ["", "abc", "🚀e\u0301\n中文\r\nend", "0123456789" * 12],
    [0, 1, 2, 4, 5, 9, 10, 11, 99, 120, 200], [None, "", "/profiles/red/SOUL.md"]
):
    warnings = []
    scope["_record_truncation_warning"] = warnings.append
    expected = scope["_truncate_content"](content, "SOUL.md", max_chars=limit, read_path=path)
    cases.append(dict(content=content, filename="SOUL.md", max_chars=limit, read_path=path,
                      expected=expected, warning=warnings[0] if warnings else None))
path = root / "rust/tools/context-truncation-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_context_truncation_goldens.py [--check]")
print(f"Verified {len(cases)} context truncations")
