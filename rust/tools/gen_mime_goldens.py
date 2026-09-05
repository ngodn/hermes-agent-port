#!/usr/bin/env python3
"""Capture CPython's MIME defaults and execute path MIME inference.

The defaults are data from Python's PSF-licensed standard library. System MIME
files are supplied separately by the Rust runtime, in Python's known-file order.
"""
import io
import json
import mimetypes
from pathlib import Path
import sys

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/mime-goldens.json"
DEFAULTS = REPO / "rust/tools/mime-defaults.json"


def generate():
    database = mimetypes.MimeTypes()
    overrides = "application/x-custom png cust\ntext/x-audit audit # comment\nimage/x-private pic\n"
    paths = ["a.png", "a.PNG", "a.png.gz", "a.png.GZ", "a.svgz", "a.SVGZ", "a.tgz", "a.tbz2", "a.txz",
             "a.Z", "a.txt.Z", "a.txt.z", ".png", "..png", ".hidden.png", "dir/.png", "a.", "a",
             "a.jpg", "a.jpeg", "a.bmp", "a.ico", "a.tif", "a.tiff", "a.avif", "a.heic", "a.webp", "a.audit", "a.cust", "a.pic",
             "https://host/a.png?q=x#f", "a.png?q=x", "data:image/png;base64,abcd", "data:;base64,abcd", "data:bad",
             "http://host/a.png;params?x", "file:///tmp/a.png", "s3://bucket/a.png", "C:/a.png"]
    defaults = dict(types=mimetypes._types_map_default, suffixes=mimetypes._suffix_map_default,
                    encodings=mimetypes._encodings_map_default, knownfiles=mimetypes.knownfiles)
    cases = []
    for custom in [False, True]:
        if custom:
            database.readfp(io.StringIO(overrides))
        for path in paths:
            mime, encoding = database.guess_type(path)
            cases.append(dict(path=path, custom=custom, mime=mime, encoding=encoding))
    return json.dumps(dict(defaults=defaults, overrides=overrides, cases=cases), indent=2, sort_keys=True) + "\n"


if __name__ == "__main__":
    result = json.loads(generate())
    defaults = json.dumps(result.pop("defaults"), indent=2, sort_keys=True) + "\n"
    content = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content or DEFAULTS.read_text() != defaults:
            raise SystemExit("MIME fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_mime_goldens.py [--check]")
    else:
        OUT.write_text(content)
        DEFAULTS.write_text(defaults)
    print("Verified", len(json.loads(content)["cases"]), "MIME cases")
