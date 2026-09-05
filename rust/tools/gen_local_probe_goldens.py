#!/usr/bin/env python3
"""Execute Python probe decisions with recorded HTTP responses, without I/O."""
import ast
import json
from pathlib import Path
import re
import sys
import time
import types

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/local-probe-goldens.json"


def generate():
    tree = ast.parse((REPO / "agent/model_metadata.py").read_text())
    names = {"_normalize_base_url", "_localhost_to_ipv4", "_lmstudio_server_root", "_auth_headers",
             "detect_local_server_type", "query_ollama_supports_vision"}
    module = ast.Module(body=[ast.parse("from __future__ import annotations").body[0]] +
                        [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in names], type_ignores=[])
    scope = dict(re=re, time=time, _ENDPOINT_PROBE_TTL_SECONDS=3600,
                 _ENDPOINT_PROBE_FAILURE_TTL_SECONDS=300,
                 _endpoint_blackholed=lambda _: False, _local_probe_disk_get=lambda *_: None,
                 _local_probe_disk_put=lambda *_: None, _is_connect_timeout=lambda _: False,
                 _strip_provider_prefix=lambda model: model)
    exec(compile(module, "agent/model_metadata.py (probe oracle)", "exec"), scope)
    calls = []
    responses = {}

    class Response:
        def __init__(self, response):
            self.status_code = response.get("status", 200)
            self.text = response.get("body", "")

        def json(self):
            return json.loads(self.text)

    class Client:
        def __init__(self, *, timeout, headers):
            self.headers = headers

        def __enter__(self):
            return self

        def __exit__(self, *_):
            pass

        def get(self, url):
            path = url.removeprefix("http://probe")
            calls.append(dict(method="GET", path=path, auth=self.headers.get("Authorization")))
            return Response(responses.get(path, dict(status=404)))

        def post(self, url, *, json):
            path = url.removeprefix("http://probe")
            calls.append(dict(method="POST", path=path, auth=self.headers.get("Authorization"), body=json))
            return Response(responses.get(path, dict(status=404)))

    fake = types.ModuleType("httpx")
    fake.Client = Client
    original = sys.modules.get("httpx")
    sys.modules["httpx"] = fake
    detection = []
    vision = []
    try:
        scenarios = [({}, "unknown"), ({"/api/v1/models": {"body": "not-json"}}, "lm-studio"),
                     ({"/api/v1/models": {"status": 302, "location": "/redirect-target"},
                       "/redirect-target": {"body": "redirected"}}, "redirects-not-followed")]
        for payload in [{"models": []}, ["models"], "models", {"error": "Unexpected endpoint"}, None, 1]:
            scenarios.append(({"/api/tags": {"body": json.dumps(payload)}}, "tags"))
        scenarios += [
            ({"/v1/props": {"body": "default_generation_settings"}}, "llamacpp"),
            ({"/props": {"body": "default_generation_settings"}}, "llamacpp-legacy"),
            ({"/v1/props": {"body": "other"}, "/props": {"body": "default_generation_settings"}}, "no-fallback-on-200"),
        ]
        for payload in [{"version": None}, ["version"], "version", {}, None]:
            scenarios.append(({"/version": {"body": json.dumps(payload)}}, "version"))
        for responses, label in scenarios:
            scope["_endpoint_probe_path_cache"] = {}
            calls.clear()
            result = scope["detect_local_server_type"]("http://probe/v1/", api_key=" key ")
            detection.append(dict(label=label, responses=responses, expected=result, calls=list(calls)))
        payloads = [{}, {"capabilities": ["vision"]}, {"capabilities": ["VISION"]},
                    {"capabilities": [" vision "]}, {"capabilities": [True, 1]},
                    {"capabilities": ["completion"], "model_info": {"x.vision.block_count": 1}},
                    {"capabilities": [], "model_info": {"X.VISION.BLOCK_COUNT": 0}},
                    {"capabilities": "vision"}, {"capabilities": None},
                    {"model_info": {"prefix.vision.block_count.suffix": None}},
                    {"model_info": ["vision.block_count"]}]
        for payload in payloads:
            scope["_endpoint_probe_path_cache"] = {}
            responses = {"/api/tags": {"body": '{"models": []}'}, "/api/show": {"body": json.dumps(payload)}}
            calls.clear()
            result = scope["query_ollama_supports_vision"]("model:7b", "http://probe/v1", api_key=" key ")
            vision.append(dict(responses=responses, expected=result, calls=list(calls)))
    finally:
        if original is None:
            del sys.modules["httpx"]
        else:
            sys.modules["httpx"] = original
    normalization = []
    for url in ["", "  http://localhost:123/v1///  ", "\u001chttp://localhost/v1\u001f",
                "http://localhost", "http://localhost?x=1", "HTTP://localhost/v1",
                "http://LOCALHOST/v1", "http://localhost.evil/v1", "https://localhost/v1",
                "http://remote/path?upstream=http://localhost/v1", "localhost:123/v1",
                "http://host/api/v1/", "http://host/api/", "http://host/nested/api/v1"]:
        normalized = scope["_localhost_to_ipv4"](scope["_normalize_base_url"](url))
        server = normalized[:-3] if normalized.endswith("/v1") else normalized
        normalization.append(dict(input=url, server=server, lmstudio=scope["_lmstudio_server_root"](normalized)))
    return json.dumps(dict(detection=detection, vision=vision, normalization=normalization), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Local probe fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_local_probe_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified local probe cases:", {k: len(v) for k, v in json.loads(content).items()})
