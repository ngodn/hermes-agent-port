#!/usr/bin/env python3
"""Execute the real base-profile model-list hook with a controlled urllib boundary."""
import importlib.util
import itertools
import json
from pathlib import Path
import sys
from types import SimpleNamespace
from gen_managed_capability_goldens import REPO

OUT = REPO / "rust/tools/provider-fetch-goldens.json"


def generate():
    spec = importlib.util.spec_from_file_location("provider_fetch_oracle", REPO / "providers/base.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    module._profile_user_agent = lambda: "hermes-cli/fixture"
    target = "hermes_cli.urllib_security"
    previous = sys.modules.get(target)
    cases = []
    try:
        for base, catalog, caller in itertools.product(
            ["", "https://inference.example/v1", "https://inference.example/v1/"],
            ["", " https://catalog.example/models "],
            [None, "", "  ", "https://inference.example/v1", "https://inference.example/v1/", " https://proxy.example/v1/ "]):
            cases.append(dict(base=base, catalog=catalog, caller=caller, body={"data": [{"id": "m"}]}, key="secret", headers={}))
        for body in [[], [{"id": "a"}, {"id": "a"}, {"id": None}, {"id": 4}, {"other": 1}, "skip"], {},
                     {"data": []}, {"data": None}, {"data": {}}, {"data": "abc"}, {"data": False},
                     {"data": [{"id": ["a"]}]}, None, False, "bad"]:
            cases.append(dict(base="https://inference.example/v1", catalog="", caller=None, body=body, key="", headers={}))
        cases.append(dict(base="https://inference.example/v1", catalog="", caller=None, body=[], key="key",
                          headers={"Authorization": "Custom override", "Accept": "custom/type", "User-Agent": "custom-agent", "X-Private": "private-value"}))
        results = []
        for case in cases:
            calls = []
            class Response:
                def __enter__(self): return self
                def __exit__(self, *args): pass
                def read(self): return json.dumps(case["body"]).encode()
            def open_url(request, *, timeout):
                calls.append(dict(url=request.full_url, headers={k.lower(): v for k, v in request.header_items()}, timeout=timeout))
                return Response()
            sys.modules[target] = SimpleNamespace(open_credentialed_url=open_url)
            profile = module.ProviderProfile(name="fixture", base_url=case["base"], models_url=case["catalog"], default_headers=case["headers"])
            result = profile.fetch_models(api_key=case["key"], base_url=case["caller"], timeout=2)
            results.append(dict(**case, expected=result, calls=calls))
        hostnames = []
        for explicit, base in itertools.product(["", " Explicit.Host. "], ["", "https://EXAMPLE.COM:443/v1", "http://127.1/", "//Host.Name./m", "http://user:pass@[::1]:80/v1", "relative/path"]):
            profile = module.ProviderProfile(name="fixture", base_url=base, hostname=explicit)
            hostnames.append(dict(explicit=explicit, base=base, expected=profile.get_hostname()))
    finally:
        sys.modules.pop(spec.name, None)
        if previous is None: sys.modules.pop(target, None)
        else: sys.modules[target] = previous
    return json.dumps(dict(fetch=results, hostnames=hostnames), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content: raise SystemExit("Provider fetch fixtures differ from Python")
    elif sys.argv[1:]: raise SystemExit("Usage: gen_provider_fetch_goldens.py [--check]")
    else: OUT.write_text(content)
    data = json.loads(content)
    print("Verified", len(data["fetch"]), "fetch and", len(data["hostnames"]), "hostname cases")
