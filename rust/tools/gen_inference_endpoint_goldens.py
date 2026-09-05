#!/usr/bin/env python3
"""Execute inference endpoint resolution with an explicit turn runtime.

Only the context-local runtime accessor is replaced. Provider candidates in
Python are a set, so these fixtures avoid conflicting dictionary candidates;
their precedence has no stable reference result across Python processes.
"""
import importlib.util
import itertools
import json
from pathlib import Path
import sys
import types

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/inference-endpoint-goldens.json"


def generate():
    spec = importlib.util.spec_from_file_location("endpoint_reference", REPO / "agent/image_routing.py")
    source = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(source)
    auxiliary = types.ModuleType("agent.auxiliary_client")
    original = sys.modules.get("agent.auxiliary_client")
    sys.modules["agent.auxiliary_client"] = auxiliary
    cases = []
    configs = [None, [], "bad", {}, {"model": []}, {"providers": []}]
    values = [None, False, True, 0, 7, 1e-6, "", "\u001c value \u001f", [], {}, [True, None, "a'b"]]
    for value in values:
        for field in ["base_url", "api_key"]:
            configs.extend([
                {"model": {field: value}},
                {"providers": {"live": {field: value}}},
                {"providers": {"custom:live": {field: value}}},
                {"custom_providers": [None, [], {"name": " LiVe ", field: value}]},
            ])
    configs.extend([
        {"model": {"provider": "alternate"}, "providers": {"alternate": {"base_url": "alternate-url", "api_key": "alternate-key"}}},
        {"model": {"base_url": "model-url", "api_key": "model-key"}, "providers": {"live": {"base_url": "provider-url", "api_key": "provider-key"}}},
        {"custom_providers": [{"name": "LIVE", "base_url": "first", "api_key": "first"}, {"name": "live", "base_url": "second", "api_key": "second"}]},
    ])
    runtimes = [{}, {"provider": " LIVE ", "base_url": " runtime-url ", "api_key": " runtime-key "},
                {"provider": "other", "base_url": "other-url", "api_key": "other-key"},
                {"provider": "live", "base_url": "\u001c\u001f", "api_key": "\u001c\u001f"}]
    try:
        for cfg, provider, runtime in itertools.product(configs, ["live", "custom:live", ""], runtimes):
            auxiliary._runtime_main_value = runtime.get
            cases.append(dict(cfg=cfg, provider=provider, runtime=runtime,
                              base_url=source._resolve_inference_base_url(cfg, provider),
                              api_key=source._resolve_inference_api_key(cfg, provider)))
    finally:
        if original is None:
            del sys.modules["agent.auxiliary_client"]
        else:
            sys.modules["agent.auxiliary_client"] = original
    return json.dumps(cases, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Inference endpoint fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_inference_endpoint_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print(f"Verified {len(json.loads(content))} inference endpoint cases")
