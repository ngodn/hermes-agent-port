#!/usr/bin/env python3
"""Execute the Python invalid tool-name error formatter."""
import ast
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/invalid-tool-name-goldens.json'
tree = ast.parse((ROOT / 'agent/conversation_loop.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_invalid_tool_name_error_content')
namespace = {}
exec(compile(ast.Module(body=[node], type_ignores=[]), 'invalid-tool-name', 'exec'), namespace)
rows = []
for name in ['', ' ', '\u001c\u001f', '\u0085', '\u200b', 'typo', ' lookup ', '<tool_call>', "bad'name", '猫']:
    for names in [[], ['terminal'], ['z', 'a'], ['z', 'a', 'a']]:
        rows.append({'name': name, 'valid_names': names, 'expected': namespace['_invalid_tool_name_error_content'](name, names)})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_invalid_tool_name_goldens.py [--check]')
print(f'Verified {len(rows)} invalid-name cases')
