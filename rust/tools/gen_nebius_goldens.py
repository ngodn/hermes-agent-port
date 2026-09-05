#!/usr/bin/env python3
"""Execute the real Nebius profile for native registration and request fixtures."""
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
load('agent.reasoning_effort', 'agent/reasoning_effort.py')
module = load('nebius_reference', 'plugins/model-providers/nebius-token-factory/__init__.py')
assert {name for name, value in vars(module.NebiusTokenFactoryProfile).items() if callable(value)} == {'build_api_kwargs_extras'}
profile = module.nebius_token_factory
configs = [None, [], True, {}, {'enabled': False}, {'enabled': 0}, {'enabled': None}]
configs += [{'effort': effort} for effort in [None, False, True, 0, 123, [], ['high'], {'custom': True}, '', ' ', '\x1chigh\x1f', 'none', 'off', 'disabled', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra', 'custom']]
models = [None, '', 'vendor/DeepSeek-R1-fast', 'DEEPSEEK-V4', 'DeepSeek-Reasoner', 'openai/gpt-oss-120b', 'GLM-5.1', 'Kimi-K2.5', 'MiniMax-M2', 'qwen3-coder', 'vendor/deepseek-v3', 'gpt-oss/llama', 'Llama-3.3', '\x1cQwen3\x1f']
rows = []
for model in models:
    for config in configs:
        for supports in [False, True]:
            result = profile.build_api_kwargs_extras(model=model, reasoning_config=config, supports_reasoning=supports)
            rows.append(dict(model=model, config=config, supports=supports, result=result))
definition = asdict(profile)
definition['fixed_temperature'] = {'kind': 'inherit'}
for name, value in [('nebius-profile.json', definition), ('nebius-goldens.json', rows)]:
    path = ROOT / 'rust/tools' / name
    content = json.dumps(value, indent=2) + '\n'
    if sys.argv[1:] == ['--check']: assert path.read_text() == content, f'{name} differs from Python'
    elif not sys.argv[1:]: path.write_text(content)
    else: raise SystemExit('usage: gen_nebius_goldens.py [--check]')
print(f'Verified {len(rows)} Nebius hook cases and profile definition')
