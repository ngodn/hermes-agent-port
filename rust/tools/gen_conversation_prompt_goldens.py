#!/usr/bin/env python3
"""Capture stored-prompt restore decisions from the actual Python helper."""
from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

import agent.conversation_loop as loop
import agent.system_prompt as system_prompt
import hermes_cli.lifecycle as lifecycle
import agent.credits_tracker as credits_tracker
import tools.mcp_tool as mcp_tool
import tools.bot_mode_probe as bot_probe

OUTPUT = ROOT / "rust/tools/conversation-prompt-goldens.json"


class Db:
    def __init__(self, row):
        self.row = row
        self.reads = 0
        self.writes = []

    def get_session(self, session_id):
        self.reads += 1
        return self.row

    def get_session_title(self, session_id):
        return (self.row or {}).get("title")

    def update_system_prompt(self, session_id, prompt):
        self.writes.append([session_id, prompt])


def run_case(spec):
    db = Db(spec.get("row"))
    builds = []
    restored = []
    reconstructed = []
    agent = SimpleNamespace(
        _session_db=db,
        session_id="session-1",
        model=spec.get("model", "model-a"),
        provider=spec.get("provider", "provider-a"),
        platform=spec.get("platform", "cli"),
        _bot_mode_protocol=spec.get("bot_protocol", True),
        _session_title_hint=spec.get("title_hint", ""),
        _cached_system_prompt=None,
        _build_system_prompt=lambda message: builds.append(message) or "fresh prompt",
    )
    saved = {
        "resolve_agent_cwd": loop.resolve_agent_cwd,
        "restore_plugin_prompt_sections": system_prompt.restore_plugin_prompt_sections,
        "reconstruct_static_prefix": system_prompt.reconstruct_static_prefix,
        "invoke_hook": lifecycle.invoke_hook,
        "seed_credits_at_session_start": credits_tracker.seed_credits_at_session_start,
        "persist_agent_tool_names": mcp_tool.persist_agent_tool_names,
        "stored_prompt_capability_stale": bot_probe.stored_prompt_capability_stale,
        "stored_bot_chat_prompt_needs_upgrade": bot_probe.stored_bot_chat_prompt_needs_upgrade,
    }
    loop.resolve_agent_cwd = lambda: spec.get("cwd", "/work")
    system_prompt.restore_plugin_prompt_sections = lambda obj, prompt: restored.append(prompt)
    system_prompt.reconstruct_static_prefix = lambda obj, system_message=None: reconstructed.append(system_message)
    lifecycle.invoke_hook = lambda *args, **kwargs: None
    credits_tracker.seed_credits_at_session_start = lambda obj: None
    mcp_tool.persist_agent_tool_names = lambda obj: None
    bot_probe.stored_prompt_capability_stale = lambda prompt, home=None: spec.get("capability_stale", False)
    bot_probe.stored_bot_chat_prompt_needs_upgrade = lambda prompt, home=None: spec.get("legacy_upgrade", False)
    try:
        loop._restore_or_build_system_prompt(
            agent,
            spec.get("system_message"),
            [{"role": "user", "content": "old"}] if spec.get("history") else [],
        )
    finally:
        loop.resolve_agent_cwd = saved["resolve_agent_cwd"]
        system_prompt.restore_plugin_prompt_sections = saved["restore_plugin_prompt_sections"]
        system_prompt.reconstruct_static_prefix = saved["reconstruct_static_prefix"]
        lifecycle.invoke_hook = saved["invoke_hook"]
        credits_tracker.seed_credits_at_session_start = saved["seed_credits_at_session_start"]
        mcp_tool.persist_agent_tool_names = saved["persist_agent_tool_names"]
        bot_probe.stored_prompt_capability_stale = saved["stored_prompt_capability_stale"]
        bot_probe.stored_bot_chat_prompt_needs_upgrade = saved["stored_bot_chat_prompt_needs_upgrade"]
    return {
        "prompt": agent._cached_system_prompt,
        "reads": db.reads,
        "writes": db.writes,
        "builds": builds,
        "restored": restored,
        "reconstructed": reconstructed,
    }


def generate():
    valid = "User home directory: /home/u\nCurrent working directory: /work\nModel: model-a\nProvider: provider-a\nPlatform: cli"
    cases = [
        {"name": "first-turn-ignores-row", "history": False, "row": {"system_prompt": valid}},
        {"name": "missing-row", "history": True, "row": None},
        {"name": "null-prompt", "history": True, "row": {"system_prompt": None}},
        {"name": "empty-prompt", "history": True, "row": {"system_prompt": ""}},
        {"name": "valid-reuse", "history": True, "row": {"system_prompt": valid}},
        {"name": "stale-model", "history": True, "row": {"system_prompt": valid}, "model": "model-b"},
        {"name": "stale-provider", "history": True, "row": {"system_prompt": valid}, "provider": "provider-b"},
        {"name": "stale-platform", "history": True, "row": {"system_prompt": valid}, "platform": "telegram"},
        {"name": "stale-cwd", "history": True, "row": {"system_prompt": valid}, "cwd": "/elsewhere"},
        {"name": "capability-stale", "history": True, "row": {"system_prompt": valid}, "capability_stale": True},
        {"name": "bot-legacy-upgrade", "history": True, "row": {"system_prompt": valid, "title": "Bot Chat"}, "legacy_upgrade": True},
        {"name": "ordinary-legacy-reuses", "history": True, "row": {"system_prompt": valid, "title": "ordinary"}, "legacy_upgrade": True},
    ]
    return [{**case, "expected": run_case(case)} for case in cases]


if __name__ == "__main__":
    text = json.dumps(generate(), indent=2, ensure_ascii=True) + "\n"
    if "--check" in sys.argv:
        assert OUTPUT.read_text() == text, "conversation prompt goldens differ"
    else:
        OUTPUT.write_text(text)
