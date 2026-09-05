#!/usr/bin/env python3
"""Execute auth-store loading and per-provider root fallback from the source."""
import ast
import base64
import itertools
import json
import logging
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict, Optional
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[2]
tree = ast.parse((ROOT / "hermes_cli/auth.py").read_text())
names = {"_load_auth_store", "_migrate_stale_nous_portal_url", "read_credential_pool"}
nodes = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in names]
logger = logging.getLogger("auth-fixtures")
logger.disabled = True
ns = dict(Path=Path, Optional=Optional, Dict=Dict, Any=Any, json=json, logger=logger,
          AUTH_STORE_VERSION=1, urlparse=urlparse, _NOUS_STALE_PORTAL_HOSTS={"api.nousresearch.com"},
          DEFAULT_NOUS_PORTAL_URL="https://portal.nousresearch.com")
exec(compile(ast.Module(body=nodes, type_ignores=[]), "auth-read", "exec"), ns)
load = ns["_load_auth_store"]
values = [None, [], {}, {"other":[]}, {"openai-api":[]}, {"openai-api":"invalid"},
          {"openai-api":[{"access_token":"fixture-key"}]}, {"openai-api":[None], "other":[1]},
          {"custom:openai-api":[{"access_token":"custom-fixture"}]}]
rows = []
for profile, root, provider in itertools.product(values, values, [None, "openai-api", "custom:openai-api", "absent"]):
    profile_store = {"credential_pool":profile}
    root_store = {"credential_pool":root}
    ns["_load_auth_store"] = lambda: profile_store
    ns["_load_global_auth_store"] = lambda: root_store
    result = ns["read_credential_pool"](provider)
    rows.append(dict(profile=profile_store, root=root_store, provider=provider, result=result))

store_rows = []
raw_values = [None, [], 1, "invalid", {}, {"providers":{}}, {"credential_pool":{}},
              {"providers":None,"credential_pool":{}}, {"providers":[],"systems":{"nous_portal":{}}},
              {"systems":{}}, {"systems":{"nous_portal":None}},
              {"systems":{"nous_portal":{"portal_base_url":"https://api.nousresearch.com"}}}]
for portal in [None, "", " https://API.NOUSRESEARCH.COM/path ", "https://portal.nousresearch.com", "http://example.com", False, 7]:
    raw_values.append({"version":9,"providers":{"nous":{"portal_base_url":portal}},"extra":"retained"})
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "auth.json"
    for raw in raw_values:
        for bom in [False, True]:
            data = (b"\xef\xbb\xbf" if bom else b"") + json.dumps(raw).encode()
            path.write_bytes(data)
            try:
                result = load(path)
                error = False
            except Exception:
                result, error = None, True
            store_rows.append(dict(bytes=base64.b64encode(data).decode(), result=result, error=error))

for name, output in [("auth-pool-read-goldens.json", rows), ("auth-store-read-goldens.json", store_rows)]:
    path = ROOT / "rust/tools" / name
    text = json.dumps(output, indent=2) + "\n"
    if sys.argv[1:] == ["--check"]:
        assert path.read_text() == text
    elif not sys.argv[1:]:
        path.write_text(text)
    else:
        raise SystemExit("usage: gen_auth_pool_read_goldens.py [--check]")
    print(f"Verified {len(output)} cases in {name}")
