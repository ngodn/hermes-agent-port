#!/usr/bin/env python3
"""Execute SessionStore's reset predicate without loading the gateway runtime."""
import ast
import itertools
import json
import sys
from datetime import datetime, timedelta
from pathlib import Path
from types import SimpleNamespace

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / 'gateway/session.py').read_text())
method = next(n for n in ast.walk(tree) if isinstance(n, ast.FunctionDef) and n.name == '_should_reset')
namespace = dict(SessionEntry=object, SessionSource=object, Optional=__import__('typing').Optional,
                 timedelta=timedelta, logger=SimpleNamespace(debug=lambda *a: None))
exec(compile(ast.Module(body=[method], type_ignores=[]), 'session-reset-source', 'exec'), namespace)
policies = [dict(mode=mode, idle_minutes=60, at_hour=4) for mode in ['none', 'idle', 'daily', 'both', 'unknown', []]]
policies += [dict(mode='both', idle_minutes=idle, at_hour=hour) for idle, hour in [(0,4), (-1,4), (1.5,4), ('1',4), (60,24), (60,'4'), (60,4.0), (True,True)]]
rows = []
now = datetime(2026, 9, 6, 4)
for policy, delta, active, before_hour in itertools.product(policies, [timedelta(), timedelta(microseconds=1), timedelta(minutes=60), timedelta(minutes=60, microseconds=1), timedelta(days=2)], [False, True], [False, True]):
    clock = now - timedelta(hours=1) if before_hour else now
    updated = clock - delta
    namespace['_now'] = lambda: clock
    oracle = SimpleNamespace(_generate_session_key=lambda source:'key',
        _has_active_processes_safe=lambda *a, **kw:active,
        config=SimpleNamespace(get_reset_policy=lambda **kw:SimpleNamespace(**policy)))
    result, error = None, False
    try:
        result = namespace['_should_reset'](oracle, SimpleNamespace(updated_at=updated), SimpleNamespace(platform='slack', chat_type='group'))
    except (TypeError, ValueError, OverflowError):
        error = True
    rows.append(dict(policy=policy, updated=updated.isoformat(), now=clock.isoformat(), active=active, result=result, error=error))
for policy, clock in [
    (dict(mode='daily', at_hour=4, idle_minutes=60), datetime(1, 1, 1)),
    (dict(mode='idle', at_hour=4, idle_minutes=60), datetime(9999, 12, 31, 23, 59)),
    (dict(mode='idle', at_hour=4, idle_minutes=-60), datetime(1, 1, 1)),
]:
    oracle.config = SimpleNamespace(get_reset_policy=lambda **kw:SimpleNamespace(**policy))
    oracle._has_active_processes_safe = lambda *a, **kw:False
    namespace['_now'] = lambda: clock
    result, error = None, False
    try:
        result = namespace['_should_reset'](oracle, SimpleNamespace(updated_at=clock), SimpleNamespace(platform='slack', chat_type='group'))
    except (TypeError, ValueError, OverflowError):
        error = True
    rows.append(dict(policy=policy, updated=clock.isoformat(), now=clock.isoformat(), active=False, result=result, error=error))
path = root / 'rust/tools/session-reset-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit('usage: gen_session_reset_goldens.py [--check]')
print(f'Verified {len(rows)} session reset decisions')

# Run the registry method itself against refreshed in-memory entries. The OS
# refresh implementation is outside this predicate's port boundary.
from contextlib import nullcontext
registry_tree = ast.parse((root / 'tools/process_registry.py').read_text())
method = next(n for n in ast.walk(registry_tree) if isinstance(n, ast.FunctionDef) and n.name == 'has_active_for_session')
namespace['time'] = SimpleNamespace(time=lambda:100.0)
exec(compile(ast.Module(body=[method], type_ignores=[]), 'process-age-source', 'exec'), namespace)
processes = []
for owner, exited, started, limit in itertools.product(['key', 'other'], [False, True], [-100.0, 0.0, 99.0, 100.0, 101.0], [None, -1.0, 0.0, 1.0, 100.0]):
    refreshed = []
    process = SimpleNamespace(session_key=owner, exited=exited, started_at=started)
    oracle = SimpleNamespace(_lock=nullcontext(), _running={'p':process}, _refresh_detached_session=refreshed.append)
    result = namespace['has_active_for_session'](oracle, 'key', max_active_age=limit)
    assert refreshed == [process]
    processes.append(dict(process_session=owner, exited=exited, started=started, limit=limit, result=result))
run_tree = ast.parse((root / 'gateway/run.py').read_text())
assignment = next(n for n in ast.walk(run_tree) if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == '_bg_max_age_seconds' for t in n.targets))
expression = compile(ast.Expression(assignment.value), 'process-age-config-source', 'eval')
settings = []
for hours in [None, False, True, 0, -1, 0.5, 24, '', '24', [], [1], {}, {'x':1}]:
    result, error = None, False
    try:
        result = eval(expression, {'_bg_max_age_hours':hours})
    except (TypeError, ValueError):
        error = True
    settings.append(dict(hours=hours, result=result, error=error))
path = root / 'rust/tools/session-process-age-goldens.json'
text = json.dumps(dict(processes=processes, settings=settings), indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(processes)} process age checks and {len(settings)} settings')
