#!/usr/bin/env python3
"""Execute the Python STT orchestration with deterministic provider effects.

Provider and path callbacks are controlled here; actual speech services are
outside this contract. Python's method body, imports, fallback and string logic
execute unchanged.
"""
import ast
import asyncio
import json
import logging
from pathlib import Path
import sys
from types import ModuleType, SimpleNamespace

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/transcription-goldens.json"


async def generate():
    tree = ast.parse((REPO / "gateway/run.py").read_text())
    runner = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "GatewayRunner")
    method = next(n for n in runner.body if isinstance(n, ast.AsyncFunctionDef) and n.name == "_enrich_message_with_transcription")
    cls = ast.parse("class Runner:\n    pass").body[0]
    cls.body = [method]
    logger = logging.getLogger("transcription-oracle")
    logger.addHandler(logging.NullHandler())
    logger.propagate = False
    scope = dict(asyncio=asyncio, logger=logger)
    module = ast.Module(body=[ast.parse("from __future__ import annotations").body[0], cls], type_ignores=[])
    exec(compile(ast.fix_missing_locations(module), "gateway/run.py (transcription)", "exec"), scope)
    cases = []
    primary_results = [
        {"success": True, "transcript": "spoken"},
        {"success": True, "transcript": "\u001c \u001f"},
        {"success": True, "transcript": None},
        {"success": True, "transcript": 0},
        {"success": True, "transcript": 123},
        {"success": True},
        {"success": False}, {}, [],
        {"success": "false", "transcript": "truthy"},
    ]
    for primary in primary_results:
        for fallback in [{"success": True, "transcript": "recovered"}, {"success": False}, {"raise": True}]:
            cases.append(dict(enabled=True, available=True, caption="caption", paths=["a", "b", "a"], primary=primary, fallback=fallback))
    cases += [
        dict(enabled=True, available=True, caption="caption", paths=["a"], primary={"raise": True}, fallback={"success": True, "transcript": "must not run"}),
        dict(enabled=True, available=True, caption="", paths=[], primary={}, fallback={}),
    ]
    for enabled, available in [(False, True), (True, False)]:
        for caption in ["", "caption", "\u001c(The user sent a message with no text content)\u001f"]:
            cases.append(dict(enabled=enabled, available=available, caption=caption, paths=["a", "b", "a"], primary={}, fallback={}))
    for case in cases:
        calls = []
        def absolute(path):
            calls.append(["absolute", path])
            return "/fixture/" + path
        async def duration(path):
            calls.append(["duration", path])
            return "0:12" if path.endswith("a") else ""
        def result(kind, path, *args):
            calls.append([kind, path])
            if path == "b":
                return {"success": True, "transcript": "second"}
            value = case[kind]
            if isinstance(value, dict) and value.get("raise"):
                raise RuntimeError("provider failed")
            return value
        def visible(path):
            calls.append(["visible", path])
            return "/agent" + path
        providers = ModuleType("tools.transcription_tools")
        providers.transcribe_audio = lambda path, *args: result("primary", path)
        providers.transcribe_audio_local_fallback = lambda path: result("fallback", path)
        paths = ModuleType("tools.credential_files")
        paths.to_agent_visible_cache_path = visible
        sys.modules["tools.transcription_tools"] = providers if case["available"] else None
        sys.modules["tools.credential_files"] = paths
        scope["os"] = SimpleNamespace(path=SimpleNamespace(abspath=absolute))
        scope["_probe_audio_duration"] = duration
        obj = scope["Runner"]()
        obj.config = SimpleNamespace(stt_enabled=case["enabled"])
        output = await obj._enrich_message_with_transcription(case["caption"], case["paths"])
        case["expected"] = dict(output=output, calls=calls)
    return json.dumps(cases, indent=2) + "\n"


if __name__ == "__main__":
    content = asyncio.run(generate())
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Transcription fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_transcription_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)), "transcription orchestration cases")
