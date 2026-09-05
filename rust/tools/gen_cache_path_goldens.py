#!/usr/bin/env python3
"""Generate cache-path fixtures using real Python imports and isolated homes.

Use the shared Hermes venv (scripts/run_tests.sh's documented fallback), or an
interpreter with the project dependencies installed. No credentials are used.
"""
import json
import os
from pathlib import Path
import sys
import tempfile

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/cache-path-goldens.json"
sys.path.insert(0, str(REPO))


def generate():
    cases = []
    for layout in ["fresh", "empty-legacy", "populated-legacy", "file-legacy"]:
        with tempfile.TemporaryDirectory(prefix="cache-parity-", dir=REPO / "rust/tools") as folder:
            home = Path(folder).resolve()
            os.environ["HERMES_HOME"] = str(home)
            from hermes_constants import set_hermes_home_override, reset_hermes_home_override
            token = set_hermes_home_override(home)
            try:
                from tools.credential_files import get_cache_directory_mounts, to_agent_visible_cache_path, from_agent_visible_cache_path
                old = home / "image_cache"
                if layout in {"empty-legacy", "populated-legacy"}:
                    old.mkdir()
                if layout == "populated-legacy":
                    (old / "existing.png").write_bytes(b"image")
                if layout == "file-legacy":
                    old.write_bytes(b"not a directory")
                def normalized(value):
                    return str(value).replace(str(home), "__HOME__")
                mounts = [dict(host_path=normalized(m["host_path"]), container_path=m["container_path"])
                          for m in get_cache_directory_mounts("/remote/")]
                checks = []
                for backend in ["docker", " Docker ", "modal", "ssh", "daytona", "vercel_sandbox", "local", "singularity"]:
                    os.environ["TERMINAL_ENV"] = backend
                    for relative in ["cache/images/a.png", "image_cache/existing.png", "attachments", "attachments/a.zip", "images/photo.png", "images-other/a", "cache/images/../outside"]:
                        host = str(home / relative)
                        mapped = to_agent_visible_cache_path(host, "/remote/")
                        inverse = from_agent_visible_cache_path(mapped, "/remote/")
                        checks.append(dict(backend=backend, host=normalized(host), mapped=normalized(mapped), inverse=normalized(inverse)))
                cases.append(dict(layout=layout, mounts=mounts, checks=checks))
            finally:
                reset_hermes_home_override(token)
    return json.dumps(cases, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Cache path fixtures differ from real Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_cache_path_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", sum(len(case["checks"]) for case in json.loads(content)), "cache path mappings via real Python imports")
