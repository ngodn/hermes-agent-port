#!/usr/bin/env python3
"""Execute the runner's image-routing wrapper with recorded runtime effects."""
import ast
import itertools
import json
import logging
from pathlib import Path
import sys
from types import ModuleType

from gen_image_routing_goldens import REPO, source_module

OUT = REPO / "rust/tools/session-image-routing-goldens.json"


def generate():
    image = source_module()
    sys.modules["agent.image_routing"] = image
    aux = ModuleType("agent.auxiliary_client")
    config = ModuleType("hermes_cli.config")
    sys.modules["agent.auxiliary_client"] = aux
    sys.modules["hermes_cli.config"] = config
    tree = ast.parse((REPO / "gateway/run.py").read_text())
    runner = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "GatewayRunner")
    method = next(n for n in runner.body if isinstance(n, ast.FunctionDef) and n.name == "_decide_image_input_mode")
    cls = ast.parse("class Runner:\n    pass").body[0]
    cls.body = [method]
    scope = dict(logger=logging.getLogger("session-image-oracle"))
    module = ast.Module(body=[ast.parse("from __future__ import annotations").body[0], cls], type_ignores=[])
    exec(compile(ast.fix_missing_locations(module), "gateway/run.py (image routing)", "exec"), scope)
    cases = []
    for provider, model, identity, mode in itertools.product(["", " explicit "], ["", " chosen "], [False, True], ["auto", "native"]):
        cases.append(dict(provider=provider, model=model, source=identity, session_key=None,
                          cfg={"agent": {"image_input_mode": mode}}, runtime_model=" session-m ",
                          runtime={"provider": " session-p ", "requested_provider": " custom:live "}, fault=""))
    for runtime_model, runtime in [(None, None), (42, []), ("", {"provider": None, "requested_provider": 12}),
                                  ("\u001cm\u001f", {"provider": "\u001cp\u001f"})]:
        cases.append(dict(provider="", model="", source=False, session_key="key", cfg=None,
                          runtime_model=runtime_model, runtime=runtime, fault=""))
    for fault in ["load", "resolve", "provider", "model", "lookup"]:
        cases.append(dict(provider="", model="", source=True, session_key="key", cfg=None,
                          runtime_model="", runtime={}, fault=fault))
    for key in [None, "", "key"]:
        cases.append(dict(provider="", model="", source=False, session_key=key, cfg=[],
                          runtime_model="session-m", runtime={"provider": "session-p"}, fault=""))
    for case in cases:
        calls = []
        def effect(name, result):
            calls.append([name])
            if case["fault"] == name:
                raise RuntimeError(name)
            return result
        config.load_config = lambda: effect("load", {})
        aux._read_main_provider = lambda: effect("provider", "default-p")
        aux._read_main_model = lambda: effect("model", "default-m")
        def resolve(self, source, session_key, user_config):
            calls.append(["resolve", source is not None, session_key, user_config])
            if case["fault"] == "resolve":
                raise RuntimeError("resolve")
            return case["runtime_model"], case["runtime"]
        scope["Runner"]._resolve_session_agent_runtime = resolve
        def lookup(provider, model, cfg, *, requested_provider=""):
            calls.append(["lookup", provider, model, cfg, requested_provider])
            if case["fault"] == "lookup":
                raise RuntimeError("lookup")
            return True
        image._lookup_supports_vision = lookup
        output = scope["Runner"]()._decide_image_input_mode(
            source=object() if case["source"] else None, session_key=case["session_key"],
            user_config=case["cfg"], provider=case["provider"], model=case["model"])
        case["expected"] = dict(output=output, calls=calls)
    return json.dumps(cases, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Session image routing fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_session_image_routing_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)), "session image routing cases")
