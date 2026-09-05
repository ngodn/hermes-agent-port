#!/usr/bin/env python3
"""Execute the reference bare tool-template marker grammar."""
import ast
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/tool-marker-goldens.json'
tree = ast.parse((ROOT / 'agent/conversation_loop.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == '_STALE_MARKER_RE' for t in n.targets))
namespace = {'re': re}
exec(compile(ast.Module(body=[node], type_ignores=[]), 'tool-marker', 'exec'), namespace)
rows = []
for content in ['', '[memory]', ' [current_time]\n', '[_]', '[report.md]', '[foo-bar_1]', '[1foo]', '[.]', '[]', '[猫]',
                '[memory] done', 'done [memory]', '[memory][todo]', '[memory\n]', '\u001c[memory]\u001f', '\u200b[memory]', '[a/b]']:
    marker = bool(namespace['_STALE_MARKER_RE'].fullmatch(content.strip()))
    rows.append({'content': content, 'marker': marker, 'expected': '' if marker else content})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text() == text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_tool_marker_goldens.py [--check]')
print(f'Verified {len(rows)} tool-marker cases')
