#!/usr/bin/env python3
"""Execute Python reasoning resolution and the real Upstage hook as port oracles."""
import ast
from dataclasses import asdict
import importlib.util
import json
from pathlib import Path
import sys
from types import ModuleType

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / 'rust/tools/upstage-goldens.json'
PROFILE = REPO / 'rust/tools/upstage-profile.json'

def load(name, path):
    spec = importlib.util.spec_from_file_location(name, REPO / path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module

base = load('providers.base', 'providers/base.py')
providers = ModuleType('providers')
providers.register_provider = lambda profile: None
sys.modules['providers'] = providers
reasoning = load('agent.reasoning_effort', 'agent/reasoning_effort.py')
upstage = load('upstage_reference', 'plugins/model-providers/upstage/__init__.py')
assert set(name for name, value in vars(upstage.UpstageProfile).items() if callable(value)) == {'build_api_kwargs_extras'}, 'New Upstage hooks require a native port'
constants = ast.parse((REPO / 'hermes_constants.py').read_text())
names = {'parse_reasoning_effort', '_canonical_model_variants', 'resolve_per_model_reasoning_effort', 'resolve_reasoning_config'}
nodes = [node for node in constants.body if isinstance(node, ast.FunctionDef) and node.name in names or isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'VALID_REASONING_EFFORTS' for t in node.targets)]
ns = {'__name__': 'reasoning_reference'}
exec(compile(ast.Module(body=nodes, type_ignores=[]), 'hermes_constants.py', 'exec'), ns)

rows = []
configs = [None, {}, False, [], 'high', {'enabled': False}, {'enabled': 0}, {'effort': None}, {'effort': 0}, {'effort': True}, {'effort': ['high']}]
configs += [{'effort': e} for e in ['', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra', ' HIGH ', '\x1chigh\x1f', 'custom']]
for model in [None, '', 'solar-pro3', 'SOLAR-MINI-250127', 'vendor/syn-pro-2026', 'future-solar']:
    for cfg in configs:
        try:
            result = upstage.upstage.build_api_kwargs_extras(reasoning_config=cfg, model=model)
            rows.append(dict(model=model, config=cfg, result=result))
        except Exception as e:
            rows.append(dict(model=model, config=cfg, error=type(e).__name__))
clamps = []
for effort in [None, '', ' HIGH ', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra', '\x1chigh\x1f', 'bespoke']:
    for supported in [None, [], ['none'], ['custom'], ['LOW', 'HIGH'], ['none', 'high', 'max'], ['low', 'medium', 'high'], ['\x1clow', 'high\x1f']]:
        for overrides in [None, {'xhigh': 'max', 'medium': 'high', 'custom': 'low'}]:
            clamps.append(dict(effort=effort, supported=supported, overrides=overrides, result=reasoning.clamp_effort(effort, supported, overrides)))
resolutions = []
for model in ['', 'solar-pro3', 'claude-opus.4.5', 'openrouter/anthropic/claude-opus-4-5', 'a1-2-3', 'a١-٢-٣']:
    for effort in [None, False, True, 'none', 'high', 'custom', 'ultra']:
        for overrides in [{}, {'solar-pro3': False}, {'claude-opus-4.5': 'max'}, {'anthropic/claude-opus-4.5': 'low'}, {'a1.2-3': 'minimal'}]:
            cfg = {'model': {'default': 'solar-pro3'}, 'agent': {'reasoning_effort': effort, 'reasoning_overrides': overrides}}
            resolutions.append(dict(model=model, config=cfg, result=ns['resolve_reasoning_config'](cfg, model)))
for cfg in [None, [], False, {'agent': []}, {'model': ' solar-pro3 ', 'agent': {'reasoning_effort': '\x1chigh\x1f'}}]:
    resolutions.append(dict(model='', config=cfg, result=ns['resolve_reasoning_config'](cfg, '')))
profile = asdict(upstage.upstage)
profile['fixed_temperature'] = {'kind': 'inherit'}
outputs = {OUT: dict(hooks=rows, clamps=clamps, resolutions=resolutions), PROFILE: profile}
for path, value in outputs.items():
    text = json.dumps(value, indent=2) + '\n'
    if sys.argv[1:] == ['--check']:
        assert path.read_text() == text, f'{path} differs from Python'
    elif not sys.argv[1:]: path.write_text(text)
    else: raise SystemExit('usage: gen_upstage_goldens.py [--check]')
print(f'Verified {len(rows)} hooks, {len(clamps)} clamps, {len(resolutions)} resolutions')
