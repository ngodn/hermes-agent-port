#!/usr/bin/env python3
"""Generate golden test fixtures for bounded coding project facts extraction.

Extracts literal constants directly from `agent/coding_context.py` and runs the
actual Python oracles (`detect_project_facts`, `_project_facts`, `_read_small`)
against real temporary filesystem trees to generate byte-exact test fixtures
for `rust/crates/hermes-gateway/src/coding_project_facts.rs`.

Usage:
    python3 rust/tools/gen_coding_project_facts_goldens.py
    python3 rust/tools/gen_coding_project_facts_goldens.py --check
"""
from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

import agent.coding_context as cc

GOLDENS_PATH = REPO_ROOT / "rust/tools/coding-project-facts-goldens.json"


def generate_constants() -> dict:
    return {
        "project_markers": list(cc._PROJECT_MARKERS),
        "context_files": list(cc._CONTEXT_FILES),
        "project_manifests": [m for m in cc._PROJECT_MARKERS if m not in cc._CONTEXT_FILES],
        "py_lockfiles": [list(pair) for pair in cc._PY_LOCKFILES],
        "js_lockfiles": [list(pair) for pair in cc._JS_LOCKFILES],
        "verify_targets": list(cc._VERIFY_TARGETS),
        "max_verify_commands": cc._MAX_VERIFY_COMMANDS,
        "max_fact_file_bytes": cc._MAX_FACT_FILE_BYTES,
        "max_project_manifests_rendered": 6,
    }


def generate_read_small_cases() -> list[dict]:
    cases = []

    # 1. Empty file
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "empty.txt"
        p.write_bytes(b"")
        cases.append({
            "id": "empty_file",
            "description": "Empty 0-byte file returns empty string",
            "content_hex": b"".hex(),
            "is_dir": False,
            "exists": True,
            "expected": cc._read_small(p),
        })

    # 2. Plain ASCII content
    text = "hello world\nline 2\n"
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "plain.txt"
        p.write_text(text, encoding="utf-8")
        cases.append({
            "id": "plain_ascii",
            "description": "Simple UTF-8 text file",
            "content_hex": text.encode("utf-8").hex(),
            "is_dir": False,
            "exists": True,
            "expected": cc._read_small(p),
        })

    # 3. CRLF newlines normalized to LF
    crlf_raw = b"line1\r\nline2\r\nline3\r\n"
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "crlf.txt"
        p.write_bytes(crlf_raw)
        cases.append({
            "id": "crlf_newlines",
            "description": "CRLF newlines are converted to LF by universal newlines",
            "content_hex": crlf_raw.hex(),
            "is_dir": False,
            "exists": True,
            "expected": cc._read_small(p),
        })

    # 4. Lone CR newlines normalized to LF
    cr_raw = b"line1\rline2\rline3\r"
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "cr.txt"
        p.write_bytes(cr_raw)
        cases.append({
            "id": "lone_cr_newlines",
            "description": "Lone CR newlines are converted to LF by universal newlines",
            "content_hex": cr_raw.hex(),
            "is_dir": False,
            "exists": True,
            "expected": cc._read_small(p),
        })

    # 5. Invalid UTF-8 bytes replaced with replacement char
    invalid_utf8 = b"valid pre \xff\xfe\xfd valid post\n"
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "invalid_utf8.txt"
        p.write_bytes(invalid_utf8)
        cases.append({
            "id": "invalid_utf8_replacement",
            "description": "Invalid UTF-8 bytes are replaced with replacement character U+FFFD",
            "content_hex": invalid_utf8.hex(),
            "is_dir": False,
            "exists": True,
            "expected": cc._read_small(p),
        })

    # 6. File exactly 256 * 1024 bytes (cap boundary)
    exact_cap = b"x" * (256 * 1024)
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "exact_cap.txt"
        p.write_bytes(exact_cap)
        cases.append({
            "id": "exact_max_bytes",
            "description": "File exactly at max size limit (256 KiB) is read",
            "content_hex": None,  # don't store 256KB in json, test generates it
            "size_exact": 256 * 1024,
            "is_dir": False,
            "exists": True,
            "expected": cc._read_small(p),
        })

    # 7. File 256 * 1024 + 1 bytes (exceeds cap)
    over_cap = b"x" * (256 * 1024 + 1)
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "over_cap.txt"
        p.write_bytes(over_cap)
        cases.append({
            "id": "max_bytes_plus_one",
            "description": "File 1 byte over max limit returns empty string",
            "content_hex": None,
            "size_exact": 256 * 1024 + 1,
            "is_dir": False,
            "exists": True,
            "expected": cc._read_small(p),
        })

    # 8. Directory path
    with tempfile.TemporaryDirectory() as td:
        d = Path(td) / "some_dir"
        d.mkdir()
        cases.append({
            "id": "directory_path",
            "description": "Directory returns empty string",
            "content_hex": None,
            "is_dir": True,
            "exists": True,
            "expected": cc._read_small(d),
        })

    # 9. Nonexistent path
    with tempfile.TemporaryDirectory() as td:
        missing = Path(td) / "missing.txt"
        cases.append({
            "id": "nonexistent_path",
            "description": "Missing file returns empty string",
            "content_hex": None,
            "is_dir": False,
            "exists": False,
            "expected": cc._read_small(missing),
        })

    return cases


