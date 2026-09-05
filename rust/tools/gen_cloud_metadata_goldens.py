#!/usr/bin/env python3
"""Execute Python capability lookup and override selection on controlled registries."""
import ast
from copy import deepcopy
from dataclasses import dataclass, asdict
import itertools
import json
import logging
from pathlib import Path
import sys
from gen_managed_capability_goldens import extracted, REPO

OUT = REPO / "rust/tools/cloud-metadata-goldens.json"
MAPPING = REPO / "rust/tools/models-dev-provider-map.json"


def generate():
    tree = ast.parse((REPO / "agent/models_dev.py").read_text())
    mapping = next(ast.literal_eval(n.value) for n in tree.body if isinstance(n, ast.AnnAssign)
                   and isinstance(n.target, ast.Name) and n.target.id == "PROVIDER_TO_MODELS_DEV")
    scope = dict(dataclass=dataclass, logger=logging.getLogger("oracle"),
                 PROVIDER_TO_MODELS_DEV=mapping, _MODELS_DEV_TO_PROVIDER=None, _OVERRIDE_WARNED_KEYS=set())
    extracted("agent/models_dev.py", {"ModelCapabilities", "_models_dev_to_hermes_ids", "_provider_override_section",
              "_explicit_model_override", "_default_model_override", "_override_for", "_override_int",
              "_get_provider_models", "_find_model_entry", "get_model_capabilities", "_override_context_window",
              "_default_override_context", "_extract_context", "lookup_models_dev_context"}, scope)
    entries = [{}, {"attachment": True}, {"attachment": True, "modalities": {"input": []}},
               {"attachment": False, "modalities": {"input": ["image"]}},
               {"attachment": True, "modalities": {"input": "image"}},
               {"attachment": "false", "modalities": []}]
    for field, values in {
        "limit": [None, [], {"context": True, "output": 0.5}, {"context": "100", "output": -5},
                  {"context": 123.8, "output": 456}],
        "family": [None, False, 7, ["family"]],
        "tool_call": [False, True, "false", [], [0]],
        "reasoning": [False, True, "false", None],
    }.items():
        entries.extend({field: value} for value in values)
    patches = [None, {}, {"supports_vision": False}, {"supports_tools": "false", "supports_reasoning": []},
               {"context_window": "١_٠٢٤", "max_output_tokens": "5.5"},
               {"context_window": 0.5, "max_output_tokens": True}, {"model_family": ["x", True]}]
    scenarios = []
    for entry, patch, model in itertools.product(entries, patches, ["known", "KNOWN", "unknown"]):
        overrides = {"openai": {"_default": {"supports_vision": True, "context_window": 777}}}
        if patch is not None:
            overrides["openai"][model] = patch
        scenarios.append(dict(provider="openai", model=model,
                              registry={"openai": {"models": {"known": entry}}},
                              config={"model_overrides": overrides}))
    for hermes, mapped in mapping.items():
        for key in [hermes, mapped]:
            scenarios.append(dict(provider=hermes, model="m", registry={mapped: {"models": {"m": {"attachment": True}}}},
                                  config={"model_overrides": {key: {"m": {"supports_vision": False}}}}))
            scenarios.append(dict(provider=mapped, model="unknown", registry={},
                                  config={"model_overrides": {hermes: {"_default": {"supports_vision": True}}}}))
    for key in ["m:cloud", "M:CLOUD", "m-cloud", "M-CLOUD"]:
        scenarios.append(dict(provider="ollama-cloud", model="m", registry={"ollama-cloud": {"models": {key: {"attachment": False}}}},
                              config={"model_overrides": {"_default": {"supports_vision": True}}}))
    for provider in ["custom", " openai ", "OPENAI", "", "github-copilot"]:
        for config in [{}, {"model_overrides": {"_default": {}}},
                       {"model_overrides": {"_default": {"supports_vision": True}}}]:
            scenarios.append(dict(provider=provider, model="m", registry={"openai": {"models": {"m": {}}}}, config=config))
    for models in [{"model": {"attachment": True}, "MODEL": {"attachment": False}},
                   {"MODEL": {"attachment": False}, "model": {"attachment": True}}]:
        scenarios.append(dict(provider="openai", model="MoDeL", registry={"openai": {"models": models}}, config={}))
        scenarios.append(dict(provider="openai", model="MoDeL", registry={},
                              config={"model_overrides": {"openai": {key: {"supports_vision": value["attachment"]} for key, value in models.items()}}}))
    results = []
    for case in scenarios:
        scope["_load_model_overrides"] = lambda: case["config"].get("model_overrides", {})
        scope["fetch_models_dev"] = lambda **kwargs: case["registry"]
        result = scope["get_model_capabilities"](case["provider"], case["model"])
        results.append(dict(**case, expected=asdict(result) if result is not None else None,
                            context=scope["lookup_models_dev_context"](case["provider"], case["model"])))
    return json.dumps(results, indent=2) + "\n", json.dumps(list(mapping.items()), indent=2) + "\n"


if __name__ == "__main__":
    logging.disable(logging.CRITICAL)
    content, mapping = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content or MAPPING.read_text() != mapping:
            raise SystemExit("Cloud metadata fixtures or mapping differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_cloud_metadata_goldens.py [--check]")
    else:
        OUT.write_text(content)
        MAPPING.write_text(mapping)
    print("Verified", len(json.loads(content)), "cloud metadata cases")
