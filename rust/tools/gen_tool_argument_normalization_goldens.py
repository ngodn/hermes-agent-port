#!/usr/bin/env python3
"""Execute the actual pre-execution argument-normalization loop."""
import ast
import json
import sys
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/tool-argument-normalization-goldens.json'
tree = ast.parse((ROOT / 'agent/conversation_loop.py').read_text())
loop = next(node for node in ast.walk(tree) if isinstance(node, ast.For) and node.body
            and isinstance(node.body[0], ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'args' for t in node.body[0].targets)
            and isinstance(node.iter, ast.Attribute) and node.iter.attr == 'tool_calls')
program = compile(ast.Module(body=[loop], type_ignores=[]), 'argument-normalization', 'exec')
values = [None, '', ' ', '\u001c\u001f', '\u200b', '{}', ' {"x": 1} ', 'broken', True, False, 0, 12, 1e-5,
          {}, [], {'z': 1, 'a': 2}, ['猫', '😀', '\x7f', '\x00\n'], {'x': [True, False, None, {'a': 1.0, 'b': 1e20}]},
          {'"\\': 'text\t\r'}, {'x': 1e-7}, {'x': -0.0}]
rows = []
for raw in values:
    tc = SimpleNamespace(function=SimpleNamespace(name='fixture', arguments=raw))
    scope = {'assistant_message': SimpleNamespace(tool_calls=[tc]), 'json': json, '_mixed_invalid_batch': False, 'invalid_json_args': []}
    exec(program, scope)
    rows.append({'raw': raw, 'expected': tc.function.arguments})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_tool_argument_normalization_goldens.py [--check]')
print(f'Verified {len(rows)} argument-normalization cases')
