#!/usr/bin/env python3
"""Execute Python's media text builders and attachment-note expressions.

The oracle excludes sandbox translation and the rest
of the inbound pipeline. Those are separate contracts, not approximated here.
"""
import ast
import itertools
import json
import mimetypes
import os
import re
import sys
from types import SimpleNamespace

from gen_inbound_media_goldens import REPO, oracle

OUT = REPO / "rust/tools/media-context-goldens.json"


def generate():
    scope = oracle()
    tree = ast.parse((REPO / "gateway/run.py").read_text())
    names = {"_build_media_placeholder", "_build_document_context_note"}
    functions = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in names]
    assert len(functions) == len(names)
    runner = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "GatewayRunner")
    prepare = next(n for n in runner.body if isinstance(n, ast.AsyncFunctionDef) and n.name == "_prepare_inbound_message_text")
    assignments = {"basename", "parts", "display_name"}
    document_block = next(n for n in prepare.body if isinstance(n, ast.If)
                          and ast.unparse(n.test) == "event.media_urls"
                          and any(isinstance(call, ast.Call) and isinstance(call.func, ast.Name)
                                  and call.func.id == "_build_document_context_note" for call in ast.walk(n)))
    name_nodes = [n for n in ast.walk(document_block) if isinstance(n, ast.Assign)
                  and any(isinstance(t, ast.Name) and t.id in assignments for t in n.targets)]
    assert len(name_nodes) == 4
    name_fn = ast.parse("def display_name_for(path):\n    pass").body[0]
    name_fn.body = name_nodes + [ast.Return(value=ast.Name(id="display_name", ctx=ast.Load()))]
    functions.append(name_fn)
    text_extensions = next(n for n in ast.walk(document_block) if isinstance(n, ast.Assign)
                           and any(isinstance(t, ast.Name) and t.id == "_TEXT_EXTENSIONS" for t in n.targets))
    mime_branch = next(n for n in ast.walk(document_block) if isinstance(n, ast.If)
                       and isinstance(n.test, ast.Compare) and isinstance(n.test.left, ast.Name) and n.test.left.id == "mtype")
    mime_fn = ast.parse("def document_mime_for(path, mtype):\n    pass").body[0]
    mime_fn.body = [mime_branch, ast.Return(value=ast.Name(id="mtype", ctx=ast.Load()))]
    functions.extend([text_extensions, mime_fn])
    scope.update(os=os, re=re, _mimetypes=mimetypes)
    for kind in ["audio", "video"]:
        condition = "audio_file_paths" if kind == "audio" else "video_paths"
        block = next(n for n in prepare.body if isinstance(n, ast.If) and ast.unparse(n.test) == condition)
        note = next(n for n in ast.walk(block) if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "_note" for t in n.targets))
        function = ast.parse(f"def {kind}_note(_display, _agent_path):\n    return None").body[0]
        function.body[0].value = note.value
        functions.append(function)
    exec(compile(ast.fix_missing_locations(ast.Module(body=functions, type_ignores=[])),
                 "gateway/run.py (media context builders)", "exec"), scope)
    placeholders = []
    for kind, mimes in itertools.product(scope["MessageType"], [[], ["image/png", "application/pdf", "audio/ogg", "video/mp4"], ["IMAGE/PNG", "", "text/plain"]]):
        paths = ["one.png", "文档.pdf", "voice.ogg", "movie.mp4"]
        event = SimpleNamespace(message_type=kind, media_types=mimes, media_urls=paths)
        placeholders.append(dict(message_type=kind.value, media_types=mimes, media_urls=paths,
                                 expected=scope["_build_media_placeholder"](event)))
    placeholders.append(dict(message_type="photo", media_types=[], media_urls=[], expected=""))
    documents = []
    attachments = []
    for name, path in [("notes.txt", "/cache/notes.txt"), ("文档's.pdf", "/cache/空 格.pdf"), ("", ""), ("a[quoted]", "line\nnext")]:
        for mime, inlined in itertools.product(["text/plain", "application/pdf", "", "Text/plain", "text/markdown"], [False, True]):
            documents.append(dict(display_name=name, agent_path=path, mtype=mime, content_inlined=inlined,
                                  expected=scope["_build_document_context_note"](name, path, mime, content_inlined=inlined)))
        attachments.append(dict(display_name=name, agent_path=path,
                                audio=scope["audio_note"](name, path), video=scope["video_note"](name, path)))
    names = [dict(path=path, expected=scope["display_name_for"](path)) for path in [
        "", "/cache/", "one.txt", "one_two.txt", "id_stamp_name_more.pdf",
        "/cache/id_stamp_中文 ½Ⅳ.txt", "id_stamp_e\u0301.txt", "id_stamp_\u0345.txt",
        "id_stamp_[x]'\n.png", "id_stamp_", "__name.txt", "a_b_../x.txt",
        "C:\\cache\\one.txt", "/cache/a_b_a-b.c_d.txt",
    ]]
    mime_cases = [dict(path=path, supplied=mime, expected=scope["document_mime_for"](path, mime))
                 for path, mime in itertools.product(["a.txt", "a.JSON", "a.md", "a.xml", "a.yaml", "a.csv", "a.toml", "a.pdf", "a.ttf", "a.png", "a.txt.gz", "unknown.xyzabc", ".txt", "..txt"],
                                                     ["", "application/octet-stream", "custom/type", "Application/octet-stream"])]
    return json.dumps(dict(placeholders=placeholders, documents=documents, attachments=attachments, names=names, mime_cases=mime_cases), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Media context fixtures differ from Python source")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_media_context_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified media context fixtures:", {key: len(value) for key, value in json.loads(content).items()})
