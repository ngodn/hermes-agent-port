#!/usr/bin/env python3
"""Run native image loading with real files, read guards and Pillow decoding.

Pass-through cases compare bytes; transcoded cases compare MIME, dimensions and
RGBA pixels since PNG encoders need not produce identical compressed streams.
"""
import base64
from io import BytesIO
import json
import logging
import os
from pathlib import Path
import sys
import tempfile
from types import ModuleType

from PIL import Image
from gen_file_read_safety_goldens import load_source
from gen_image_routing_goldens import REPO, source_module

OUT = REPO / "rust/tools/native-image-goldens.json"


def generate():
    source = source_module()
    source.logger.disabled = True
    guard = load_source()
    sys.modules["agent.file_safety"] = guard
    aux = ModuleType("agent.auxiliary_client")
    aux._runtime_main_value = lambda key: "managed" if key == "provider" else ""
    sys.modules["agent.auxiliary_client"] = aux
    caps = ModuleType("hermes_cli.local_runtime.capabilities")
    sys.modules["hermes_cli.local_runtime.capabilities"] = caps
    universal = ["image/png", "image/jpeg", "image/gif", "image/webp"]
    files = {}
    for fmt, mode in [("PNG", "RGBA"), ("JPEG", "RGB"), ("GIF", "RGB"), ("WEBP", "RGBA"), ("BMP", "RGB"), ("TIFF", "RGBA")]:
        picture = Image.new(mode, (2, 3), (25, 50, 75, 125) if mode == "RGBA" else (25, 50, 75))
        buffer = BytesIO()
        picture.save(buffer, format=fmt, **({"lossless": True} if fmt == "WEBP" else {}))
        files[f"image.{fmt.lower()}"] = buffer.getvalue()
    files["wrong.webp"] = files["image.png"]
    integer_image = Image.new("I", (2, 3))
    integer_image.putdata([0, 128, 255, 256, 1024, 65535])
    buffer = BytesIO()
    integer_image.save(buffer, format="TIFF")
    files["integer.tiff"] = buffer.getvalue()
    import struct
    for mode, code, values in [("I;16", "H", [0, 128, 255, 256, 1024, 65535]), ("F", "f", [-2.5, 0.5, 128.9, 255.0, 256.0, 1024.0])]:
        picture = Image.frombytes(mode, (2, 3), struct.pack("<6" + code, *values))
        buffer = BytesIO()
        picture.save(buffer, format="TIFF")
        files[f"numeric-{code}.tiff"] = buffer.getvalue()
    files["corrupt.png"] = b"not a decoded image"
    files["vector.svg"] = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>"
    files["unknown.bin"] = b"unknown"
    files[".env"] = files["image.png"]
    signatures = [b"", b"x", b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff", b"GIF87a", b"GIF89a",
                  b"RIFF1234WEBP", b"RIFFWEBP", b"BM", b"II*\0", b"MM\0*", b"\0\0\1\0",
                  b" \t\n<SVG></SVG>", b"\x1c<svg/>", b"<?xml?><svg/>", b"<?xml?>no svg", b"\xef\xbb\xbf<svg/>"]
    signatures += [b"1234ftyp" + brand for brand in [b"avif", b"avis", b"heic", b"mif1", b"msf1", b"junk"]]
    sniff = [dict(bytes=list(raw), expected=source._sniff_mime_from_bytes(raw)) for raw in signatures]
    cases = []
    for name in files:
        cases.append(dict(paths=[name], urls=[], caption="\u001c caption \u001f", managed=False,
                          pixels=name.endswith((".bmp", ".tiff"))))
    cases += [
        dict(paths=["image.webp"], urls=[], caption="", managed=True, pixels=True),
        dict(paths=["image.bmp", "image.bmp"], urls=[" https://host/a.png "], caption="", managed=False, pixels=True),
        dict(paths=["image.png", "missing.png", ".env", "directory.png", "image.png"], urls=["", "\u001c https://host/a.png \u001f", "https://host/a.png"], caption="", managed=False, pixels=False),
        dict(paths=[], urls=[], caption="\u001c \u001f", managed=False, pixels=False),
        dict(paths=[], urls=["https://host/a.png"], caption="", managed=False, pixels=False),
        dict(paths=["missing.png"], urls=[], caption="unchanged", managed=False, pixels=False),
    ]
    old_cwd = Path.cwd()
    try:
        with tempfile.TemporaryDirectory(prefix="native-image-", dir=REPO / "rust/tools") as temp:
            root = Path(temp)
            guard._hermes_home_path = lambda: root / ".hermes/profile"
            guard._hermes_root_path = lambda: root / ".hermes"
            for name, raw in files.items():
                (root / name).write_bytes(raw)
            (root / "directory.png").mkdir()
            os.chdir(root)
            for case in cases:
                caps.is_managed_provider = lambda *args: case["managed"]
                caps.ACCEPTED_IMAGE_MIMES = ["image/png", "image/jpeg", "image/gif"]
                parts, skipped = source.build_native_content_parts(case["caption"], case["paths"], case["urls"])
                if case["pixels"]:
                    for part in parts:
                        if part["type"] == "image_url" and part["image_url"]["url"].startswith("data:"):
                            header, data = part["image_url"]["url"].split(",", 1)
                            with Image.open(BytesIO(base64.b64decode(data))) as picture:
                                part["image_url"] = dict(mime=header[5:].split(";")[0], size=list(picture.size), rgba=list(picture.convert("RGBA").tobytes()))
                case["expected"] = dict(parts=parts, skipped=skipped)
    finally:
        os.chdir(old_cwd)
    return json.dumps(dict(files={name: base64.b64encode(raw).decode() for name, raw in files.items()}, sniff=sniff, cases=cases, universal=universal), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Native image fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_native_image_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified native image cases:", {key: len(json.loads(content)[key]) for key in ["sniff", "cases"]})
