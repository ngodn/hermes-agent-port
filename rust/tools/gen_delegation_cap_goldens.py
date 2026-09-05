#!/usr/bin/env python3
"""Execute the Python delegation limit resolver and batch cap."""
import ast
import json
import logging
import sys
import types
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/delegation-cap-goldens.json'
namespace = {'logger': logging.getLogger('oracle'), '_DEFAULT_MAX_CONCURRENT_CHILDREN':10, '_HIGH_CONCURRENCY_WARNED':False}
logging.disable(logging.CRITICAL)
tree = ast.parse((ROOT / 'tools/delegate_tool.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_get_max_concurrent_children')
exec(compile(ast.Module(body=[node], type_ignores=[]), 'delegate-config', 'exec'), namespace)
pkg = types.ModuleType('tools'); pkg.__path__ = []
module = types.ModuleType('tools.delegate_tool'); module._get_max_concurrent_children = namespace['_get_max_concurrent_children']
sys.modules['tools'] = pkg; sys.modules['tools.delegate_tool'] = module
tree = ast.parse((ROOT / 'run_agent.py').read_text())
cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == 'AIAgent')
node = next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == '_cap_delegate_task_calls'); node.decorator_list = []
exec(compile(ast.Module(body=[node], type_ignores=[]), 'delegate-cap', 'exec'), namespace)
rows = []
names = ['delegate_task', 'other', 'delegate_task', 'delegate_task', 'other']
for raw in [None, 0, -2, 1, 2, 20, 2.9, True, False, '2', 'bad', {}, []]:
    for env in [None, '3', 'bad']:
        namespace['_load_config'] = lambda: {'max_concurrent_children': raw}
        namespace['os'] = SimpleNamespace(getenv=lambda key: env)
        calls = [SimpleNamespace(id=str(i), function=SimpleNamespace(name=name)) for i,name in enumerate(names)]
        limit = namespace['_get_max_concurrent_children']()
        capped = namespace['_cap_delegate_task_calls'](calls)
        rows.append({'config':{'delegation':{'max_concurrent_children':raw}}, 'env':env, 'limit':limit, 'names':names, 'ids':[call.id for call in capped]})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text() == text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_delegation_cap_goldens.py [--check]')
print(f'Verified {len(rows)} delegation cap cases')
