#!/usr/bin/env python3
"""Execute Python's deterministic batch ID repair before tool execution."""
import ast
import copy
import json
import logging
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/unique-call-goldens.json'
tree = ast.parse((ROOT / 'agent/message_sanitization.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'uniquify_tool_call_ids')
namespace = {'logger': logging.getLogger('oracle')}
logging.disable(logging.CRITICAL)
exec(compile(ast.Module(body=[node], type_ignores=[]), 'unique-call', 'exec'), namespace)
rows = []
for identifier in ['a', ' a ', 'a|item', ' a | item', '|item', '', None, 12]:
    for pairing in [None, '', 'pair', 'pair|item', 12]:
        calls = [{'id': identifier, 'call_id': pairing, 'function': {'name': 'lookup', 'arguments': '{}'}} for _ in range(3)]
        rows.append({'calls': calls, 'expected': namespace['uniquify_tool_call_ids'](copy.deepcopy(calls))})
for identifiers in [['a', 'a_d2', 'a', 'a'], ['a', 'a', 'a_d2'], ['a|one', 'a|two'], ['a', 'b', 'a', 'b']]:
    calls = [{'id': value} for value in identifiers]
    rows.append({'calls': calls, 'expected': namespace['uniquify_tool_call_ids'](copy.deepcopy(calls))})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_unique_call_goldens.py [--check]')
print(f'Verified {len(rows)} unique-call cases')
