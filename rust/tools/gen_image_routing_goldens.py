#!/usr/bin/env python3
"""Run real image-routing configuration logic with a recorded capability lookup.

The lookup itself is deliberately controlled. These cases verify configuration
precedence and whether the routing function consults capabilities, not providers.
"""
import importlib.util
import itertools
import json
from pathlib import Path
import sys

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/image-routing-goldens.json"


def source_module():
    spec = importlib.util.spec_from_file_location("image_routing_reference", REPO / "agent/image_routing.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def generate():
    source = source_module()
    raw_values = [None, False, True, 0, 1, 2, -1, 0.0, 1.0, "", "true", "FALSE", " on ",
                  "yes", "no", "off", "1", "0", "bad", "\u001cTRUE\u001f", [], {}, [1], {"k": 1}]
    coercion = [dict(value=value, expected=source._coerce_capability_bool(value)) for value in raw_values]
    configs = [None, [], "string", {}, {"model": "model"}, {"providers": []}, {"model": {"provider": 1}}]
    for raw in raw_values:
        configs += [
            {"model": {"supports_vision": raw}, "providers": {"custom": {"models": {"m": {"vision": True}}}}},
            {"providers": {"live": {"models": {"m": {"supports_vision": raw, "vision": True}}}}},
            {"custom_providers": [{"name": " LIVE ", "models": {"m": {"vision": raw}}}]},
        ]
    configs += [
        {"model": {"provider": 1e-6}, "providers": {str(1e-6): {"models": {"m": {"vision": True}}}}},
        {"model": {"provider": [1e20]}, "providers": {str([1e20]): {"models": {"m": {"vision": False}}}}},
        {"model": {"provider": [True, None, "a'b"]}, "providers": {str([True, None, "a'b"]): {"models": {"m": {"vision": True}}}}},
        {"model": {"provider": {"x": True}}, "providers": {str({"x": True}): {"models": {"m": {"vision": False}}}}},
        {"model": {"provider": "default"}, "custom_providers": [
            {"name": "default", "models": {"m": {"vision": False}}},
            {"name": "live", "models": {"m": {"vision": True}}}]},
        {"providers": {"custom:live": {"models": {"m": {"vision": False}}},
                       "live": {"models": {"m": {"vision": True}}}}},
        {"model": {"provider": "\u001clive\u001f"}, "providers": {"live": {"models": {"m": {"vision": True}}}}},
        {"providers": {"live": {"models": {"m": None}}}, "custom_providers": [None, [], {"name": "live", "models": {"m": {"vision": False}}}]},
    ]
    overrides = []
    for cfg, requested in itertools.product(configs, ["", "custom:live"]):
        overrides.append(dict(cfg=cfg, provider="custom", model="m", requested=requested,
                              expected=source._supports_vision_override(cfg, "custom", "m", requested_provider=requested)))
    aux = [dict(cfg={"auxiliary": value}, expected=source._explicit_aux_vision_override({"auxiliary": value}))
           for value in [None, [], "bad", {}, {"vision": []}, {"vision": "bad"}]]
    for key, raw in itertools.product(["provider", "model", "base_url"], raw_values + ["auto", " AUTO ", "\u001cauto\u001f"]):
        cfg = {"auxiliary": {"vision": {key: raw}}}
        aux.append(dict(cfg=cfg, expected=source._explicit_aux_vision_override(cfg)))
    modes = [dict(value=raw, expected=source._coerce_mode(raw)) for raw in raw_values + ["native", " TEXT ", "\u001cNATIVE\u001f"]]
    decisions = []
    for mode, explicit, capability, requested in itertools.product(
        ["auto", "native", "text", "invalid", "\u001cNATIVE\u001f"], [False, True], [None, False, True, "error"], ["", "custom:live"]
    ):
        cfg = {"agent": {"image_input_mode": mode}, "auxiliary": {"vision": {"provider": "custom" if explicit else "auto"}}}
        calls = []
        def lookup(provider, model, cfg, *, requested_provider=""):
            calls.append([provider, model, requested_provider])
            if capability == "error":
                raise RuntimeError("lookup failed")
            return capability
        source._lookup_supports_vision = lookup
        try:
            output = source.decide_image_input_mode("custom", "m", cfg, requested_provider=requested)
        except RuntimeError:
            output = "error"
        decisions.append(dict(cfg=cfg, capability=capability, requested=requested, expected=output, calls=calls))
    return json.dumps(dict(coercion=coercion, overrides=overrides, aux=aux, modes=modes, decisions=decisions), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Image routing fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_image_routing_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified image routing cases:", {key: len(value) for key, value in json.loads(content).items()})
