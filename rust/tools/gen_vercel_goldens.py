#!/usr/bin/env python3
"""Execute the real Vercel profile and retain both request-hook maps."""
from dataclasses import asdict
import importlib.util
import json
from pathlib import Path
import sys
from types import ModuleType
ROOT = Path(__file__).resolve().parents[2]
def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module
load('providers.base', 'providers/base.py')
providers = ModuleType('providers')
providers.register_provider = lambda profile: None
sys.modules['providers'] = providers
module = load('vercel_reference', 'plugins/model-providers/ai-gateway/__init__.py')
assert {n for n, v in vars(module.VercelAIGatewayProfile).items() if callable(v)} == {'build_api_kwargs_extras'}
profile = asdict(module.vercel)
profile['fixed_temperature'] = {'kind': 'inherit'}
rows = []
for config in [None, {}, [], False, True, '', 'high', 0, 3, ['ab'], [['enabled', False], ['effort', 'high']], [['x', 1], ['x', 2]], {'enabled': False}, {'enabled': 0}, {'effort': 'ultra'}, {'custom': {'nested': True}}]:
    for supports in [False, True]:
        row = dict(config=config, supports=supports)
        try: row['result'] = module.vercel.build_api_kwargs_extras(reasoning_config=config, supports_reasoning=supports)
        except Exception as error: row['error'] = type(error).__name__
        rows.append(row)
for name, value in [('vercel-profile.json', profile), ('vercel-goldens.json', rows)]:
    path = ROOT / 'rust/tools' / name
    text = json.dumps(value, indent=2) + '\n'
    if sys.argv[1:] == ['--check']: assert path.read_text() == text
    elif not sys.argv[1:]: path.write_text(text)
    else: raise SystemExit('usage: gen_vercel_goldens.py [--check]')
print(f'Verified {len(rows)} Vercel request-hook cases')
