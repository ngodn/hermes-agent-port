#!/usr/bin/env python3
"""Generate golden fixtures for tools/skills_guard.py _check_structure.

Exercises _check_structure against real temporary skill directories using the
actual _load_skill_ignore implementation from tools/skills_guard.

Covers:
1. Symlink escaping outside the skill directory (relative, nested, prefix confusion)
2. Broken symlinks (missing targets inside and outside skill directory)
3. Suspicious binary extensions (.exe, .dll, .so, .dylib, .bin, .dat, .com, .msi, .dmg, .app, .deb, .rpm)
4. Executable permission modes on non-script files (.md, .txt, .json, .yaml, extensionless)
5. Allowed executable script types (.sh, .bash, .py, .rb, .pl)
6. File count thresholds (MAX_FILE_COUNT = 50: exact 50 allowed, 51 flagged)
7. Single file size thresholds (MAX_SINGLE_FILE_KB = 256: 256KB allowed, 257KB flagged)
8. Total skill size thresholds (MAX_TOTAL_SIZE_KB = 5120: 5120KB allowed, 5250KB flagged)
9. Root SKILL.md never ignored (even with * or explicit ignore rules)
10. Ignored directory trees (.skillignore and .clawhubignore) containing threats
11. Glob ignore patterns and leading slash anchoring
12. Combined structural anomalies

All temporary filesystem root paths in finding text are normalized using the
placeholder: <TEMP_DIR>.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

import tools.skills_guard as sg

OUT_GOLDENS = REPO_ROOT / "rust" / "tools" / "skill-structure-goldens.json"
TEMP_DIR_PLACEHOLDER = "<TEMP_DIR>"

STRUCTURAL_PATTERN_ORDER: dict[str, int] = {
    "symlink_escape": 0,
    "broken_symlink": 1,
    "oversized_file": 2,
    "binary_file": 3,
    "unexpected_executable": 4,
    "too_many_files": 5,
    "oversized_skill": 6,
}

SCHEMA_DEFINITION: dict[str, Any] = {
    "case": {
        "name": "Unique identifier string for the test case",
        "category": "Classification category for grouping related scenarios",
        "description": "Explanatory description of what the test case exercises",
        "files": "List of file definitions: {path: str, content: Optional[str], size: int, executable: bool}",
        "symlinks": "List of symlink definitions: {path: str, relative_path: str, target: str}",
        "ignore_file_contents": "Optional string with .skillignore rules (or null)",
        "clawhubignore_contents": "Optional string with .clawhubignore rules (or null)",
        "expected_findings": "List of Finding dicts from Python _check_structure",
        "expected": "Alias for expected_findings",
        "ordering_normalized": "Boolean indicating that deterministic sort order was applied",
    },
    "file_item": {
        "path": "Relative path of file within the skill directory",
        "content": "Text content of the file (null if synthetic sized file)",
        "size": "Size in bytes",
        "executable": "Boolean flag indicating whether executable mode bit 0o111 was set",
    },
    "symlink_item": {
        "path": "Relative path of symlink within the skill directory",
        "relative_path": "Alias for path",
        "target": "Target destination of the symbolic link",
    },
    "finding": {
        "pattern_id": "Rule identifier (e.g. symlink_escape, binary_file, unexpected_executable, oversized_file, too_many_files, oversized_skill)",
        "severity": "Severity level (critical, high, medium, low)",
        "category": "Finding category (traversal, structural)",
        "file": "Relative path of the flagged file, or '(directory)' for skill-level limits",
        "line": "Line number (0 for all structural checks)",
        "match": "Matched snippet or size string (with temporary root normalized to <TEMP_DIR>)",
        "description": "Explanatory finding description from skills_guard.py",
    },
    "ordering": {
        "per_file": "Sorted alphabetically by relative path, then by Python source pattern order on the same file",
        "directory": "Evaluated at end of traversal in Python source order (too_many_files, then oversized_skill)",
    },
}

EXCLUSIONS_DEFINITION: list[str] = [
    "Non-UTF8 filesystem path bytes: All paths in skill packages are valid UTF-8 strings.",
    "OS-specific access control lists (Windows ACLs, macOS extended attributes): Only POSIX mode bits (0o111) are tested.",
    "Live virtual filesystem nodes (/proc, /dev, /sys) as symlink targets.",
    "Hardlink directory loops (prevented by modern POSIX operating systems).",
    "Multi-gigabyte file allocation: Large files use precise sparse sizing rather than multi-GB allocations.",
]


def finding_sort_key(item: dict[str, Any]) -> tuple[int, str, int]:
    is_dir = 1 if item["file"] == "(directory)" else 0
    pattern_rank = STRUCTURAL_PATTERN_ORDER.get(item["pattern_id"], 99)
    return (is_dir, item["file"], pattern_rank)


def build_raw_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    # -----------------------------------------------------------------------
    # Group 1: Clean Baseline Skills
    # -----------------------------------------------------------------------
    cases.append({
        "name": "clean-minimal",
        "category": "clean_baseline",
        "description": "Minimal valid skill containing only root SKILL.md",
        "files": [
            {"path": "SKILL.md", "content": "---\nname: minimal\n---\n# Minimal Skill\n"}
        ],
    })

    cases.append({
        "name": "clean-standard-hierarchy",
        "category": "clean_baseline",
        "description": "Standard skill package with README, references, assets, and executable Python helper",
        "files": [
            {"path": "SKILL.md", "content": "# Standard Skill\n"},
            {"path": "README.md", "content": "# Documentation\n"},
            {"path": "references/guide.md", "content": "# Reference Guide\n"},
            {"path": "assets/config.json", "content": "{\"active\": true}\n"},
            {"path": "scripts/helper.py", "content": "print('ok')\n", "executable": True},
        ],
    })

    cases.append({
        "name": "clean-all-allowed-scripts",
        "category": "clean_baseline",
        "description": "All recognized script extensions (.sh, .bash, .py, .rb, .pl) with executable permission",
        "files": [
            {"path": "SKILL.md", "content": "# Script Collection\n"},
            {"path": "scripts/run.sh", "content": "#!/bin/sh\necho 1\n", "executable": True},
            {"path": "scripts/tool.bash", "content": "#!/bin/bash\necho 2\n", "executable": True},
            {"path": "scripts/main.py", "content": "print(3)\n", "executable": True},
            {"path": "scripts/task.rb", "content": "puts 4\n", "executable": True},
            {"path": "scripts/parse.pl", "content": "print 5;\n", "executable": True},
        ],
    })

    # -----------------------------------------------------------------------
    # Group 2: Symlink Escapes (Path Traversal)
    # -----------------------------------------------------------------------
    cases.append({
        "name": "symlink-escape-parent",
        "category": "symlink_escapes",
        "description": "Symlink pointing to parent directory outside skill root",
        "files": [
            {"path": "SKILL.md", "content": "# Escape Test\n"}
        ],
        "symlinks": [
            {"path": "escape_parent", "target": ".."}
        ],
    })

    cases.append({
        "name": "symlink-escape-outside-dir",
        "category": "symlink_escapes",
        "description": "Symlink pointing to file in external sibling directory outside skill root",
        "files": [
            {"path": "SKILL.md", "content": "# Escape Test\n"}
        ],
        "symlinks": [
            {"path": "leak_secret", "target": "../outside/secret.txt"}
        ],
    })

    cases.append({
        "name": "symlink-escape-nested-relative",
        "category": "symlink_escapes",
        "description": "Deeply nested symlink traversing multiple levels outside skill root",
        "files": [
            {"path": "SKILL.md", "content": "# Escape Test\n"}
        ],
        "symlinks": [
            {"path": "sub/deep/leak", "target": "../../../outside/target.txt"}
        ],
    })

    cases.append({
        "name": "symlink-escape-sibling-prefix-confusion",
        "category": "symlink_escapes",
        "description": "Symlink resolving to sibling directory sharing path prefix (regression defense)",
        "files": [
            {"path": "SKILL.md", "content": "# Escape Test\n"}
        ],
        "symlinks": [
            {"path": "backdoor_link", "target": "../skill-backdoor/malicious.py"}
        ],
    })

    # -----------------------------------------------------------------------
    # Group 3: Symlink Internal (Allowed)
    # -----------------------------------------------------------------------
    cases.append({
        "name": "symlink-internal-valid-file",
        "category": "symlink_internal",
        "description": "Symlink resolving to existing file within skill directory",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "real_script.py", "content": "print('hello')\n"},
        ],
        "symlinks": [
            {"path": "alias.py", "target": "real_script.py"}
        ],
    })

    cases.append({
        "name": "symlink-internal-valid-nested",
        "category": "symlink_internal",
        "description": "Symlink in nested subdirectory resolving to sibling directory within skill",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "scripts/engine.py", "content": "print('engine')\n"},
        ],
        "symlinks": [
            {"path": "tools/engine_link.py", "target": "../scripts/engine.py"}
        ],
    })

    cases.append({
        "name": "symlink-internal-valid-dir",
        "category": "symlink_internal",
        "description": "Directory symlink pointing to another directory within skill",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "scripts/sub/task.py", "content": "print('task')\n"},
        ],
        "symlinks": [
            {"path": "bin", "target": "scripts/sub"}
        ],
    })

    # -----------------------------------------------------------------------
    # Group 4: Broken Symlinks
    # -----------------------------------------------------------------------
    cases.append({
        "name": "symlink-broken-target-inside",
        "category": "symlink_broken",
        "description": "Broken symlink whose target does not exist but is located within skill directory",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"}
        ],
        "symlinks": [
            {"path": "broken_internal", "target": "missing_file.txt"}
        ],
    })

    cases.append({
        "name": "symlink-broken-target-outside",
        "category": "symlink_broken",
        "description": "Broken symlink whose missing target resolves outside skill directory",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"}
        ],
        "symlinks": [
            {"path": "broken_outside", "target": "../missing_outside.txt"}
        ],
    })

    cases.append({
        "name": "symlink-broken-nested-outside",
        "category": "symlink_broken",
        "description": "Nested broken symlink traversing upward to missing external path",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"}
        ],
        "symlinks": [
            {"path": "nested/broken_leak", "target": "../../missing_root.txt"}
        ],
    })

    cases.append({
        "name": "symlink-broken-through-file",
        "category": "symlink_broken",
        "description": "Symlink whose path component treats a regular file as a directory",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "regular.txt", "content": "just text\n"},
        ],
        "symlinks": [
            {"path": "broken_notdir", "target": "regular.txt/subfile"}
        ],
    })

    # -----------------------------------------------------------------------
    # Group 5: Suspicious Binary Suffixes (All 12 from SUSPICIOUS_BINARY_EXTENSIONS)
    # -----------------------------------------------------------------------
    binary_specs = [
        ("exe", "payload.exe"),
        ("dll", "library.dll"),
        ("so", "driver.so"),
        ("dylib", "lib.dylib"),
        ("bin", "firmware.bin"),
        ("dat", "weights.dat"),
        ("com", "command.com"),
        ("msi", "installer.msi"),
        ("dmg", "disk.dmg"),
        ("app", "application.app"),
        ("deb", "package.deb"),
        ("rpm", "package.rpm"),
    ]
    for ext_name, filename in binary_specs:
        cases.append({
            "name": f"binary-suffix-{ext_name}",
            "category": "binary_suffixes",
            "description": f"Suspicious binary extension .{ext_name} detected in skill package",
            "files": [
                {"path": "SKILL.md", "content": "# Skill\n"},
                {"path": filename, "content": "binary content\n"},
            ],
        })

    cases.append({
        "name": "binary-suffix-uppercase",
        "category": "binary_suffixes",
        "description": "Uppercase binary extension .EXE normalized and detected case-insensitively",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "MALWARE.EXE", "content": "binary content\n"},
        ],
    })

    cases.append({
        "name": "binary-suffix-mixedcase",
        "category": "binary_suffixes",
        "description": "Mixed case binary extension .So normalized and detected case-insensitively",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "Module.So", "content": "binary content\n"},
        ],
    })

    cases.append({
        "name": "binary-benign-zip",
        "category": "binary_suffixes",
        "description": "Benign zip archive not present in SUSPICIOUS_BINARY_EXTENSIONS",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "bundle.zip", "content": "zip archive content\n"},
        ],
    })

    cases.append({
        "name": "binary-benign-png",
        "category": "binary_suffixes",
        "description": "Benign image file not present in SUSPICIOUS_BINARY_EXTENSIONS",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "assets/diagram.png", "content": "png bytes\n"},
        ],
    })

    # -----------------------------------------------------------------------
    # Group 6: Executable Modes (Permission Bits)
    # -----------------------------------------------------------------------
    cases.append({
        "name": "exec-mode-markdown",
        "category": "executable_modes",
        "description": "SKILL.md with unexpected executable bit set (0o755)",
        "files": [
            {"path": "SKILL.md", "content": "# Executable Doc\n", "executable": True}
        ],
    })

    cases.append({
        "name": "exec-mode-text",
        "category": "executable_modes",
        "description": "Text file (.txt) with unexpected executable bit set (0o755)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "notes.txt", "content": "plain notes\n", "executable": True},
        ],
    })

    cases.append({
        "name": "exec-mode-json",
        "category": "executable_modes",
        "description": "JSON configuration file (.json) with unexpected executable bit set (0o755)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "config.json", "content": "{\"key\": \"val\"}\n", "executable": True},
        ],
    })

    cases.append({
        "name": "exec-mode-yaml",
        "category": "executable_modes",
        "description": "YAML configuration file (.yaml) with unexpected executable bit set (0o755)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "params.yaml", "content": "port: 8080\n", "executable": True},
        ],
    })

    cases.append({
        "name": "exec-mode-extensionless",
        "category": "executable_modes",
        "description": "Extensionless file with unexpected executable bit set (0o755)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "runner", "content": "#!/usr/bin/env custom\nrun\n", "executable": True},
        ],
    })

    cases.append({
        "name": "exec-mode-binary-with-exec",
        "category": "executable_modes",
        "description": "Binary executable (.exe) with executable bit set, triggering both rules",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "bad.exe", "content": "binary code\n", "executable": True},
        ],
    })

    cases.append({
        "name": "exec-mode-user-bit-only",
        "category": "executable_modes",
        "description": "Non-script file with user-only executable bit (mode 0o700)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "user_exec.txt", "content": "text\n", "mode": 0o700},
        ],
    })

    cases.append({
        "name": "exec-mode-group-bit-only",
        "category": "executable_modes",
        "description": "Non-script file with group-only executable bit (mode 0o654)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "group_exec.txt", "content": "text\n", "mode": 0o654},
        ],
    })

    cases.append({
        "name": "exec-mode-other-bit-only",
        "category": "executable_modes",
        "description": "Non-script file with other-only executable bit (mode 0o645)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "other_exec.txt", "content": "text\n", "mode": 0o645},
        ],
    })

    cases.append({
        "name": "exec-mode-non-exec-clean",
        "category": "executable_modes",
        "description": "Non-script file with standard non-executable mode (0o644)",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "clean.txt", "content": "plain text\n", "mode": 0o644},
        ],
    })

    # -----------------------------------------------------------------------
    # Group 7: Threshold Limits (File Count, Single File Size, Total Size)
    # -----------------------------------------------------------------------
    # File count: MAX_FILE_COUNT = 50
    fifty_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    for i in range(49):
        fifty_files.append({"path": f"file_{i:02d}.txt", "content": "x\n"})
    cases.append({
        "name": "threshold-file-count-exact-50",
        "category": "thresholds",
        "description": "Skill directory with exactly MAX_FILE_COUNT (50) files remains within limits",
        "files": fifty_files,
    })

    fifty_one_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    for i in range(50):
        fifty_one_files.append({"path": f"file_{i:02d}.txt", "content": "x\n"})
    cases.append({
        "name": "threshold-file-count-exceeded-51",
        "category": "thresholds",
        "description": "Skill directory with 51 files exceeds MAX_FILE_COUNT limit of 50",
        "files": fifty_one_files,
    })

    fifty_five_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    for i in range(54):
        fifty_five_files.append({"path": f"file_{i:02d}.txt", "content": "x\n"})
    cases.append({
        "name": "threshold-file-count-large-55",
        "category": "thresholds",
        "description": "Skill directory with 55 files exceeds MAX_FILE_COUNT limit of 50",
        "files": fifty_five_files,
    })

    # Single file size: MAX_SINGLE_FILE_KB = 256 (262144 bytes)
    cases.append({
        "name": "threshold-single-file-exact-256kb",
        "category": "thresholds",
        "description": "Single file with exact size 256KB (262144 bytes) does not exceed limit",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "exact_limit.txt", "size": sg.MAX_SINGLE_FILE_KB * 1024},
        ],
    })

    cases.append({
        "name": "threshold-single-file-exceeded-257kb",
        "category": "thresholds",
        "description": "Single file with size 257KB exceeds MAX_SINGLE_FILE_KB limit of 256KB",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "over_limit.txt", "size": 257 * 1024},
        ],
    })

    cases.append({
        "name": "threshold-single-file-large-300kb",
        "category": "thresholds",
        "description": "Single file with size 300KB exceeds MAX_SINGLE_FILE_KB limit of 256KB",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "big_data.txt", "size": 300 * 1024},
        ],
    })

    # Total skill size: MAX_TOTAL_SIZE_KB = 5120 (5242880 bytes)
    exact_total_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    for i in range(20):
        exact_total_files.append({"path": f"part_{i:02d}.txt", "size": 256 * 1024})
    cases.append({
        "name": "threshold-total-size-exact-5120kb",
        "category": "thresholds",
        "description": "Total directory size exactly 5120KB does not trigger oversized_skill finding",
        "files": exact_total_files,
    })

    over_total_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    for i in range(21):
        over_total_files.append({"path": f"chunk_{i:02d}.txt", "size": 250 * 1024})
    cases.append({
        "name": "threshold-total-size-exceeded-5250kb",
        "category": "thresholds",
        "description": "Total directory size 5250KB exceeds MAX_TOTAL_SIZE_KB limit of 5120KB",
        "files": over_total_files,
    })

    # Combined thresholds: file count + single file size + total directory size
    combined_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    combined_files.append({"path": "huge.txt", "size": 300 * 1024})
    for i in range(50):
        combined_files.append({"path": f"data_{i:02d}.txt", "size": 100 * 1024})
    cases.append({
        "name": "threshold-multiple-limits-hit",
        "category": "thresholds",
        "description": "Skill exceeding file count (52), single file size (300KB), and total size (5300KB)",
        "files": combined_files,
    })

    # -----------------------------------------------------------------------
    # Group 8: Root SKILL.md Never Ignored
    # -----------------------------------------------------------------------
    cases.append({
        "name": "ignore-root-skill-md-wildcard",
        "category": "root_skill_md_protection",
        "description": "Wildcard pattern * in .skillignore ignores other files but never root SKILL.md",
        "files": [
            {"path": "SKILL.md", "content": "# Protected Root Skill\n"},
            {"path": "other.txt", "content": "ignored\n"},
            {"path": "malware.exe", "content": "ignored binary\n"},
        ],
        "ignore_file_contents": "*\n",
    })

    cases.append({
        "name": "ignore-root-skill-md-explicit",
        "category": "root_skill_md_protection",
        "description": "Explicit SKILL.md and /SKILL.md in .skillignore cannot un-scan root SKILL.md",
        "files": [
            {"path": "SKILL.md", "content": "# Protected Root Skill\n"},
            {"path": "doc.txt", "content": "ignored\n"},
        ],
        "ignore_file_contents": "SKILL.md\n/SKILL.md\n*.txt\n",
    })

    cases.append({
        "name": "ignore-root-skill-md-executable-flagged",
        "category": "root_skill_md_protection",
        "description": "Executable root SKILL.md under ignore * still flags unexpected_executable",
        "files": [
            {"path": "SKILL.md", "content": "# Executable Protected Root\n", "executable": True},
            {"path": "script.sh", "content": "echo ignored\n"},
        ],
        "ignore_file_contents": "*\n",
    })

    cases.append({
        "name": "ignore-nested-skill-md-never-ignored",
        "category": "root_skill_md_protection",
        "description": "Nested SKILL.md files are also protected from ignore rules by base name rule",
        "files": [
            {"path": "SKILL.md", "content": "# Root Skill\n"},
            {"path": "sub/SKILL.md", "content": "# Nested Skill\n"},
            {"path": "sub/other.txt", "content": "ignored text\n"},
        ],
        "ignore_file_contents": "*\n",
    })

    # -----------------------------------------------------------------------
    # Group 9: Ignored Trees and Exclusion Rules
    # -----------------------------------------------------------------------
    ignored_tree_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    for i in range(55):
        ignored_tree_files.append({"path": f"ignored_dir/f_{i:02d}.txt", "content": "ignored\n"})
    ignored_tree_files.append({"path": "ignored_dir/bad.exe", "content": "binary\n"})
    ignored_tree_files.append({"path": "ignored_dir/big.dat", "size": 400 * 1024})

    cases.append({
        "name": "ignore-tree-directory-with-threats",
        "category": "ignored_trees",
        "description": "Entire ignored directory tree exempts files, binaries, sizes, and escaping symlinks",
        "files": ignored_tree_files,
        "symlinks": [
            {"path": "ignored_dir/escape_link", "target": "../../outside"}
        ],
        "ignore_file_contents": "ignored_dir/\n",
    })

    clawhub_files = [{"path": "SKILL.md", "content": "# Skill\n"}]
    for i in range(55):
        clawhub_files.append({"path": f"vendor/v_{i:02d}.txt", "content": "vendor\n"})
    cases.append({
        "name": "ignore-tree-clawhubignore-compatibility",
        "category": "ignored_trees",
        "description": "Compatibility with .clawhubignore file excluding directory trees",
        "files": clawhub_files,
        "clawhubignore_contents": "vendor/\n",
    })

    cases.append({
        "name": "ignore-glob-patterns",
        "category": "ignored_trees",
        "description": "Glob patterns (*.tmp, *.bin) correctly ignore matching binary and temporary files",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "scratch.tmp", "content": "temp data\n"},
            {"path": "payload.bin", "content": "binary data\n"},
            {"path": "valid.txt", "content": "kept\n"},
        ],
        "ignore_file_contents": "*.tmp\n*.bin\n",
    })

    cases.append({
        "name": "ignore-anchored-slash",
        "category": "ignored_trees",
        "description": "Leading slash /anchored.bin ignores root file but does not ignore nested file",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "root_only.bin", "content": "ignored at root\n"},
            {"path": "sub/root_only.bin", "content": "not ignored in sub\n"},
        ],
        "ignore_file_contents": "/root_only.bin\n",
    })

    cases.append({
        "name": "ignore-files-themselves-excluded",
        "category": "ignored_trees",
        "description": ".skillignore and .clawhubignore are always excluded from file counts and checks",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "main.py", "content": "print('ok')\n"},
        ],
        "ignore_file_contents": "# Hermetic ignore\n",
        "clawhubignore_contents": "# Clawhub ignore\n",
    })

    cases.append({
        "name": "ignore-unignored-files-flagged",
        "category": "ignored_trees",
        "description": "Ignored docs directory exempts docs, but unignored src/malware.exe is detected",
        "files": [
            {"path": "SKILL.md", "content": "# Skill\n"},
            {"path": "docs/notes.txt", "content": "doc\n"},
            {"path": "docs/old.exe", "content": "ignored binary\n"},
            {"path": "src/malware.exe", "content": "active binary\n"},
        ],
        "ignore_file_contents": "docs/\n",
    })

    # Verify case name uniqueness
    seen_names: set[str] = set()
    for case in cases:
        cname = case["name"]
        if cname in seen_names:
            raise ValueError(f"Duplicate test case name: {cname}")
        seen_names.add(cname)

    return cases


def materialize_and_evaluate(case_raw: dict[str, Any]) -> dict[str, Any]:
    name = case_raw["name"]
    category = case_raw["category"]
    description = case_raw["description"]
    files_input = case_raw.get("files", [])
    symlinks_input = case_raw.get("symlinks", [])
    ignore_content = case_raw.get("ignore_file_contents")
    clawhubignore_content = case_raw.get("clawhubignore_contents")

    with tempfile.TemporaryDirectory(prefix="skill-struct-") as temp_dir_str:
        temp_root = Path(temp_dir_str)
        skill_dir = temp_root / "skill"
        skill_dir.mkdir(parents=True, exist_ok=True)

        outside_dir = temp_root / "outside"
        outside_dir.mkdir(parents=True, exist_ok=True)
        (outside_dir / "secret.txt").write_text("secret outside data\n", encoding="utf-8")
        (outside_dir / "target.txt").write_text("target outside data\n", encoding="utf-8")

        sibling_backdoor = temp_root / "skill-backdoor"
        sibling_backdoor.mkdir(parents=True, exist_ok=True)
        (sibling_backdoor / "malicious.py").write_text("evil()\n", encoding="utf-8")

        if ignore_content is not None:
            (skill_dir / ".skillignore").write_text(ignore_content, encoding="utf-8")
        if clawhubignore_content is not None:
            (skill_dir / ".clawhubignore").write_text(clawhubignore_content, encoding="utf-8")

        serialized_files: list[dict[str, Any]] = []
        for file_spec in files_input:
            rel_path = file_spec["path"]
            content = file_spec.get("content")
            size = file_spec.get("size")
            mode = file_spec.get("mode")
            executable = file_spec.get("executable", False)

            file_path = skill_dir / rel_path
            file_path.parent.mkdir(parents=True, exist_ok=True)

            if content is not None:
                encoded = content.encode("utf-8")
                file_path.write_bytes(encoded)
                actual_size = len(encoded)
            elif size is not None:
                with open(file_path, "wb") as f:
                    if size > 0:
                        f.seek(size - 1)
                        f.write(b"\0")
                actual_size = size
            else:
                file_path.write_bytes(b"")
                actual_size = 0

            if mode is not None:
                os.chmod(file_path, mode)
                executable = bool(mode & 0o111)
            elif executable:
                os.chmod(file_path, 0o755)
            else:
                os.chmod(file_path, 0o644)

            serialized_files.append({
                "path": rel_path,
                "content": content,
                "size": actual_size,
                "executable": executable,
            })

        serialized_symlinks: list[dict[str, Any]] = []
        for symlink_spec in symlinks_input:
            rel_path = symlink_spec.get("path") or symlink_spec.get("relative_path")
            target = symlink_spec["target"]

            link_path = skill_dir / rel_path
            link_path.parent.mkdir(parents=True, exist_ok=True)
            link_path.symlink_to(target)

            serialized_symlinks.append({
                "path": rel_path,
                "relative_path": rel_path,
                "target": target,
            })

        ignore = sg._load_skill_ignore(skill_dir)
        raw_findings = sg._check_structure(skill_dir, ignore=ignore)

        temp_root_resolved = str(temp_root.resolve())
        temp_root_raw = str(temp_root)

        normalized_findings: list[dict[str, Any]] = []
        for finding in raw_findings:
            match_str = finding.match
            match_str = match_str.replace(temp_root_resolved, TEMP_DIR_PLACEHOLDER)
            if temp_root_raw != temp_root_resolved:
                match_str = match_str.replace(temp_root_raw, TEMP_DIR_PLACEHOLDER)

            normalized_findings.append({
                "pattern_id": finding.pattern_id,
                "severity": finding.severity,
                "category": finding.category,
                "file": finding.file,
                "line": finding.line,
                "match": match_str,
                "description": finding.description,
            })

        normalized_findings.sort(key=finding_sort_key)

        return {
            "name": name,
            "category": category,
            "description": description,
            "files": serialized_files,
            "symlinks": serialized_symlinks,
            "ignore_file_contents": ignore_content,
            "clawhubignore_contents": clawhubignore_content,
            "expected_findings": normalized_findings,
            "expected": normalized_findings,
            "ordering_normalized": True,
        }


def generate_all_goldens() -> dict[str, Any]:
    raw_cases = build_raw_cases()
    evaluated_cases = [materialize_and_evaluate(c) for c in raw_cases]

    return {
        "generator": "rust/tools/gen_skill_structure_goldens.py",
        "description": "Golden reference cases for tools/skills_guard.py _check_structure",
        "version": "1.0",
        "placeholder": TEMP_DIR_PLACEHOLDER,
        "schema": SCHEMA_DEFINITION,
        "exclusions": EXCLUSIONS_DEFINITION,
        "constants": {
            "max_file_count": sg.MAX_FILE_COUNT,
            "max_single_file_kb": sg.MAX_SINGLE_FILE_KB,
            "max_total_size_kb": sg.MAX_TOTAL_SIZE_KB,
            "suspicious_binary_extensions": sorted(list(sg.SUSPICIOUS_BINARY_EXTENSIONS)),
            "always_ignored_names": sorted(list(sg._ALWAYS_IGNORED_NAMES)),
            "never_ignorable": sorted(list(sg._NEVER_IGNORABLE)),
        },
        "ordering_normalization": {
            "rule": "Per-file findings are sorted alphabetically by relative path, then by Python source pattern evaluation order; directory findings follow at the end in Python source evaluation order.",
            "source_order": [
                "symlink_escape",
                "broken_symlink",
                "oversized_file",
                "binary_file",
                "unexpected_executable",
                "too_many_files",
                "oversized_skill",
            ],
        },
        "total_cases": len(evaluated_cases),
        "cases": evaluated_cases,
    }


def report_schema_and_exclusions(data: dict[str, Any]) -> None:
    cases = data["cases"]
    categories = sorted({c["category"] for c in cases})
    findings_count = sum(len(c["expected_findings"]) for c in cases)

    print("=" * 70)
    print("Skill Structure Golden Cases Report")
    print("=" * 70)
    print(f"Total Cases: {len(cases)}")
    print(f"Total Expected Findings: {findings_count}")
    print(f"Categories ({len(categories)}): {', '.join(categories)}")
    print("-" * 70)
    print("Schema Definition:")
    print("  Case Fields:")
    for field_name, desc in data["schema"]["case"].items():
        print(f"    - {field_name}: {desc}")
    print("  File Item Fields:")
    for field_name, desc in data["schema"]["file_item"].items():
        print(f"    - {field_name}: {desc}")
    print("  Symlink Item Fields:")
    for field_name, desc in data["schema"]["symlink_item"].items():
        print(f"    - {field_name}: {desc}")
    print("  Finding Fields:")
    for field_name, desc in data["schema"]["finding"].items():
        print(f"    - {field_name}: {desc}")
    print("-" * 70)
    print("Exclusions:")
    for exc in data["exclusions"]:
        print(f"  - {exc}")
    print("-" * 70)
    print("Category Breakdown:")
    for cat in categories:
        cat_cases = [c for c in cases if c["category"] == cat]
        cat_findings = sum(len(c["expected_findings"]) for c in cat_cases)
        print(f"  {cat.ljust(26)}: {len(cat_cases):2d} cases, {cat_findings:2d} findings")
    print("=" * 70)


def main() -> None:
    if "--help" in sys.argv or "-h" in sys.argv:
        print("Usage: gen_skill_structure_goldens.py [--check] [--report]")
        sys.exit(0)

    goldens_data = generate_all_goldens()
    content = json.dumps(goldens_data, indent=2, ensure_ascii=True) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not OUT_GOLDENS.exists():
            raise SystemExit(f"Missing goldens file: {OUT_GOLDENS}")

        actual = OUT_GOLDENS.read_text(encoding="utf-8")
        if actual != content:
            raise SystemExit("Skill structure goldens differ from Python oracle")

        report_schema_and_exclusions(goldens_data)
        print(f"Verified {len(goldens_data['cases'])} skill structure cases against Python oracle")
    elif sys.argv[1:] in ([], ["--report"]):
        OUT_GOLDENS.write_text(content, encoding="utf-8")
        report_schema_and_exclusions(goldens_data)
        print(f"Generated {len(goldens_data['cases'])} skill structure cases to {OUT_GOLDENS}")
    else:
        raise SystemExit("Usage: gen_skill_structure_goldens.py [--check] [--report]")


if __name__ == "__main__":
    main()
