#!/usr/bin/env python3
"""Execute Python's name repair, including difflib fuzzy matching."""
import ast
import json
import random
import sys
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/tool-name-repair-goldens.json'
tree = ast.parse((ROOT / 'agent/agent_runtime_helpers.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'repair_tool_call')
namespace = {}
exec(compile(ast.Module(body=[node], type_ignores=[]), 'tool-name-repair', 'exec'), namespace)
valid = ['terminal', 'execute_code', 'session_search', 'write_file', 'todo', 'browser_click', 'current_time']
names = ['', ' ', 'TERMINAL', 'write file', 'write-file', 'CurrentTime', 'CurrentTimeTool_tool', 'TodoTool_tool', 'BrowserClick_tool',
         'terminal" parameter="command" string="true', 'session_search<bad>', 'termminal', 'execute_cod', 'curent_time', 'unknown', '猫']
cases = [(name, valid) for name in names]
rng = random.Random(9318)
for _ in range(120):
    name = rng.choice(valid)
    index = rng.randrange(len(name))
    variant = rng.choice([name[:index] + name[index+1:], name[:index] + 'x' + name[index:], name[:index] + name[index].upper() + name[index+1:]])
    cases.append((variant, valid))
# Direct difflib path: cutoff boundaries, tie order and autojunk at 200 chars.
cases += [('abc', ['abd', 'abe']), ('tide', ['diet']), ('diet', ['tide']),
          ('a' * 210 + 'b', ['a' * 210 + 'c']), ('b' + 'a' * 210, ['c' + 'a' * 210]),
          ('a' * 199 + 'b', ['a' * 199 + 'c']), ('xy' * 150 + 'z', ['xy' * 150 + 'q'])]
rows = [{'name': name, 'valid': valid, 'expected': namespace['repair_tool_call'](SimpleNamespace(valid_tool_names=valid), name)} for name, valid in cases]
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_tool_name_repair_goldens.py [--check]')
print(f'Verified {len(rows)} name-repair cases')
