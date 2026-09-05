#!/usr/bin/env python3
"""Execute Python's duplicate-call filter on raw argument spellings."""
import ast
import json
import logging
import sys
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/duplicate-call-goldens.json'
tree = ast.parse((ROOT / 'run_agent.py').read_text())
cls = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == 'AIAgent')
node = next(node for node in cls.body if isinstance(node, ast.FunctionDef) and node.name == '_deduplicate_tool_calls')
node.decorator_list = []
namespace = {'json': json, 'logger': logging.getLogger('oracle')}
logging.disable(logging.CRITICAL)
exec(compile(ast.Module(body=[node], type_ignores=[]), 'deduplicate-tool-calls', 'exec'), namespace)
pairs = [('{}', ' {} '), ('{"b":2,"a":1}', '{"a":1,"b":2}'),
         ('{"nested":{"b":2,"a":1}}', '{"nested":{"a":1,"b":2}}'),
         ('{"a":1}', '{"a":1.0}'), ('{"a":1e0}', '{"a":1.0}'),
         ('{"x":"猫"}', '{"x":"\\u732b"}'), ('[1,2]', '[2,1]'), ('null','null'),
         ('broken','broken'), ('broken',' broken '), ('{"a":1}', '{"a":2}'),
         ('{"x":true}', '{"x":1}')]
rows = []
for left, right in pairs:
    for names in [('one','one'), ('one','two')]:
        calls = [{'id':str(index), 'name':name, 'arguments':raw} for index,(name,raw) in enumerate([(names[0],left),(names[1],right),(names[0],left)])]
        sdk = [SimpleNamespace(id=call['id'], function=SimpleNamespace(name=call['name'], arguments=call['arguments'])) for call in calls]
        expected = namespace['_deduplicate_tool_calls'](sdk)
        rows.append({'calls':calls, 'ids':[call.id for call in expected]})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_duplicate_call_goldens.py [--check]')
print(f'Verified {len(rows)} duplicate-call cases')
