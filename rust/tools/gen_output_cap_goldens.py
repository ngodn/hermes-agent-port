#!/usr/bin/env python3
"""Execute Python output-cap routing and gateway/init resolution blocks."""
import ast
import contextlib
import io
import json
from pathlib import Path
import sys
import textwrap
from types import SimpleNamespace, MethodType
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[2]
ns = {'urlparse': urlparse}
utils = ast.parse((ROOT / 'utils.py').read_text())
names = {'base_url_hostname', 'base_url_host_matches', 'model_forces_max_completion_tokens'}
exec(compile(ast.Module(body=[n for n in utils.body if isinstance(n, ast.FunctionDef) and n.name in names], type_ignores=[]), 'utils.py', 'exec'), ns)
agent = next(n for n in ast.parse((ROOT / 'run_agent.py').read_text()).body if isinstance(n, ast.ClassDef) and n.name == 'AIAgent')
methods = {'_is_direct_openai_url', '_is_azure_openai_url', '_is_github_copilot_url', '_max_tokens_param'}
exec(compile(ast.Module(body=[n for n in agent.body if isinstance(n, ast.FunctionDef) and n.name in methods], type_ignores=[]), 'run_agent.py', 'exec'), ns)
source = (ROOT / 'gateway/run.py').read_text()
start = source.index('    model_cfg = _get_model_config()\n    max_tokens = None')
end = source.index('    capabilities = runtime.get("capabilities")', start)
gateway_block = compile(textwrap.dedent(source[start:end]), 'gateway-output-cap', 'exec')
source = (ROOT / 'agent/agent_init.py').read_text()
start = source.index('    _model_cfg = _agent_cfg.get("model", {})\n    if agent.max_tokens is None')
end = source.index('    agent._session_init_model_config["max_tokens"]', start)
init_block = compile(textwrap.dedent(source[start:end]), 'init-output-cap', 'exec')
rows = []
for model in ['', 'gpt-4', 'gpt-4o-mini', 'gpt-4.1-test', 'vendor/gpt-5.4', 'vendor/o1', 'o3-mini', 'o4x', 'prefix/gpt-5/not-openai', 'llama', '\x1co3\x1f']:
    for url in ['https://api.openai.com/v1', 'api.openai.com/v1', 'https://API.OPENAI.COM./v1', 'https://api.openai.com.evil/v1', 'https://proxy.test/api.openai.com', 'https://resource.openai.azure.com/openai/v1', 'https://openai.azure.com/v1', 'https://api.githubcopilot.com', 'https://other.githubcopilot.com/v1', 'https://githubcopilot.com/v1', 'http://localhost:8080/v1', 'https://openrouter.ai/api/v1']:
        stub = SimpleNamespace(model=model, _base_url_lower=url.lower())
        for name in methods: setattr(stub, name, MethodType(ns[name], stub))
        rows.append(dict(model=model, url=url, result=stub._max_tokens_param(42)))
resolutions = []
for raw in [None, False, True, -1, 0, 42, 12.8, '123', 'bad', '', ' ١_٢ ', [], {}]:
    for env in [None, '', '99', '0', '-3', 'invalid', '1_024']:
        for fallback in [None, 256, 0, False]:
            scope = {'os': SimpleNamespace(environ={} if env is None else {'HERMES_MAX_TOKENS': env}), '_get_model_config': lambda: {'max_tokens': raw}, 'runtime': {'max_output_tokens': fallback}}
            exec(gateway_block, scope)
            stub = SimpleNamespace(max_tokens=scope['max_tokens'])
            scope = {'agent': stub, '_agent_cfg': {'model': {'max_tokens': raw}}, '_ra': lambda: SimpleNamespace(logger=SimpleNamespace(warning=lambda *args: None)), 'sys': sys}
            with contextlib.redirect_stderr(io.StringIO()): exec(init_block, scope)
            resolutions.append(dict(raw=raw, env=env, fallback=fallback, result=stub.max_tokens))
content = json.dumps(dict(parameters=rows, resolutions=resolutions), indent=2) + '\n'
path = ROOT / 'rust/tools/output-cap-goldens.json'
if sys.argv[1:] == ['--check']: assert path.read_text() == content
elif not sys.argv[1:]: path.write_text(content)
else: raise SystemExit('usage: gen_output_cap_goldens.py [--check]')
print(f'Verified {len(rows)} parameter and {len(resolutions)} cap-resolution cases')
