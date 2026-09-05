#!/usr/bin/env python3
"""Execute the Python source's pure media rules without importing the runner.

Run with Python 3.11-3.13. AST extraction excludes gateway startup, credentials,
and enrichment APIs. The classification loop is extracted, not transcribed.
Use --check to verify committed fixtures against the current Python source.
"""
import ast
import enum
import itertools
import json
import sys
from pathlib import Path
from types import SimpleNamespace

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/inbound-media-goldens.json"
NAMES = (
    "_event_media_type_at", "_event_media_is_image", "_event_media_is_audio",
    "_event_media_is_stt_input", "_event_media_is_video",
)


def oracle():
    tree = ast.parse((REPO / "gateway/run.py").read_text())
    base = ast.parse((REPO / "gateway/platforms/base.py").read_text())
    message_type = next(n for n in base.body if isinstance(n, ast.ClassDef) and n.name == "MessageType")
    functions = [next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == name) for name in NAMES]
    runner = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "GatewayRunner")
    prepare = next(n for n in runner.body if isinstance(n, ast.AsyncFunctionDef) and n.name == "_prepare_inbound_message_text")
    media_block = next(n for n in prepare.body if isinstance(n, ast.If) and ast.unparse(n.test) == "event.media_urls")
    loop = next(n for n in media_block.body if isinstance(n, ast.For))
    assert ast.unparse(loop.iter) == "enumerate(event.media_urls)"
    classify = ast.parse('''def classify(event, _pending_stt_prepared):
    image_paths, audio_paths, audio_file_paths, video_paths = [], [], [], []
    return dict(image_paths=image_paths, transcription_paths=audio_paths,
                audio_file_paths=audio_file_paths, video_paths=video_paths)
''').body[0]
    classify.body.insert(1, loop)
    module = ast.fix_missing_locations(ast.Module(body=[message_type, *functions, classify], type_ignores=[]))
    scope = {"Enum": enum.Enum}
    exec(compile(module, "gateway/run.py (extracted media rules)", "exec"), scope)
    return scope


def generate():
    scope = oracle()
    # Rotations put each MIME beyond/inside shorter metadata arrays and preserve
    # mixed ordering. Duplicated paths ensure classification does not deduplicate.
    mimes = ["", "image/png", "audio/ogg", "video/mp4", "application/pdf",
             "IMAGE/PNG", " audio/ogg", "text/plain", "audio/ogg; codecs=opus"]
    layouts = [[], [""], mimes, mimes[:3]]
    layouts.extend(mimes[i:] + mimes[:i] for i in range(1, len(mimes)))
    cases = []
    for kind, types, cached in itertools.product(scope["MessageType"], layouts, [False, True]):
        paths = ["same", "same", "voice.ogg", "movie.mp4", "文档.pdf", "six"]
        event = SimpleNamespace(message_type=kind, media_types=types, media_urls=paths)
        cases.append(dict(
            message_type=kind.value, media_types=types, media_urls=paths,
            pending_stt_prepared=cached,
            predicates=[[scope[name](event, i) for name in NAMES] for i in range(len(paths) + 1)],
            classified=scope["classify"](event, cached),
        ))
    empty = SimpleNamespace(message_type=scope["MessageType"].VOICE, media_types=[], media_urls=[])
    cases.append(dict(message_type="voice", media_types=[], media_urls=[], pending_stt_prepared=False,
                      predicates=[[scope[name](empty, 0) for name in NAMES]],
                      classified=scope["classify"](empty, False)))
    return "[\n" + ",\n".join(json.dumps(case, ensure_ascii=False) for case in cases) + "\n]\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Inbound media fixtures differ from current Python source")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_inbound_media_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print(f"Verified {len(json.loads(content))} Python media cases")