def generate_project_facts_cases() -> list[dict]:
    specs = [
        {
            "id": "empty_workspace",
            "description": "Empty workspace produces empty facts and snapshot lines",
            "files": {},
            "dirs": [],
        },
        {
            "id": "rust_cargo_only",
            "description": "Rust workspace with Cargo.toml only",
            "files": {
                "Cargo.toml": "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            },
            "dirs": [],
        },
        {
            "id": "python_uv_only",
            "description": "Python workspace with pyproject.toml and uv.lock",
            "files": {
                "pyproject.toml": "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
                "uv.lock": "version = 1\n",
            },
            "dirs": [],
        },
        {
            "id": "python_poetry_pytest_ini",
            "description": "Python Poetry workspace with pytest.ini",
            "files": {
                "pyproject.toml": "[tool.poetry]\nname = \"demo\"\n",
                "poetry.lock": "",
                "pytest.ini": "[pytest]\nminversion = 6.0\n",
            },
            "dirs": [],
        },
        {
            "id": "python_pipenv_pyproject_pytest",
            "description": "Python Pipenv workspace with [tool.pytest in pyproject.toml",
            "files": {
                "pyproject.toml": "[project]\nname = \"demo\"\n\n[tool.pytest.ini_options]\nminversion = \"6.0\"\n",
                "Pipfile.lock": "{}",
            },
            "dirs": [],
        },
        {
            "id": "node_pnpm_scripts",
            "description": "Node workspace with pnpm lock and filtered scripts",
            "files": {
                "package.json": json.dumps({
                    "name": "demo",
                    "scripts": {
                        "build": "vite build",
                        "test": "vitest",
                        "lint": "eslint .",
                        "dev": "vite",
                        "start": "vite",
                    },
                }),
                "pnpm-lock.yaml": "lockfileVersion: '6.0'\n",
            },
            "dirs": [],
        },
        {
            "id": "node_bun_dedup",
            "description": "Node workspace with multiple bun lockfiles deduplicated to one package manager",
            "files": {
                "package.json": json.dumps({
                    "scripts": {"test": "bun test"},
                }),
                "bun.lockb": "",
                "bun.lock": "",
            },
            "dirs": [],
        },
        {
            "id": "node_yarn",
            "description": "Node workspace with yarn.lock and verify targets",
            "files": {
                "package.json": json.dumps({
                    "scripts": {"test": "jest", "check": "tsc"},
                }),
                "yarn.lock": "",
            },
            "dirs": [],
        },
        {
            "id": "node_package_lock_npm",
            "description": "Node workspace with package-lock.json using npm",
            "files": {
                "package.json": json.dumps({
                    "scripts": {"fmt": "prettier -w .", "format": "prettier -w ."},
                }),
                "package-lock.json": "{}",
            },
            "dirs": [],
        },
        {
            "id": "node_no_lockfile_fallback_npm",
            "description": "Node workspace with package.json but no lockfile falls back to npm",
            "files": {
                "package.json": json.dumps({
                    "scripts": {"test": "vitest"},
                }),
            },
            "dirs": [],
        },
        {
            "id": "makefile_targets_ordered",
            "description": "Makefile with various target syntaxes and verify command ordering",
            "files": {
                "Makefile": (
                    "# Top comment\n"
                    "all: build\n"
                    "\n"
                    "test:\n"
                    "\tpytest\n"
                    "lint   :\n"
                    "\tflake8\n"
                    "check: test lint\n"
                    "format :\n"
                    "\tblack .\n"
                    "other_target:\n"
                    "\techo skip\n"
                    "# test:\n"
                    "  lint:\n"
                ),
            },
            "dirs": [],
        },
        {
            "id": "scripts_run_tests_sh_only",
            "description": "Workspace with scripts/run_tests.sh script",
            "files": {
                "scripts/run_tests.sh": "#!/bin/bash\nexit 0\n",
            },
            "dirs": ["scripts"],
        },
        {
            "id": "all_sources_priority_and_max_cap",
            "description": "All verify sources present; commands deduplicated and capped at 8",
            "files": {
                "scripts/run_tests.sh": "#!/bin/sh\nexit 0\n",
                "package.json": json.dumps({
                    "scripts": {
                        "test": "vitest",
                        "tests": "vitest",
                        "lint": "eslint .",
                        "typecheck": "tsc",
                        "check": "checker",
                        "build": "builder",
                        "fmt": "formatter",
                        "format": "formatter",
                    },
                }),
                "pnpm-lock.yaml": "",
                "pytest.ini": "[pytest]\n",
                "Makefile": "test:\ncheck:\n",
            },
            "dirs": ["scripts"],
        },
        {
            "id": "manifests_limit_rendering",
            "description": "More than 6 manifests detected; prompt snapshot caps rendered list at 6",
            "files": {
                "pyproject.toml": "",
                "setup.py": "",
                "setup.cfg": "",
                "requirements.txt": "",
                "package.json": "{}",
                "tsconfig.json": "{}",
                "deno.json": "{}",
                "Cargo.toml": "",
                "go.mod": "",
                "Makefile": "",
                "uv.lock": "",
                "pnpm-lock.yaml": "",
            },
            "dirs": [],
        },
        {
            "id": "context_files_only",
            "description": "Context files present without manifests; excluded from manifests list",
            "files": {
                "AGENTS.md": "# Agents\n",
                "CLAUDE.md": "# Claude\n",
                ".cursorrules": "// Cursor rules\n",
            },
            "dirs": [],
        },
        {
            "id": "context_files_excluded_from_manifests",
            "description": "Context files are filtered out of manifests list even though they are project markers",
            "files": {
                "Cargo.toml": "[package]\nname = \"demo\"\n",
                "AGENTS.md": "# Agents\n",
                "CLAUDE.md": "# Claude\n",
                ".cursorrules": "// Cursor rules\n",
            },
            "dirs": [],
        },
        {
            "id": "dirs_matching_manifest_names_ignored",
            "description": "Directories matching manifest or context filenames are not counted as files",
            "files": {
                "Cargo.toml": "[package]\nname = \"demo\"\n",
            },
            "dirs": [
                "Makefile",
                "package.json",
                "scripts/run_tests.sh",
                "AGENTS.md",
            ],
        },
        {
            "id": "package_json_malformed_json",
            "description": "Malformed package.json is gracefully ignored without raising error",
            "files": {
                "package.json": "{ malformed json: true ",
            },
            "dirs": [],
        },
        {
            "id": "package_json_scripts_null",
            "description": "package.json with null scripts key is handled gracefully",
            "files": {
                "package.json": json.dumps({"name": "demo", "scripts": None}),
            },
            "dirs": [],
        },
        {
            "id": "package_json_scripts_empty",
            "description": "package.json with empty scripts dictionary",
            "files": {
                "package.json": json.dumps({"name": "demo", "scripts": {}}),
            },
            "dirs": [],
        },
        {
            "id": "package_json_scripts_not_object",
            "description": "package.json with array scripts key is handled safely",
            "files": {
                "package.json": "{\"scripts\": [\"test\", \"lint\"]}",
            },
            "dirs": [],
        },
        {
            "id": "size_cap_package_json",
            "description": "package.json larger than 256 KiB is present as manifest but not read for scripts",
            "files": {
                "package.json": json.dumps({"scripts": {"test": "vitest"}}) + " " * (257 * 1024),
            },
            "dirs": [],
        },
        {
            "id": "size_cap_pyproject_toml",
            "description": "pyproject.toml larger than 256 KiB is present as manifest but not read for pytest detection",
            "files": {
                "pyproject.toml": "[tool.pytest.ini_options]\n" + " " * (257 * 1024),
            },
            "dirs": [],
        },
        {
            "id": "size_cap_makefile",
            "description": "Makefile larger than 256 KiB is present as manifest but not read for verify commands",
            "files": {
                "Makefile": "test:\n\techo 1\n" + " " * (257 * 1024),
            },
            "dirs": [],
        },
        {
            "id": "makefile_crlf",
            "description": "Makefile with CRLF line endings matches targets correctly",
            "files": {
                "Makefile": "test:\r\n\techo 1\r\nlint:\r\n\techo 2\r\n",
            },
            "dirs": [],
        },
        {
            "id": "polyglot_monorepo",
            "description": "Polyglot monorepo with multiple manifests, tools and context files",
            "files": {
                "go.mod": "module example.com/mod\n",
                "Dockerfile": "FROM golang:1.21\n",
                "scripts/run_tests.sh": "#!/bin/sh\ngo test ./...\n",
                "Makefile": "build:\n\tgo build\ncheck:\n\tgolangci-lint run\n",
                "AGENTS.md": "# Project Instructions\n",
            },
            "dirs": ["scripts"],
        },
        {
            "id": "pyproject_without_pytest",
            "description": "pyproject.toml without pytest section does not add pytest verify command",
            "files": {
                "pyproject.toml": "[tool.black]\nline-length = 88\n",
            },
            "dirs": [],
        },
        {
            "id": "multiple_lockfiles_order",
            "description": "Multiple lockfiles ordered according to priority sequence and deduplicated",
            "files": {
                "uv.lock": "",
                "Pipfile.lock": "",
                "yarn.lock": "",
                "bun.lockb": "",
            },
            "dirs": [],
        },
    ]

    cases = []
    for spec in specs:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            for d in spec["dirs"]:
                (root / d).mkdir(parents=True, exist_ok=True)
            for rel_path, content in spec["files"].items():
                p = root / rel_path
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(content, encoding="utf-8")

            facts = cc.detect_project_facts(root)
            lines = cc._project_facts(root)

            cases.append({
                "id": spec["id"],
                "description": spec["description"],
                "files": spec["files"],
                "dirs": spec["dirs"],
                "expected": {
                    "manifests": facts.manifests,
                    "package_managers": facts.package_managers,
                    "verify_commands": facts.verify_commands,
                    "context_files": facts.context_files,
                    "lines": lines,
                },
            })

    return cases


def build_goldens() -> dict:
    return {
        "constants": generate_constants(),
        "read_small_cases": generate_read_small_cases(),
        "project_facts_cases": generate_project_facts_cases(),
    }


def main():
    goldens = build_goldens()
    rendered = json.dumps(goldens, indent=2) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not GOLDENS_PATH.exists():
            raise SystemExit(f"Missing {GOLDENS_PATH}")
        existing = GOLDENS_PATH.read_text(encoding="utf-8")
        if existing != rendered:
            raise SystemExit(
                f"{GOLDENS_PATH} is out of date. Run `python3 rust/tools/gen_coding_project_facts_goldens.py` to regenerate."
            )
        print(f"Verified {len(goldens['project_facts_cases'])} project facts cases and {len(goldens['read_small_cases'])} read_small cases.")
        return

    if not sys.argv[1:]:
        GOLDENS_PATH.write_text(rendered, encoding="utf-8")
        print(f"Wrote {len(goldens['project_facts_cases'])} project facts cases and {len(goldens['read_small_cases'])} read_small cases to {GOLDENS_PATH}")
        return

    raise SystemExit("usage: gen_coding_project_facts_goldens.py [--check]")


if __name__ == "__main__":
    main()
