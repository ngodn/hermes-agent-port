#!/usr/bin/env python3
"""Execute the summary helper's actual regex call for cleanup comparisons."""
import ast
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/summary-cleanup-goldens.json'
tree = ast.parse((ROOT / 'agent/chat_completion_helpers.py').read_text())
helper = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'handle_max_iterations')
call = next(n for n in ast.walk(helper) if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute)
            and isinstance(n.func.value, ast.Name) and n.func.value.id == 're' and n.func.attr == 'sub')
expression = compile(ast.Expression(call), 'summary-cleanup', 'eval')
constants = ast.parse((ROOT / 'agent/context_compressor.py').read_text())
request = next(ast.literal_eval(n.value) for n in constants.body if isinstance(n, ast.Assign)
               and any(isinstance(t, ast.Name) and t.id == 'MAX_ITERATIONS_SUMMARY_REQUEST' for t in n.targets))
values = ['', ' answer ', '<think>secret</think> answer', '<think>secret</think>',
          '<think>unclosed', '<THINK>secret</THINK> answer', 'a<think>one</think> b<think>two</think> c',
          '<think>a\nb</think>\nreply', '<think>outer<think>inner</think>end</think>',
          'before </think> after']
values += [f'<think>x</think>{space}answer{space}' for space in ['\x1c', '\x1f', '\u0085', '\u00a0', '\u200b', '\u2028', '\u3000']]
rows = []
for value in values:
    text = value.strip()
    expected = eval(expression, {'re': re, 'final_response': text}).strip() if '<think>' in text else text
    rows.append({'input': value, 'expected': expected})
text = json.dumps({'request': request, 'cases': rows}, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_summary_cleanup_goldens.py [--check]')
print(f'Verified {len(rows)} summary cleanup cases')
