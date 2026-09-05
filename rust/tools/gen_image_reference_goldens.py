#!/usr/bin/env python3
"""Run Python image-reference extraction against a temporary real filesystem."""
import json
import os
from pathlib import Path
import sys
import tempfile

from gen_image_routing_goldens import REPO, source_module

OUT = REPO / "rust/tools/image-reference-goldens.json"


def generate():
    source = source_module()
    files = ["a.png", "b.JPG", "文档.png", "½.png", "e\u0301.png", "\u0345.png",
             "x.tİff", "x.tıff", "x.tiff", "a-b/c_d.png", ".hidden.png", "with space.png",
             "back\\slash.png", "a.pngsuffix", "a.png后", "a.png\u0301"]
    texts = ["", "no refs", "~/a.png", "__HOME__/a.png __HOME__/b.JPG ~/a.png",
             "~/missing.png ~/directory.png", "`~/a.png` ~/b.JPG", "```python\n~/a.png\n``` ~/b.JPG",
             "```~/a.png```", "```\n~/a.png", "x~/a.png", "(~/a.png),", "https://host/a.png",
             "https://host/a.png?token=abc&x=1).", "https://host/a.pngsuffix",
             "HTTPS://HOST/a.PNG#fragment", "file://__HOME__/a.png", "https://host/a.png https://host/a.png",
             "`https://host/a.png` https://host/b.jpg", "```\nhttps://host/a.png\n```",
             "~/a.png/other ~/a.png後", "~/a.png\u0301", "\u0345~/a.png", "½~/a.png",
             "https://host/a.png?x=1!?)]>", "https://host/a.tİff", "https://host/a.tıff",
             "~/../a.png", "__HOME__//a.png", "~someone/a.png", "https://host/a.png?x='quoted'"]
    texts += ["~/" + filename for filename in files]
    old_home = os.environ.get("HOME")
    cases = []
    try:
        with tempfile.TemporaryDirectory(prefix="image-refs-", dir=REPO / "rust/tools") as temp:
            root = Path(temp)
            for filename in files:
                target = root / filename
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(b"fixture")
            (root / "directory.png").mkdir()
            os.environ["HOME"] = temp
            for text in texts:
                actual = text.replace("__HOME__", temp)
                paths, urls = source.extract_image_refs(actual)
                cases.append(dict(text=text, paths=[p.replace(temp, "__HOME__") for p in paths], urls=urls))
    finally:
        if old_home is None:
            os.environ.pop("HOME", None)
        else:
            os.environ["HOME"] = old_home
    return json.dumps(dict(files=files, directories=["directory.png"], cases=cases), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Image reference fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_image_reference_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)["cases"]), "image reference cases")
