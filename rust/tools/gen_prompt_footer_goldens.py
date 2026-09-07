"""Execute Python's actual footer block with captured display-zone dates."""
import ast
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
tree = ast.parse((ROOT / "agent/system_prompt.py").read_text())
function = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "build_system_prompt_parts")
begin = next(i for i, n in enumerate(function.body) if isinstance(n, ast.ImportFrom) and n.module == "hermes_time")
end = next(i for i in range(begin, len(function.body)) if isinstance(function.body[i], ast.Return))
# Bind the imported time functions directly, leaving the full rendering block
# unchanged. No dependency on the host clock, timezone, or Hermes config.
code = compile(ast.fix_missing_locations(ast.Module(body=function.body[begin + 1:end], type_ignores=[])), "python-footer", "exec")
cases = []
for iana, zone in [(None, None), (None, timezone.utc), ("UTC", timezone.utc),
                   ("Asia/Kuala_Lumpur", timezone(timedelta(hours=8), "MYT")),
                   ("America/New_York", timezone(timedelta(hours=-4), "EDT"))]:
    now = datetime(2026, 9, 7, 14, 23, tzinfo=zone)
    for days in (0, 1, 365):
        start = now - timedelta(days=days)
        for timeless in (False, True):
            for pass_id in (False, True):
                for populated in (False, True):
                    agent = SimpleNamespace(
                        _bot_chat_timeless_prompt=timeless, pass_session_id=pass_id,
                        session_id="session" if populated else "",
                        model="provider/model" if populated else "",
                        provider="provider" if populated else "",
                        platform="tui" if populated else "",
                    )
                    namespace = dict(agent=agent, _hermes_now=lambda: now,
                                     _hermes_tz=lambda: SimpleNamespace(key=iana),
                                     _session_start_like=lambda agent, now: start, volatile_parts=[])
                    exec(code, namespace)
                    inputs = dict(now_date=now.date().isoformat(), start_date=start.date().isoformat(),
                                  iana=iana, abbreviation=now.strftime("%Z"), offset=now.strftime("%z"),
                                  timeless=timeless, pass_session_id=pass_id,
                                  session_id=agent.session_id, model=agent.model,
                                  provider=agent.provider, platform=agent.platform)
                    cases.append(dict(input=inputs, expected=namespace["volatile_parts"][0]))
target = Path(__file__).with_name("prompt-footer-goldens.json")
rendered = json.dumps(cases, indent=2, ensure_ascii=True) + "\n"
if "--check" in sys.argv:
    assert target.read_text() == rendered, "Footer differs from Python"
else:
    target.write_text(rendered)
print(f"Python footer: {len(cases)} cases")

import re
start_function = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "_session_start_like")
# Supply the captured machine offset directly; preserve selection/conversion.
start_function.body = [n for n in start_function.body if not (isinstance(n, ast.Try) and "machine_local_tz = datetime.now()" in ast.unparse(n))]
start_code = compile(ast.fix_missing_locations(ast.Module(body=[start_function], type_ignores=[])), "python-session-start", "exec")
cases = []
for root_id in (None, "invalid", "20250101_230000_old", "２０２６0907_010203"):
    for session_id in ("", "20260907_235959_tip", "20260230_000000_bad", "20260917_0１0２0３_suffix", "20260907_235960_bad"):
        for offset in (None, -18000, 28800):
            for creation in (None, "2024-12-31T23:30:00+00:00", "2024-12-31T23:30:00"):
                now = datetime(2026, 9, 8, 2, tzinfo=timezone(timedelta(hours=8)))
                ns = dict(Any=object, re=re, machine_local_tz=timezone(timedelta(seconds=offset)) if offset is not None else None)
                exec(start_code, ns)
                agent = SimpleNamespace(session_id=session_id, session_start=datetime.fromisoformat(creation) if creation else None,
                                        _session_db=SimpleNamespace(get_conversation_root=lambda sid: root_id))
                result = ns["_session_start_like"](agent, now)
                cases.append(dict(root=root_id, session=session_id, creation=creation, machine_offset=offset,
                                  now=now.isoformat(), expected=result.date().isoformat()))
target = target.with_name("session-start-date-goldens.json")
rendered = json.dumps(cases, indent=2, ensure_ascii=True) + "\n"
if "--check" in sys.argv:
    assert target.read_text() == rendered, "Session start differs from Python"
else:
    target.write_text(rendered)
print(f"Python session start: {len(cases)} cases")
