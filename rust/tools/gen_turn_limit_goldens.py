#!/usr/bin/env python3
"""Execute the reference turn-limit resolver without importing CLI dependencies."""
import ast
import json
import logging
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/turn-limit-goldens.json"
tree = ast.parse((ROOT / "hermes_cli/config.py").read_text())
names = {"TURN_LIMIT_UNLIMITED", "_UNLIMITED_SPELLINGS"}
nodes = [node for node in tree.body if
         (isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in names for t in node.targets))
         or (isinstance(node, ast.FunctionDef) and node.name == "resolve_turn_limit")]
namespace = {"sys": sys, "Any": object, "logger": logging.getLogger(__name__)}
exec(compile(ast.Module(body=nodes, type_ignores=[]), "hermes_cli/config.py", "exec"), namespace)
values = [None, True, False, {}, [], 0, -5, 1, 8, 120, 2.9, -0.5,
          "", " ", "none", "NULL", " unlimited ", "infinite", "infinity", "inf", "∞",
          "-1", "0", "-20", "120", "1_000", "2.9", "1e2", "0.9", "nan", "+inf", "-inf", "1e999",
          "１２", "١٢", "\u001c12\u001f", "1__2", "_12", "12_", "1_.0", "1e_2", "wrong", "0x10",
          "9223372036854775807"]
rows = []
for default in [sys.maxsize, 17]:
    for raw in values:
        row = {"raw": raw, "default": default}
        try:
            row["expected"] = namespace["resolve_turn_limit"](raw, default)
        except (ValueError, OverflowError) as exc:
            row["error"] = type(exc).__name__
        rows.append(row)
text = json.dumps(rows, ensure_ascii=False, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert OUT.read_text() == text, "Turn-limit fixtures differ from Python"
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit("usage: gen_turn_limit_goldens.py [--check]")
print(f"Verified {len(rows)} turn-limit cases")
