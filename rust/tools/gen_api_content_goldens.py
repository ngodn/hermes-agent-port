#!/usr/bin/env python3
"""Execute the reference replay sidecar substitution on message shapes."""
import ast
import copy
import json
import sys
from pathlib import Path
from typing import Any, Dict, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/api-content-goldens.json'
tree = ast.parse((ROOT / 'agent/turn_context.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'substitute_api_content')
namespace = {'Dict': Dict, 'Any': Any, 'Optional': Optional}
exec(compile(ast.Module(body=[node], type_ignores=[]), 'turn_context', 'exec'), namespace)
rows = []
for role in ['user', 'assistant', 'tool', 'system', 'developer', None]:
    for sidecar in [None, '', ' ', 'previous wire text', ['part'], {}, False, 12]:
        for content in ['clean', [{'type': 'text', 'text': 'clean'}, {'type': 'image_url', 'image_url': {'url': 'data:image/png;base64,AA=='}}]]:
            message = {'role': role, 'content': content, 'api_content': sidecar}
            expected = copy.deepcopy(message)
            namespace['substitute_api_content'](expected)
            rows.append({'input': message, 'expected': expected})
for message in [{}, {'role': 'user'}, {'role': 'user', 'api_content': 'stored'}, {'role': 'assistant', 'content': 'clean'}]:
    expected = copy.deepcopy(message)
    namespace['substitute_api_content'](expected)
    rows.append({'input': message, 'expected': expected})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_api_content_goldens.py [--check]')
print(f'Verified {len(rows)} API content cases')
