#!/usr/bin/env python3
"""Execute Python's structured-content codec without opening a user database."""
import ast
import importlib.util
import json
import logging
from pathlib import Path
import sys

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/content-storage-goldens.json"


def generate():
    spec = importlib.util.spec_from_file_location("sanitize_reference", REPO / "agent/message_sanitization.py")
    sanitation = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(sanitation)
    tree = ast.parse((REPO / "hermes_state.py").read_text())
    source = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "SessionDB")
    cls = ast.parse("class Codec:\n    pass").body[0]
    cls.body = [n for n in source.body if (isinstance(n, ast.FunctionDef) and n.name in {"_encode_content", "_decode_content"})
                or (isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "_CONTENT_JSON_PREFIX" for t in n.targets))]
    logger = logging.getLogger("content-storage-oracle")
    logger.disabled = True
    scope = dict(json=json, logger=logger, _sanitize_surrogates=sanitation._sanitize_surrogates)
    module = ast.Module(body=[ast.parse("from __future__ import annotations").body[0], cls], type_ignores=[])
    exec(compile(ast.fix_missing_locations(module), "hermes_state.py (content codec)", "exec"), scope)
    codec = scope["Codec"]
    values = ["plain", "中文 🖼", "[1,2]", "{\"type\":\"text\"}", "\0json:broken", "\0json:null", [], {},
              [{"type": "text", "text": "中文 🖼"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}],
              {"text": "quotes: \"\\\n", "nested": [1, None, False]}]
    cases = []
    for value in values:
        stored = codec._encode_content(value)
        cases.append(dict(input=value, stored=stored, decoded=codec._decode_content(stored)))
    for value in [None, True, 12, "text"]:
        stored = "\0json:" + json.dumps(value)
        cases.append(dict(stored=stored, decoded=codec._decode_content(stored)))
    return json.dumps(cases, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Content codec fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_content_storage_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)), "content storage cases")
