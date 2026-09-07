"""Extract the protocol's fixed text from the actual Python renderer."""
import json
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools import bot_mode_probe as probe

with patch.multiple(
    probe,
    _roster=lambda root: [("default", root)],
    _is_bot_managed=lambda path: True,
    _soul_has_protocol=lambda path: False,
    _handle=lambda name: "__HANDLE__",
    _roster_lines=lambda root, me: ["__ROSTER__"],
    _remote_paragraph=lambda root: "",
    _peer_paragraph=lambda root: "",
):
    text = probe._build_section(Path("/example"))
prefix, remaining = text.split("__HANDLE__")
middle, suffix = remaining.split("__ROSTER__")
rendered = json.dumps([prefix, middle, suffix], ensure_ascii=True, indent=2) + "\n"
target = Path(__file__).with_name("bot-protocol-template.json")
if "--check" in sys.argv:
    assert target.read_text() == rendered, "Bot protocol template differs from Python"
else:
    target.write_text(rendered)
print("Bot protocol template matches Python")

cases = []
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    worker = root / "profiles" / "worker"
    worker.mkdir(parents=True)
    for managed in (False, True):
        (worker / "profile.yaml").write_text(
            "ui_meta:\n  hermes-bots: {}\ndescription: Worker role\n"
            if managed else "description: Worker role\n"
        )
        for name, home in (("default", root), ("worker", worker)):
            for legacy in (False, True):
                (home / "SOUL.md").write_text(probe._PROTOCOL_HEADING if legacy else "Persona")
                for peers in ([], ["alpha", "beta"]):
                    remote = "\n\nRemote roster snapshot" if peers else ""
                    with patch.object(probe, "_remote_paragraph", return_value=remote), patch.object(probe, "_peers", return_value=peers):
                        expected = probe._build_section(home)
                    cases.append(dict(managed=managed, name=name, legacy=legacy, peers=peers, remote=remote, expected=expected))
                (home / "SOUL.md").unlink()
target = target.with_name("bot-protocol-goldens.json")
rendered = json.dumps(cases, ensure_ascii=True, indent=2) + "\n"
if "--check" in sys.argv:
    assert target.read_text() == rendered, "Bot protocol cases differ from Python"
else:
    target.write_text(rendered)
print(f"Bot protocol: {len(cases)} Python cases")

remote_cases = []
from tools.bot_relay import read_remote_roster
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    (root / "bot_relay").mkdir()
    (root / "profile.yaml").write_text("ui_meta:\n  hermes-bots: {}\n")
    (root / "config.yaml").write_text("bot_peers:\n  beta: {}\n  alpha: {}\n")
    for rows in [[], [None, {}, {"profile": "bad/name", "connection_id": "a"}],
                 [{"profile": "default", "connection_id": "a", "online": False}],
                 [{"profile": "worker", "handle": "@Same", "connection_id": "a", "title": "A", "description": "line\n two"},
                  {"profile": "other", "handle": "same", "connection_id": "b", "connection_label": "Laptop"}],
                 [{"profile": 123, "connection_id": True, "title": [1, 2], "description": "é" * 170}]]:
        (root / "bot_relay/roster.json").write_text(json.dumps({"agents": rows}))
        remote_cases.append(dict(rows=rows, normalized=read_remote_roster(root), expected=probe._build_section(root)))
target = target.with_name("bot-remote-goldens.json")
rendered = json.dumps(remote_cases, ensure_ascii=True, indent=2) + "\n"
if "--check" in sys.argv:
    assert target.read_text() == rendered, "Bot remote cases differ from Python"
else:
    target.write_text(rendered)
print(f"Bot remote roster: {len(remote_cases)} Python cases")

epoch_cases = []
for stored in ["", "ordinary prompt", "Capability epoch: unavailable",
               "Capability epoch: ABCDEF123456", "Capability epoch: abcdef123456",
               "Capability epoch: abcdef1234567", "Capability epoch: abcdef12345",
               "Capability epoch: abcdef123456\nCapability epoch: 000000000000",
               "prefix Capability epoch: 000000000000 suffix"]:
    for current in ["abcdef123456", "000000000000", "unavailable", "", None]:
        with patch.object(probe, "capability_fingerprint", side_effect=RuntimeError("probe failed") if current is None else None, return_value=current):
            epoch_cases.append(dict(stored=stored, current=current, expected=probe.stored_prompt_capability_stale(stored)))
target = target.with_name("bot-epoch-goldens.json")
rendered = json.dumps(epoch_cases, indent=2) + "\n"
if "--check" in sys.argv:
    assert target.read_text() == rendered, "Bot epoch cases differ from Python"
else:
    target.write_text(rendered)
print(f"Bot epoch comparison: {len(epoch_cases)} Python cases")
