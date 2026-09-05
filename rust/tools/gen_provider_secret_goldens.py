#!/usr/bin/env python3
"""Run the reference voice secret ladder with recorded scope/file/pool reads."""
import ast
import itertools
import json
import logging
import sys
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/provider-secret-goldens.json"
tree = ast.parse((ROOT / "tools/tool_backend_helpers.py").read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "resolve_provider_secret")
scope_module = types.ModuleType("agent.secret_scope")
pool_module = types.ModuleType("agent.credential_pool")
sys.modules[scope_module.__name__] = scope_module
sys.modules[pool_module.__name__] = pool_module
rows = []
for config, scope, env, multiplex, provider, pool, custom, pool_error in itertools.product(
    ["", " config-key "], [None, " \x1c", " scoped-key "], [None, " env-key "],
    [False, True], ["", "openai-api"], [None, " \x1c", " pool-key "],
    [None, " custom-key "], [False, True],
):
    calls = []
    scope_module.is_multiplex_active = lambda: multiplex

    def scoped(name):
        calls.append("scope")
        return (scope or "").strip()

    def env_getter(name):
        calls.append("env")
        return env

    def load_pool(name):
        calls.append(name)
        if pool_error:
            raise RuntimeError("fixture pool error")
        key = custom if name.startswith("custom:") else pool
        if key is None:
            return None
        entry = types.SimpleNamespace(runtime_api_key=key, access_token="")
        return types.SimpleNamespace(has_credentials=lambda: True, peek=lambda: entry)

    pool_module.load_pool = load_pool
    ns = {"_scoped_credential": scoped, "logger": logging.getLogger("fixture")}
    exec(compile(ast.Module(body=[node], type_ignores=[]), "provider-secret", "exec"), ns)
    result = ns["resolve_provider_secret"]("TEST_KEY", provider, config, env_getter)
    rows.append(dict(config=config, scope=scope, env=env, multiplex=multiplex,
                     provider=provider, pool=pool, custom=custom, pool_error=pool_error,
                     result=result, calls=calls))
text = json.dumps(rows, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit("usage: gen_provider_secret_goldens.py [--check]")
print(f"Verified {len(rows)} provider-secret scenarios")
