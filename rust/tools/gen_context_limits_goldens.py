#!/usr/bin/env python3
"""Execute Python's context-file config resolvers against typed values."""
import ast
import itertools
import json
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Optional

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/prompt_builder.py").read_text())
names = {"_get_context_file_read_timeout", "_get_context_file_max_chars", "_dynamic_context_file_max_chars"}
constants = {"_CONTEXT_FILE_READ_TIMEOUT_SECS", "CONTEXT_FILE_MAX_CHARS", "_CONTEXT_FILE_CHARS_PER_TOKEN",
             "_CONTEXT_FILE_WINDOW_FRACTION", "_CONTEXT_FILE_DYNAMIC_CEILING"}
nodes = [node for node in tree.body if (isinstance(node, ast.FunctionDef) and node.name in names)
         or (isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in constants for t in node.targets))]
scope = dict(Optional=Optional, logger=SimpleNamespace(debug=lambda *args: None))
exec(compile(ast.Module(body=nodes, type_ignores=[]), "agent/prompt_builder.py", "exec"), scope)
module = ModuleType("hermes_cli.config")
sys.modules["hermes_cli.config"] = module
cases = []
values = [None, False, True, 0, -1, 0.5, 1, 12345.9, "50000", [], {}]
for limit, timeout, context in itertools.product(values, values, [None, False, True, -1, 0, 8192, 128000, 1000000, 10000000, 128000.0, "128000"]):
    config = {"context_file_max_chars": limit, "context_file_read_timeout": timeout}
    module.load_config_readonly = lambda: config
    cases.append(dict(config=config, context_length=context,
                      limit=scope["_get_context_file_max_chars"](context),
                      timeout=scope["_get_context_file_read_timeout"]()))
path = root / "rust/tools/context-limits-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_context_limits_goldens.py [--check]")
print(f"Verified {len(cases)} context limit resolutions")
