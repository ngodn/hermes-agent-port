#!/usr/bin/env python3
"""Execute Python pending-event merging and caption rules for Rust parity."""
import ast
import itertools
import json
import sys
from types import SimpleNamespace

from gen_inbound_media_goldens import REPO, oracle

OUT = REPO / "rust/tools/pending-message-goldens.json"


def generate():
    scope = oracle()
    tree = ast.parse((REPO / "gateway/platforms/base.py").read_text())
    base = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "BasePlatformAdapter")
    caption = next(n for n in base.body if isinstance(n, ast.FunctionDef) and n.name == "_merge_caption")
    cls = ast.parse("class BasePlatformAdapter:\n    pass").body[0]
    cls.body = [caption]
    functions = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in {"_invalidate_pending_stt_cache", "merge_pending_message_event"}]
    future = ast.parse("from __future__ import annotations").body[0]
    exec(compile(ast.fix_missing_locations(ast.Module(body=[future, cls, *functions], type_ignores=[])),
                 "gateway/platforms/base.py (pending merge)", "exec"), scope)

    def event(kind, text, identifier, paths, mimes, flags):
        return dict(message_type=kind.value, text=text, message_id=identifier,
                    reply_to_message_id="reply-" + identifier,
                    media_urls=paths, media_types=mimes, media_text_inlined=flags)

    cases = []
    for i, (before_type, incoming_type, merge_text) in enumerate(itertools.product(scope["MessageType"], scope["MessageType"], [False, True])):
        layout = (i // 2) % 4
        before = event(before_type, "Meeting agenda", "old", ["old"] if layout & 1 else [], [], [True, False] if layout & 1 else [])
        incoming = event(incoming_type, "Meeting", "new", ["new", "new"] if layout & 2 else [], ["audio/ogg"] if layout & 2 else [], [])
        cases.append(dict(before=before, incoming=incoming, merge_text=merge_text))
    # Empty photo bursts still merge, whitespace-only captions participate in
    # exact caption matching, and extra inline flags must not be truncated.
    for old_text, new_text in [("", ""), ("same", " same "), ("\u001c old ", " new\u001f"), ("a\n\nb", " b ")]:
        cases.append(dict(before=event(scope["MessageType"].PHOTO, old_text, "old", [], [], []),
                          incoming=event(scope["MessageType"].PHOTO, new_text, "new", [], [], []), merge_text=False))
    for case in cases:
        def instance(data):
            values = {**data, "message_type": scope["MessageType"](data["message_type"])}
            # The Python function mutates arrays, so do not reuse fixture inputs.
            for key in ["media_urls", "media_types", "media_text_inlined"]:
                values[key] = list(values[key])
            return SimpleNamespace(**values)

        pending = {"s": instance(case["before"])}
        scope["merge_pending_message_event"](pending, "s", instance(case["incoming"]), merge_text=case["merge_text"])
        output = vars(pending["s"]).copy()
        output["message_type"] = output["message_type"].value
        case["expected"] = output
    return "[\n" + ",\n".join(json.dumps(case) for case in cases) + "\n]\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Pending message fixtures differ from Python source")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_pending_message_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)), "pending-event merges")
