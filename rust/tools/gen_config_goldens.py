#!/usr/bin/env python3
"""Generate differential goldens for the gateway config loader port.

Writes one directory per fixture under rust/tools/config-goldens/<name>/ holding
the input files (config.yaml / gateway.json) plus expected.json, which is the
real Python `load_gateway_config().to_dict()` for that HERMES_HOME.

Each fixture runs in a CLEAN environment (env -i + HERMES_HOME + PATH) so the
env-override layer does not pollute the expected output, unless the fixture
declares an explicit `env` dict.

Usage: python3 rust/tools/gen_config_goldens.py
"""
import json
import os
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust" / "tools" / "config-goldens"

# name -> (files, env-overrides)
FIXTURES: dict[str, tuple[dict[str, str], dict[str, str]]] = {
    "empty": ({}, {}),
    "gateway_json_only": (
        {"gateway.json": json.dumps({"write_sessions_json": False, "reset_triggers": ["/x"]})},
        {},
    ),
    "yaml_only": (
        {"config.yaml": "write_sessions_json: false\nalways_log_local: false\n"},
        {},
    ),
    "yaml_beats_gateway_json": (
        {
            "gateway.json": json.dumps({"write_sessions_json": True, "session_store_max_age_days": 5}),
            "config.yaml": "write_sessions_json: false\n",
        },
        {},
    ),
    "toplevel_beats_nested": (
        {"config.yaml": "write_sessions_json: false\ngateway:\n  write_sessions_json: true\n"},
        {},
    ),
    "nested_only": (
        {"config.yaml": "gateway:\n  write_sessions_json: false\n  max_concurrent_sessions: 7\n"},
        {},
    ),
    "platform_enabled_explicit": (
        {"config.yaml": "platforms:\n  telegram:\n    enabled: false\n"},
        {},
    ),
    "nested_gateway_platforms": (
        {"config.yaml": "gateway:\n  platforms:\n    discord:\n      enabled: true\n"},
        {},
    ),
    "gateway_platform_subsection": (
        {"config.yaml": "gateway:\n  api_server:\n    enabled: true\n    port: 8642\n    host: 0.0.0.0\n"},
        {},
    ),
    "api_server_extra_bridge": (
        {"config.yaml": "platforms:\n  api_server:\n    enabled: true\n    port: 8642\n    key: sekret\n    model_name: m1\n"},
        {},
    ),
    "shared_key_bridging": (
        {"config.yaml": (
            "telegram:\n"
            "  allow_from: [alice, bob]\n"
            "  require_mention: true\n"
            "  allowed_chats: [1, 2]\n"
            "  channel_prompts:\n"
            "    123: hello\n"
            "  unauthorized_dm_behavior: IGNORE\n"
        )},
        {},
    ),
    "session_reset_and_streaming": (
        {"config.yaml": (
            "session_reset:\n  mode: idle\n  idle_minutes: 30\n"
            "streaming:\n  enabled: true\n  transport: ws\n  edit_interval: 1.5\n"
        )},
        {},
    ),
    "nested_streaming_fallback": (
        {"config.yaml": "gateway:\n  streaming:\n    mode: edit\n"},
        {},
    ),
    "watchdog_keys": (
        {"config.yaml": "gateway:\n  systemd_watchdog_seconds: 20\n  loop_watchdog: false\n  loop_watchdog_max_strikes: 9\n"},
        {},
    ),
    "quick_commands_invalid": (
        {"config.yaml": "quick_commands: [not, a, mapping]\n"},
        {},
    ),
    "profile_routes": (
        {"config.yaml": (
            "profile_routes:\n"
            "  - {name: g, platform: discord, guild_id: 111, profile: Server}\n"
            "  - {name: t, platform: discord, chat_id: 222, thread_id: 333, profile: MyProfile}\n"
        )},
        {},
    ),
    "env_token_enables_platform": ({}, {"TELEGRAM_BOT_TOKEN": "tok123"}),
    "env_beats_nothing_but_explicit_disable": (
        {"config.yaml": "platforms:\n  telegram:\n    enabled: false\n"},
        {"TELEGRAM_BOT_TOKEN": "tok123"},
    ),
}

DUMP = (
    "import sys, json; sys.path.insert(0, %r);"
    "from gateway.config import load_gateway_config;"
    "print(json.dumps(load_gateway_config().to_dict(), sort_keys=True, default=str, ensure_ascii=False))"
) % str(REPO)


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    failures = []
    for name, (files, env_over) in FIXTURES.items():
        d = OUT / name
        home = d / "home"
        if home.exists():
            for p in sorted(home.rglob("*"), reverse=True):
                p.unlink() if p.is_file() else p.rmdir()
        home.mkdir(parents=True, exist_ok=True)
        for fname, content in files.items():
            (home / fname).write_text(content, encoding="utf-8")

        env = {
            "HERMES_HOME": str(home),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": os.environ.get("HOME", "/tmp"),
        }
        env.update(env_over)
        proc = subprocess.run(
            [sys.executable, "-c", DUMP],
            cwd=str(REPO), env=env, capture_output=True, text=True,
        )
        if proc.returncode != 0:
            failures.append((name, proc.stderr.strip()[-500:]))
            continue
        parsed = json.loads(proc.stdout)
        (d / "expected.json").write_text(
            json.dumps(parsed, sort_keys=True, indent=2, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        (d / "env.json").write_text(
            json.dumps(env_over, sort_keys=True, indent=2) + "\n", encoding="utf-8"
        )
        print("ok  %s" % name)

    for name, err in failures:
        print("FAIL %s: %s" % (name, err), file=sys.stderr)
    print("\n%d fixtures, %d failures" % (len(FIXTURES), len(failures)))
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
