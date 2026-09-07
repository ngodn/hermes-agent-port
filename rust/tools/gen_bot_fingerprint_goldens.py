#!/usr/bin/env python3
"""Generate capability fingerprint golden test cases from Python oracle.

Orchestrates capability_fingerprint execution against isolated temporary directories
with mocked config snapshots, covering all dimensions of the Bot Mode capability surface:
- Empty home directories (with loader failure or empty config)
- SOUL.md hashing (ASCII, UTF-8 multibyte, emoji, whitespace)
- Skill discovery (single level, deeply nested subdirectories, ignoring non-SKILL.md)
- Profile roster and role metadata (managed vs unmanaged, title, description, whitespace)
- bot_peers in config.yaml (multi-peer, blank/whitespace key filtering)
- Remote roster in bot_relay/roster.json (valid agents, malformed rows, normalization)
- Malformed and non-iterable disabled/toolsets preserving partial surface
- MCP server configurations with floats and Unicode
- Full combination scenarios
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

# Prefer PyYAML from checkout .venv
venv_lib = REPO_ROOT / ".venv" / "lib"
if venv_lib.is_dir():
    for sp in sorted(venv_lib.glob("python*/site-packages")):
        if str(sp) not in sys.path:
            sys.path.insert(1, str(sp))

import hermes_cli.config
import tools.bot_mode_probe


def build_cases() -> list[dict]:
    """Define input scenarios covering the capability fingerprint surface."""
    return [
        # 1. Empty home directory with config loader failure (config: null)
        {
            "config": None,
            "files": {},
        },
        # 2. Empty home directory with empty config snapshot
        {
            "config": {},
            "files": {},
        },
        # 3. SOUL.md with standard ASCII text
        {
            "config": {},
            "files": {
                "SOUL.md": "# Bot Soul\nYou are Hermes, an AI assistant.\n",
            },
        },
        # 4. SOUL.md with UTF-8 Unicode, CJK characters, and emojis
        {
            "config": {},
            "files": {
                "SOUL.md": "# 🤖 Persona\nこんにちは 世界！\n\nCafé au lait & emojis: 🌟🚀\n",
            },
        },
        # 5. Single top-level skill directory
        {
            "config": {},
            "files": {
                "skills/weather/SKILL.md": "---\nname: weather\ndescription: Get weather\n---\n",
            },
        },
        # 6. Deeply nested skill subdirectories
        {
            "config": {},
            "files": {
                "skills/cloud/aws/s3/SKILL.md": "---\nname: s3\n---\n",
                "skills/dev/git/commit/SKILL.md": "---\nname: commit\n---\n",
            },
        },
        # 7. Mixed skill depths while ignoring non-SKILL.md files
        {
            "config": {},
            "files": {
                "skills/alpha/SKILL.md": "---\nname: alpha\n---\n",
                "skills/tools/editor/SKILL.md": "---\nname: editor\n---\n",
                "skills/tools/editor/README.md": "# Readme\nNot a skill file\n",
                "skills/ignored_dir/notes.txt": "Informational notes only\n",
            },
        },
        # 8. Root default profile managed with Hermes-bots title and description
        {
            "config": {},
            "files": {
                "profile.yaml": "ui_meta:\n  hermes-bots:\n    title: Primary Agent\ndescription: Core system assistant\n",
            },
        },
        # 9. Named profile managed with Hermes-bots title and description
        {
            "config": {},
            "files": {
                "profiles/researcher/profile.yaml": "ui_meta:\n  hermes-bots:\n    title: Research Buddy\ndescription: Deep research and literature review\n",
            },
        },
        # 10. Multiple named profiles: managed, unmanaged, title-only, desc-only
        {
            "config": {},
            "files": {
                "profiles/coder/profile.yaml": "ui_meta:\n  hermes-bots:\n    title: Code Specialist\n",
                "profiles/reviewer/profile.yaml": "description: Code reviewer without bot title\n",
                "profiles/bot_plain/profile.yaml": "ui_meta:\n  hermes-bots: {}\n",
            },
        },
        # 11. Profile role whitespace and newline normalization
        {
            "config": {},
            "files": {
                "profiles/analyst/profile.yaml": 'ui_meta:\n  hermes-bots:\n    title: "  Data   Analyst  "\ndescription: "  Analyzes   financial\n\n  reports  and data.  "\n',
            },
        },
        # 12. bot_peers registered in config.yaml
        {
            "config": {},
            "files": {
                "config.yaml": "bot_peers:\n  spark:\n    url: http://spark.lan:8377\n  homelab:\n    url: http://homelab.lan:8377\n",
            },
        },
        # 13. bot_peers in config.yaml filtering empty and whitespace-only keys
        {
            "config": {},
            "files": {
                "config.yaml": 'bot_peers:\n  "": {}\n  "   ": {}\n  cluster_a:\n    url: http://cluster\n',
            },
        },
        # 14. Valid remote roster in bot_relay/roster.json
        {
            "config": {},
            "files": {
                "bot_relay/roster.json": json.dumps({
                    "updated_at": 1700000000,
                    "agents": [
                        {
                            "connection_id": "remote-node-1",
                            "profile": "assistant",
                            "handle": "assistant",
                            "title": "Remote Assistant",
                            "description": "Cloud worker",
                        },
                        {
                            "connection_id": "remote-node-2",
                            "profile": "scraper",
                            "handle": "scraper",
                            "title": "Web Scraper",
                            "description": "Fetches data",
                        },
                    ],
                }),
            },
        },
        # 15. Remote roster with invalid/filtered rows and Unicode titles/descriptions
        {
            "config": {},
            "files": {
                "bot_relay/roster.json": json.dumps({
                    "agents": [
                        None,
                        {"bad": "row"},
                        {
                            "connection_id": "conn1",
                            "profile": "agent_valid",
                            "title": "エージェント 1",
                            "description": "日本語",
                        },
                        {
                            "connection_id": "bad/id",
                            "profile": "agent2",
                            "title": "Invalid handle/conn",
                        },
                    ],
                }),
            },
        },
        # 16. Malformed non-iterable disabled skills drops config surface
        {
            "config": {
                "skills": {"disabled": 123},
                "tools": {"enabled_toolsets": ["coding"]},
            },
            "files": {},
        },
        # 17. Malformed non-iterable enabled_toolsets preserves partial surface (disabled_skills kept)
        {
            "config": {
                "skills": {"disabled": ["Web", "BASH"]},
                "tools": {"enabled_toolsets": 42},
            },
            "files": {},
        },
        # 18. Non-dict skills section with valid toolsets
        {
            "config": {
                "skills": "not_a_dict",
                "tools": {"enabled_toolsets": ["web", "bash", "cli"]},
            },
            "files": {},
        },
        # 19. Non-dict mcp_servers with valid disabled skills and toolsets
        {
            "config": {
                "skills": {"disabled": ["telemetry"]},
                "tools": {"enabled_toolsets": ["fs"]},
                "mcp_servers": "invalid_servers_type",
            },
            "files": {},
        },
        # 20. Unicode and float values in MCP servers
        {
            "config": {
                "mcp_servers": {
                    "weather_service": {
                        "url": "https://api.weather.example/v1",
                        "timeout": 12.5,
                        "threshold": 0.001,
                        "location": "東京 (Tokyo)",
                        "temperature_unit": "摂氏 (Celsius)",
                        "icon": "🌤️",
                    },
                },
            },
            "files": {},
        },
        # 21. Diverse float representations and multilingual Unicode in MCP config
        {
            "config": {
                "mcp_servers": {
                    "calc": {
                        "pi": 3.141592653589793,
                        "zero": 0.0,
                        "negative": -12.75,
                        "enabled": True,
                        "labels": ["München", "São Paulo", "北京"],
                        "nested": {
                            "ratio": 1.5e-3,
                            "note": "naïve café",
                        },
                    },
                },
            },
            "files": {},
        },
        # 22. Comprehensive combined integration scenario
        {
            "config": {
                "skills": {"disabled": ["Legacy_Skill", "Alpha_Test"]},
                "tools": {"enabled_toolsets": ["web", "developer"]},
                "mcp_servers": {
                    "custom_mcp": {
                        "command": "/usr/local/bin/mcp-server",
                        "score": 99.9,
                        "metadata": {"author": "François", "version": 2.1},
                    },
                },
            },
            "files": {
                "SOUL.md": "# Full Persona\nActive capability test.\n",
                "skills/infra/terraform/SKILL.md": "---\nname: terraform\n---\n",
                "skills/dev/lint/SKILL.md": "---\nname: lint\n---\n",
                "profile.yaml": "ui_meta:\n  hermes-bots:\n    title: Lead Bot\ndescription: Team leader\n",
                "profiles/subordinate/profile.yaml": "ui_meta:\n  hermes-bots:\n    title: Assistant\ndescription: Helper\n",
                "config.yaml": "bot_peers:\n  gateway_east: {}\n  gateway_west: {}\n",
                "bot_relay/roster.json": json.dumps({
                    "agents": [
                        {
                            "connection_id": "gw1",
                            "profile": "agent_alpha",
                            "title": "Alpha Agent",
                            "description": "Worker on GW1",
                        },
                    ],
                }),
            },
        },
    ]


def generate() -> str:
    """Execute each scenario against capability_fingerprint oracle."""
    raw_cases = build_cases()
    goldens = []
    for c in raw_cases:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            for rel_path, content in c["files"].items():
                dest = root / rel_path
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(content, encoding="utf-8")

            cfg = c["config"]
            if cfg is None:
                with patch(
                    "hermes_cli.config.load_config_readonly",
                    side_effect=RuntimeError("Config loader failure"),
                ):
                    digest = tools.bot_mode_probe.capability_fingerprint(root)
            else:
                with patch(
                    "hermes_cli.config.load_config_readonly",
                    return_value=cfg,
                ):
                    digest = tools.bot_mode_probe.capability_fingerprint(root)

            goldens.append({
                "config": cfg,
                "files": c["files"],
                "expected": digest,
            })
    return json.dumps(goldens, indent=2, ensure_ascii=False) + "\n"


def main():
    output_path = REPO_ROOT / "rust/tools/bot-fingerprint-goldens.json"
    content = generate()

    if sys.argv[1:] == ["--check"]:
        if not output_path.exists():
            raise SystemExit(f"Golden file does not exist: {output_path}")
        existing = output_path.read_text(encoding="utf-8")
        if existing != content:
            raise SystemExit(
                f"Golden file differs from Python oracle: {output_path}"
            )
        count = len(json.loads(content))
        print(f"Verified {count} bot fingerprint cases match oracle")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_bot_fingerprint_goldens.py [--check]")
    else:
        output_path.write_text(content, encoding="utf-8")
        count = len(json.loads(content))
        print(f"Generated {count} bot fingerprint cases to {output_path}")


if __name__ == "__main__":
    main()
