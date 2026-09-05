#!/usr/bin/env python3
"""Execute the chat transport's pre-hook reasoning normalization."""
import ast
import importlib.util
import json
from pathlib import Path
import sys
ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('reasoning_reference', ROOT / 'agent/reasoning_effort.py')
reasoning = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reasoning)
ns = {'clamp_effort': reasoning.clamp_effort, 'OPENAI_COMPAT_WIRE_EFFORTS': reasoning.OPENAI_COMPAT_WIRE_EFFORTS}
tree = ast.parse((ROOT / 'agent/transports/chat_completions.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_reasoning_config_for_model')
exec(compile(ast.Module(body=[node], type_ignores=[]), 'chat_completions.py', 'exec'), ns)
configs = [None, [], False, 'ultra', {}, {'effort': ' HIGH '}, {'effort': '\x1cultra\x1f'}]
configs += [{'enabled': enabled, 'effort': effort, 'other': {'keep': True}} for enabled in [True, False, 0, None] for effort in ['ultra', 'ULTRA', 'max', 'xhigh', 'none', 'custom', 4, False, None, [], ['ultra']]]
rows = [dict(config=cfg, result=ns['_reasoning_config_for_model']('fixture', cfg)) for cfg in configs]
text = json.dumps(rows, indent=2) + '\n'
path = ROOT / 'rust/tools/wire-reasoning-goldens.json'
if sys.argv[1:] == ['--check']: assert path.read_text() == text
elif not sys.argv[1:]: path.write_text(text)
else: raise SystemExit('usage: gen_wire_reasoning_goldens.py [--check]')
print(f'Verified {len(rows)} pre-hook wire normalization cases')
