#!/usr/bin/env python3
"""Execute the Python registry cache state machine with real temporary cache files."""
import itertools
import json
import logging
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
from gen_managed_capability_goldens import extracted, REPO

OUT = REPO / "rust/tools/cloud-catalog-goldens.json"


def generate():
    results = []
    names = {"_validate_registry", "_load_disk_cache", "_quarantine_corrupt_cache",
             "_disk_cache_age_seconds", "_load_etag", "_clear_etag", "_NotModified",
             "_fetch_models_dev_from_network", "_mark_stale_cache_grace", "_commit_registry",
             "_confirm_cache_not_modified", "_note_refresh_failure", "_background_refresh_models_dev",
             "_start_background_refresh_models_dev", "fetch_models_dev"}
    for initial, force, online, response in itertools.product(
            ["missing", "fresh", "stale", "future", "corrupt"], [False, True], [False, True], [200, 304, 503]):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            cache, etag = root / "models_dev_cache.json", root / "models_dev_cache.etag"
            if initial != "missing":
                cache.write_text("broken" if initial == "corrupt" else '{"old":{}}')
                age = {"fresh": 60, "stale": 86400, "future": -600, "corrupt": 0}[initial]
                os.utime(cache, (time.time() - age, time.time() - age))
            etag.write_text('"cached"')
            calls, workers = [], []

            class Response:
                status_code = response
                headers = {"ETag": '"v1"'}
                def raise_for_status(self):
                    if response >= 400:
                        raise RuntimeError("HTTP failure")
                def json(self):
                    return {"provider": {"models": {"vision": {"attachment": True}}}}

            def get(url, headers, timeout):
                assert timeout == (5, 10)
                calls.append(headers.get("If-None-Match"))
                return Response()

            class Worker:
                def __init__(self, target, **kwargs):
                    self.target = target
                def start(self):
                    workers.append(self.target)

            def save(data, etag=""):
                cache.write_text(json.dumps(data))
                if etag:
                    (root / "models_dev_cache.etag").write_text(etag)

            scope = dict(json=json, time=time, logger=logging.getLogger("oracle"),
                         requests=SimpleNamespace(get=get), threading=SimpleNamespace(Thread=Worker),
                         _MODELS_DEV_CACHE_TTL=14400, _MODELS_DEV_RETRY_DELAY=300,
                         _models_dev_cache={}, _models_dev_cache_time=0,
                         _models_dev_retry_after=0, _models_dev_refresh_in_flight=False,
                         _models_dev_fetch_lock=threading.Lock(), _models_dev_refresh_lock=threading.Lock(),
                         _get_cache_path=lambda: cache, _get_etag_path=lambda: etag,
                         _get_models_dev_url=lambda: "http://fixture/api.json", _save_disk_cache=save)
            extracted("agent/models_dev.py", names, scope)
            returned = scope["fetch_models_dev"](force, allow_network=online)
            for worker in workers:
                worker()
            results.append(dict(initial=initial, force=force, online=online, response=response,
                                returned=returned, final=scope["_models_dev_cache"], requests=calls,
                                backoff=scope["_models_dev_retry_after"] > time.time(),
                                quarantined=cache.with_suffix(".json.corrupt").exists(),
                                etag=etag.read_text() if etag.exists() else None))
    return json.dumps(results, indent=2) + "\n"


if __name__ == "__main__":
    logging.disable(logging.CRITICAL)
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Cloud catalog fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_cloud_catalog_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)), "cloud cache cases")
