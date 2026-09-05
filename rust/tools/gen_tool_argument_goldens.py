#!/usr/bin/env python3
"""Execute Python's argument guard after the conversation loop normalization."""
import ast
import json
import sys
from pathlib import Path
from typing import Any, Optional
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/tool-argument-goldens.json'
tree = ast.parse((ROOT / 'agent/tool_executor.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_parse_tool_arguments')
namespace = {'Any': Any, 'Optional': Optional, 'json': json}
exec(compile(ast.Module(body=[node], type_ignores=[]), 'tool-argument-guard', 'exec'), namespace)
tree = ast.parse((ROOT / 'agent/conversation_loop.py').read_text())
loop = next(node for node in ast.walk(tree) if isinstance(node, ast.For) and node.body
            and isinstance(node.body[0], ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'args' for t in node.body[0].targets)
            and isinstance(node.iter, ast.Attribute) and node.iter.attr == 'tool_calls')
normalizer = compile(ast.Module(body=[loop], type_ignores=[]), 'argument-normalization', 'exec')
rows = []
for raw in [None, '', ' ', '{}', '{"x":1}', '{"x":', '{"x":1,}', '[]', '[1]', 'null', 'false', '12', '"text"',
            'None', True, 12, {}, [], '{ "nested": {"unicode": "猫"} }', '{"a":1,"a":2}', '{"x":"line\nbreak"}']:
    tc = SimpleNamespace(function=SimpleNamespace(name='fixture', arguments=raw))
    scope = {'assistant_message': SimpleNamespace(tool_calls=[tc]), 'json': json, '_mixed_invalid_batch': False, 'invalid_json_args': []}
    exec(normalizer, scope)
    arguments, error = namespace['_parse_tool_arguments'](tc.function.arguments)
    rows.append({'raw': raw, 'arguments': arguments, 'error': error, 'syntax_invalid': bool(scope['invalid_json_args']), 'truncated': bool(scope['invalid_json_args']) and not tc.function.arguments.rstrip().endswith(('}', ']'))})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_tool_argument_goldens.py [--check]')
print(f'Verified {len(rows)} tool-argument cases')
