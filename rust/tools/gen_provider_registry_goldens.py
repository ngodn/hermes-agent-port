#!/usr/bin/env python3
"""Execute Python registration, identity mutation, and model-prefix recognition."""
import itertools
import json
import re
from types import SimpleNamespace
from pathlib import Path
import sys
from gen_managed_capability_goldens import extracted, REPO

OUT = REPO / "rust/tools/provider-registry-goldens.json"


def generate():
    scope = dict(_REGISTRY={}, _ALIASES={}, _PROVIDER_LIST_CACHE=None, _discovered=True)
    extracted("providers/__init__.py", {"register_provider", "get_provider_profile", "list_providers"}, scope)
    module = SimpleNamespace(get_provider_profile=scope["get_provider_profile"])
    previous = sys.modules.get("providers")
    sys.modules["providers"] = module
    strip = dict(_OLLAMA_TAG_PATTERN=re.compile(r"^(\d+\.?\d*b|latest|stable|q\d|fp?\d|instruct|chat|coder|vision|text)", re.IGNORECASE))
    extracted("agent/model_metadata.py", {"_strip_provider_prefix"}, strip)
    profiles, trace = {}, []
    steps = [dict(key="a", name="first", aliases=["alias", "second"]),
             dict(key="b", name="second", aliases=[]),
             dict(key="c", name="first", aliases=["new"]),
             dict(key="d", name="third", aliases=["alias"]),
             dict(key="c", name="renamed", aliases=["latest-alias"]),
             dict(key="e", name=" First ", aliases=["Mixed"])]
    queries = ["first", "second", "third", "alias", "new", "renamed", "latest-alias", " First ", "Mixed", "mixed", "missing"]
    try:
        for step in steps:
            profile = profiles.setdefault(step["key"], SimpleNamespace(marker=step["key"]))
            profile.name, profile.aliases = step["name"], tuple(step["aliases"])
            scope["register_provider"](profile)
            trace.append(dict(step=step, get={name: getattr(scope["get_provider_profile"](name), "marker", None) for name in queries},
                              listed=[p.marker for p in scope["list_providers"]()]))
        # A caller mutating the returned list must not corrupt the cached list.
        scope["list_providers"]().clear()
        final_list = [p.marker for p in scope["list_providers"]()]
        scope["register_provider"](SimpleNamespace(name="local", aliases=("custom", "http", "deepseek", "qwen")))
        cases = []
        for prefix, suffix in itertools.product(
            ["local", " LOCAL ", "custom", "unknown", "http", "HTTP", "qwen", "deepseek", " First ", "Mixed"],
            ["model", "", " m:tag ", "7b", "0.5b", "7B-extra", "7", "latest", "stable-beta", "q4_0",
             "fp16", "f8", "instructive", "chatty", "coder-x", "visionary", "textual", "\u0130NSTRUCT", "\u0131nstruct",
             "vi\u017fion", "\U0001e5f1b", "q\U0001e5f1", "١٢b", "q４", " \u001c7b\u001f ", "\nmodel"]):
            model = prefix + ":" + suffix
            cases.append(dict(model=model, expected=strip["_strip_provider_prefix"](model)))
        cases.extend(dict(model=model, expected=strip["_strip_provider_prefix"](model)) for model in ["model", "httpfoo:bar", "http://localhost/m", "https://host", ":model"])
    finally:
        if previous is None:
            sys.modules.pop("providers", None)
        else:
            sys.modules["providers"] = previous
    return json.dumps(dict(trace=trace, final_list=final_list, prefixes=cases), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Provider registry fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_provider_registry_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)["prefixes"]), "prefix cases and registration transitions")
