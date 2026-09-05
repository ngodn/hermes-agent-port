#!/usr/bin/env python3
"""Exercise the source read guard on real paths, missing tails and symlinks."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/file-read-safety-goldens.json"


def load_source():
    spec = importlib.util.spec_from_file_location("file_safety_reference", REPO / "agent/file_safety.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def generate():
    source = load_source()
    directories = ["user/.hermes/profiles/work", "project", "outside", "user/.hermes/skills/.hub",
                   "user/.hermes/profiles/work/mcp-tokens", "outside/deep"]
    links = {"project/credential.png": "../user/.hermes/auth.json",
             "project/profile-link": "../user/.hermes/profiles/work",
             "project/out": "../outside/deep", "project/loop": "loop",
             "project/missing-link": "../missing/tail",
             "user/.hermes/mcp-tokens": "../../outside",
             "user/.hermes/profiles/work/skills": "../../../../outside"}
    paths = ["", ".", "~hermes_fixture_nonexistent_user_173912/.env", "auth.json", ".env", "missing/.env", ".ENV", ".env.example", ".env.production.local",
             "credential.png", "credential.png/child", "profile-link/auth.json", "out/../.env",
             "missing-link/../.env", "loop", "loop/.env", "~/project.png", "~/.hermes/auth.json",
             "__ROOT__/outside/token.png", "__ROOT__/outside/deep/token.png",
             "__ROOT__/outside/.hub/note", "__ROOT__/project/../user/.hermes/auth.json"]
    suffixes = ["auth.json", "auth.lock", ".anthropic_oauth.json", ".env", "webhook_subscriptions.json",
                "auth/google_oauth.json", "cache/bws_cache.json", "cache/bws_cache.enc.json",
                "mcp-tokens", "mcp-tokens/token.png", "mcp-tokens-other/token.png",
                "browser-profile", "browser-profile/Cookies", "browser-profile-old/Cookies",
                "skills/.hub", "skills/.hub/index-cache", "skills/.hub/index-cache/x",
                "skills/.hub-other/index", "config.yaml", "AUTH.JSON"]
    for base in ["user/.hermes", "user/.hermes/profiles/work"]:
        paths += [f"__ROOT__/{base}/{suffix}" for suffix in suffixes]
    paths += [f"__ROOT__/project/{name}" for name in source._BLOCKED_PROJECT_ENV_BASENAMES]
    # Stable ordering even though the source denylist is a set.
    paths = sorted(set(paths))
    original_home, original_cwd = os.environ.get("HOME"), Path.cwd()
    cases = []
    try:
        with tempfile.TemporaryDirectory(prefix="read-safety-", dir=REPO / "rust/tools") as temp:
            root = Path(temp)
            for entry in directories:
                (root / entry).mkdir(parents=True, exist_ok=True)
            for entry, target in links.items():
                (root / entry).symlink_to(target)
            os.environ["HOME"] = str(root / "user")
            os.chdir(root / "project")
            source._hermes_home_path = lambda: root / "user/.hermes/profiles/work"
            source._hermes_root_path = lambda: root / "user/.hermes"
            for path in paths:
                actual = path.replace("__ROOT__", temp)
                try:
                    result = source.get_read_block_error(actual)
                    expected = result.replace(temp, "__ROOT__") if result else None
                except Exception:
                    expected = "resolution_error"
                try:
                    source.raise_if_read_blocked(actual)
                    allowed = True
                except ValueError:
                    allowed = False
                cases.append(dict(path=path, expected=expected, allowed=allowed))
    finally:
        os.chdir(original_cwd)
        if original_home is None:
            os.environ.pop("HOME", None)
        else:
            os.environ["HOME"] = original_home
    return json.dumps(dict(directories=directories, links=links, cases=cases), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Read safety fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_file_read_safety_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)["cases"]), "read safety cases")
