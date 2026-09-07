#!/usr/bin/env python3
"""Compare skill ignore rules and POSIX wildcard behavior with actual Python."""
import fnmatch
import json
from pathlib import Path
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools.skills_guard import _load_skill_ignore

OUT = ROOT / "rust/tools/skill-ignore-goldens.json"


def generate():
    patterns = ["*", "?", "a*b*c", "**a**", "*.md", "[a-z]", "[z-a]",
                "[!z-a]", "[a--b]", "[a-b-c]", "[--a]", "[]]", "[!]]",
                "[", "[]", "[!]", "[?]", "[[]", "[!a-c]", "[a&&b]",
                "[a~|b]", "[a\\b]", "[^a]", "[!a]", "docs/*", "界?"]
    names = list("abcxyz-![]?^&~|\\") + ["", "abc", "axybzc", "\n", "a/b",
            ".hidden", "nested/file.md", "file.MD", "docs/a/b", "界面"]
    globs = [{"pattern": p, "text": n, "expected": fnmatch.fnmatchcase(n, p)}
             for p in patterns for n in names]
    paths = ["SKILL.md", "nested/SKILL.md", ".skillignore", "a/.clawhubignore",
             "docs", "docs/plans/a.md", "nested/docs/a.md", "notes.md",
             "a/notes.md", "a/cache/file", "cache/file", "literal/file",
             "a/literal/file", "a/file.txt", "a//./file.txt", "README.md"]
    ignores = []
    for rules in ["*", "/docs/", "docs/", "/notes.md", "notes.md", "*.txt",
                  "cache", "cache/", "/cache", "literal/", "# comment\n\n",
                  "*.md\u2028/docs/", "[!R]*.md", "!notes.md"]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".skillignore").write_text(rules)
            (root / ".clawhubignore").write_text("README.md\n")
            matcher = _load_skill_ignore(root)
            ignores.append({"rules": rules, "compat_rules": "README.md\n",
                            "paths": paths, "expected": [matcher(p) for p in paths]})
    return {"globs": globs, "ignores": ignores}


if __name__ == "__main__":
    data = generate()
    text = json.dumps(data, indent=2, ensure_ascii=True) + "\n"
    if sys.argv[1:] == ["--check"]:
        assert OUT.read_text() == text, "skill ignore oracle differs"
        print(f"Verified {len(data['globs'])} wildcard and {len(data['ignores'])} ignore-file cases")
    elif not sys.argv[1:]:
        OUT.write_text(text)
    else:
        raise SystemExit("Usage: gen_skill_ignore_goldens.py [--check]")
