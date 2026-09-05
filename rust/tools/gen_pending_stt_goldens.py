#!/usr/bin/env python3
"""Execute pending-STT methods with deterministic transcription/send callbacks.

This checks cache and echo orchestration, not an external speech provider or
platform adapter. Method bodies and invalidation come from the Python source.
"""
import ast
import asyncio
import json
import logging
import sys
from types import SimpleNamespace

from gen_inbound_media_goldens import REPO, oracle

OUT = REPO / "rust/tools/pending-stt-goldens.json"


def runner_type():
    scope = oracle()
    tree = ast.parse((REPO / "gateway/run.py").read_text())
    runner = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "GatewayRunner")
    names = {"_pending_event_audio_paths", "_transcribe_pending_audio_event_once",
             "_echo_pending_stt_transcripts_once", "_prepare_clarify_reply_text",
             "_transcribe_and_echo_pending_voice"}
    methods = [n for n in runner.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name in names]
    assert len(methods) == len(names)
    cls = ast.parse("class Runner:\n    pass").body[0]
    cls.body = methods
    base = ast.parse((REPO / "gateway/platforms/base.py").read_text())
    invalidator = next(n for n in base.body if isinstance(n, ast.FunctionDef) and n.name == "_invalidate_pending_stt_cache")
    future = ast.parse("from __future__ import annotations").body[0]
    scope["logger"] = logging.getLogger("pending-stt-oracle")
    scope["logger"].addHandler(logging.NullHandler())
    scope["logger"].propagate = False
    scope["_UNSET"] = object()
    exec(compile(ast.fix_missing_locations(ast.Module(body=[future, cls, invalidator], type_ignores=[])),
                 "gateway/run.py (pending STT)", "exec"), scope)
    return scope


async def generate():
    scope = runner_type()
    scenarios = [
        dict(name="combined-fallback-and-reuse", kind="voice", text="caption", paths=["note.ogg"], mimes=[], steps=[
            dict(op="combined", user_text="original", fail=True),
            dict(op="combined", user_text="original", text="", transcripts=["spoken"], routing_fail=True),
            dict(op="combined", user_text="fallback", text="unused", transcripts=[]),
            dict(op="combined", user_text="next", text="unused", transcripts=[]),
        ]),
        dict(name="combined-no-audio", kind="document", text="caption", paths=["song.mp3"], mimes=["audio/mpeg"], steps=[
            dict(op="combined", user_text="original", fail=True, routing_fail=True),
        ]),
        dict(name="merge-and-echo-failure", kind="voice", text="caption", paths=["one.ogg"], mimes=["audio/ogg"], steps=[
            dict(op="transcribe", text="first", transcripts=["same"]),
            dict(op="transcribe", user_text="ignored", text="must not run", transcripts=[]),
            dict(op="echo", transcripts=["same"], fail=True),
            dict(op="echo", transcripts=["same"]),
            dict(op="append", path="two.ogg", mime="audio/ogg"),
            dict(op="transcribe", user_text="", text="merged", transcripts=["same", "same"]),
            dict(op="echo", transcripts=["same", "same"]),
            dict(op="echo", transcripts=["shorter"]),
        ]),
        dict(name="no-audio", kind="audio", text="", paths=["song.mp3"], mimes=["audio/mpeg"], steps=[
            dict(op="transcribe", text="unused", transcripts=[]),
            dict(op="transcribe", user_text="", text="unused", transcripts=[]),
            dict(op="transcribe", user_text="override", text="unused", transcripts=[]),
            dict(op="clarify", text="unused", transcripts=[]),
        ]),
        dict(name="retry-and-null-cache", kind="voice", text="original", paths=["file.pdf"], mimes=["application/pdf"], steps=[
            dict(op="transcribe", fail=True),
            dict(op="transcribe", text=None, transcripts=[]),
            dict(op="transcribe", user_text="ignored", text="unused", transcripts=[]),
        ]),
        dict(name="echo-gates", kind="voice", text="", paths=[], mimes=[], steps=[
            dict(op="echo", enabled=False, transcripts=["a"]),
            dict(op="echo", available=False, transcripts=["a"]),
            dict(op="echo", transcripts=[]),
            dict(op="echo", transcripts=["a", "a"]),
        ]),
        dict(name="clarify-transcripts", kind="voice", text="caption", paths=["one.ogg"], mimes=[], steps=[
            dict(op="clarify", text="enriched", transcripts=["\u001c first \u001f", " \n", "\u0085second\u00a0", "\u001e"]),
            dict(op="clarify", text="unused", transcripts=[]),
        ]),
        dict(name="clarify-plain", kind="text", text="\u001c plain \u001f", paths=[], mimes=[], steps=[
            dict(op="clarify", text="unused", transcripts=[]),
        ]),
    ]
    for scenario in scenarios:
        runner = scope["Runner"]()
        event = SimpleNamespace(message_type=scope["MessageType"](scenario["kind"]), text=scenario["text"],
                                media_urls=list(scenario["paths"]), media_types=list(scenario["mimes"]))
        calls, sends = [], []
        for step in scenario["steps"]:
            async def enrich(text, paths):
                calls.append(dict(text=text, paths=list(paths)))
                if step.get("fail"):
                    raise RuntimeError("transcription failed")
                return step.get("text"), step.get("transcripts", [])

            async def send(chat_id, text, metadata=None):
                sends.append(text)
                if step.get("fail"):
                    raise RuntimeError("send failed")

            runner._enrich_message_with_transcription = enrich
            runner._should_echo_stt_transcripts = lambda: step.get("enabled", True)
            runner._reply_anchor_for_event = lambda event: None
            def metadata(*args):
                if step.get("routing_fail"):
                    raise RuntimeError("routing failed")
                return None
            runner._thread_metadata_for_source = metadata
            result = None
            try:
                if step["op"] == "combined":
                    adapter = SimpleNamespace(send=send) if step.get("available", True) else None
                    result = await runner._transcribe_and_echo_pending_voice(event, adapter, SimpleNamespace(chat_id="chat"), step["user_text"], log_context="test")
                elif step["op"] == "transcribe":
                    result = await runner._transcribe_pending_audio_event_once(event, step.get("user_text"))
                elif step["op"] == "clarify":
                    result = await runner._prepare_clarify_reply_text(event)
                elif step["op"] == "append":
                    event.media_urls.append(step["path"])
                    event.media_types.append(step["mime"])
                    scope["_invalidate_pending_stt_cache"](event)
                elif step["op"] == "echo":
                    adapter = SimpleNamespace(send=send) if step.get("available", True) else None
                    await runner._echo_pending_stt_transcripts_once(event, adapter, SimpleNamespace(chat_id="chat"), step["transcripts"])
                else:
                    raise AssertionError(step["op"])
            except RuntimeError:
                result = {"error": True}
            step["expected"] = dict(result=result, calls=list(calls), sends=list(sends))
    return json.dumps(scenarios, indent=2) + "\n"


if __name__ == "__main__":
    content = asyncio.run(generate())
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Pending STT fixtures differ from Python source")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_pending_stt_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", sum(len(s["steps"]) for s in json.loads(content)), "pending STT transitions")
