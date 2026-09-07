#!/usr/bin/env python3
"""Capture canonical skill hashes from actual Python file and bundle hashing."""
import json
from pathlib import Path
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools.skills_guard import content_hash, full_content_hash


def generate():
    cases = []
    for name, files, links, single in [
        ("empty", {}, {}, False),
        ("single", {"data": "00ff0d0a"}, {}, True),
        ("names", {"Z.md": "7a", "a.md": "61", "nested/x": "00ff"}, {}, False),
        ("swapped", {"Z.md": "61", "a.md": "7a", "nested/x": "00ff"}, {}, False),
        ("ignored", {".skillignore": "2a0a", "SKILL.md": "6869", "notes": "ff"}, {}, False),
        ("unicode", {"é.txt": "c3a9", "nested/文": "0a"}, {}, False),
        ("links", {"data": "00ff"}, {"alias": "data", "missing": "absent", "directory": "."}, False),
    ]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for relative, hex_bytes in files.items():
                file = root / relative
                file.parent.mkdir(parents=True, exist_ok=True)
                file.write_bytes(bytes.fromhex(hex_bytes))
            for relative, target in links.items():
                (root / relative).symlink_to(target)
            path = root / "data" if single else root
            cases.append(dict(name=name, files=files, links=links, single=single,
                              full=full_content_hash(path), short=content_hash(path)))
    return json.dumps(cases, indent=2, ensure_ascii=True) + "\n"


if __name__ == "__main__":
    output = ROOT / "rust/tools/skill-hash-goldens.json"
    result = generate()
    if "--check" in sys.argv:
        assert output.read_text() == result, "skill hashes differ"
    else:
        output.write_text(result)
