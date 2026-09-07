#!/usr/bin/env python3
"""Run the source SessionEntry codec without importing the gateway runtime.

Platform discovery is disabled in this fixture environment. Built-in enum
normalization and the real SessionSource/SessionEntry methods still execute.
"""
import ast
import dataclasses
import enum
import json
import logging
import sys
import typing
from datetime import datetime
from pathlib import Path

root = Path(__file__).resolve().parents[2]
namespace = dict(vars(typing), dataclass=dataclasses.dataclass, field=dataclasses.field,
                 datetime=datetime, Enum=enum.Enum, logger=logging.getLogger(__name__),
                 _Platform__bundled_plugin_names=set())
config = ast.parse((root / 'gateway/config.py').read_text())
platform = next(n for n in config.body if isinstance(n, ast.ClassDef) and n.name == 'Platform')
exec(compile(ast.Module(body=[platform], type_ignores=[]), 'platform-source', 'exec'), namespace)
source = ast.parse((root / 'gateway/session.py').read_text())
names = {'_is_path_unsafe', '_is_session_key_unsafe', 'sanitize_model_override', 'SessionSource', 'SessionEntry'}
nodes = [n for n in source.body if getattr(n, 'name', '') in names or
         isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'PERSISTABLE_MODEL_OVERRIDE_KEYS' for t in n.targets)]
exec(compile(ast.Module(body=nodes, type_ignores=[]), 'session-entry-source', 'exec'), namespace)
base = dict(session_key='agent:main:slack:group:C1', session_id='session-1',
            created_at='2026-09-06T12:00:00.123456', updated_at='2026-09-06T13:00:00+08:00')
inputs = [base]
for key, values in {
    'created_at': ['0001-01-01', '9999-12-31T23:59:59.999999', '20260906', '2026-W36-7',
                   '2026-09-06 01:02:03.123456789', '2026-09-06T01:02:03Z',
                   '2026-09-06T01:02:03-03:30:12.123456', '2026-09-06T01:02:03+00:00:00.5',
                   None, 0, '', 'bad', '2026-02-30'],
    'session_key': ['agent:main:google_chat:group:spaces/a/threads/b', '../bad', '/absolute', 'C:bad', ''],
    'session_id': ['../bad', 'a/b', 'a\\b', 'C:bad', ''],
    'platform': ['slack', ' SLACK ', 'unknown-test-platform', None, '', 12],
    'metadata': [None, False, 0, '', [], {}, {'watermark':'123.456'}, [['a',1],['a',2]], ['ab'], [['a']], 1, 'bad'],
    'model_override': [None, {}, 'bad', {'api_key':'fixture-secret'},
                       {'model':'test', 'provider':'test', 'base_url':'https://example.test', 'api_key':'fixture-secret', 'api_mode':'secret'},
                       {'model':False, 'provider':12, 'base_url':None}],
    'origin': [None, 'bad', {}, {'platform':'slack'}, {'platform':'unknown-test-platform','chat_id':'C'},
               {'platform':'slack','chat_id':'C','guild_id':'T','delivered_via_upstream_relay':True}],
}.items():
    inputs.extend(dict(base, **{key:value}) for value in values)
for token in [None, '', 'turn-1', ' ', False, 12]:
    for timestamp in [None, '', 'bad', 12, '2026-09-06T12:13:14.000001Z']:
        inputs.append(dict(base, active_turn_token=token, active_turn_started_at=timestamp,
                           last_resume_marked_at=timestamp))
for legacy in [False, True, None]:
    inputs.append(dict(base, memory_flushed=legacy))
    inputs.append(dict(base, memory_flushed=legacy, expiry_finalized=False))
for field in ['input_tokens','output_tokens','total_tokens','estimated_cost_usd','chat_type',
              'suspended','resume_pending','was_auto_reset','is_fresh_reset','prev_session_id']:
    for value in [None, 0, 'old-value']:
        inputs.append(dict(base, **{field:value}))
for key in base:
    inputs.append({k:v for k,v in base.items() if k != key})
rows = []
for data in inputs:
    try:
        result = namespace['SessionEntry'].from_dict(data).to_dict()
        # Persist through JSON so dict sequence keys have their on-disk spelling.
        result = json.loads(json.dumps(result))
        rows.append(dict(input=data, result=result, error=False))
    except (ValueError, TypeError, KeyError, OverflowError):
        rows.append(dict(input=data, result=None, error=True))
path = root / 'rust/tools/session-entry-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit('usage: gen_session_entry_goldens.py [--check]')
print(f'Verified {len(rows)} persisted session entry cases')

# Exercise the actual SessionDB routing methods against SQLite. The harness
# supplies the transaction boundary used by _execute_write and no runtime state.
import contextlib
import sqlite3
state = ast.parse((root / 'hermes_state.py').read_text())
methods = {'save_gateway_routing_entry', 'replace_gateway_routing_entries',
           'load_gateway_routing_entries', 'delete_gateway_routing_entries'}
namespace['time'] = __import__('time')
for method in [n for n in ast.walk(state) if isinstance(n, ast.FunctionDef) and n.name in methods]:
    exec(compile(ast.Module(body=[method], type_ignores=[]), 'routing-db-source', 'exec'), namespace)
class RoutingHarness:
    def __init__(self):
        self.conn = sqlite3.connect(':memory:')
        self.conn.row_factory = sqlite3.Row
        schema = ast.parse((root / 'hermes_state_common.py').read_text())
        ddl = next(n.value for n in ast.walk(schema) if isinstance(n, ast.Constant)
                   and isinstance(n.value, str) and 'CREATE TABLE IF NOT EXISTS gateway_routing (' in n.value)
        table = ddl[ddl.index('CREATE TABLE IF NOT EXISTS gateway_routing ('):].split(';', 1)[0]
        self.conn.execute(table)
    def _execute_write(self, callback):
        with self.conn:
            callback(self.conn)
    def _read_ctx(self):
        return contextlib.nullcontext(self.conn)
for name in methods:
    setattr(RoutingHarness, name, namespace[name])
store = RoutingHarness()
operations = [
    dict(op='save', scope='a', key='same', value='first'),
    dict(op='save', scope='b', key='same', value='other'),
    dict(op='save', scope='a', key='same', value='updated'),
    dict(op='save', scope='a', key='', value='ignored'),
    dict(op='save', scope='a', key='same', value=''),
    dict(op='replace', scope='a', entries={'new':'raw-json', '':'ignored', 'empty':''}),
    dict(op='delete', scope='b', keys=['missing', 'same', 'same']),
    dict(op='save', scope='', key='unicode-雪', value='{}'),
    dict(op='replace', scope='a', entries={}),
    dict(op='delete', scope='', keys=[]),
]
for operation in operations:
    scope = operation['scope']
    if operation['op'] == 'save':
        store.save_gateway_routing_entry(operation['key'], operation['value'], scope=scope)
    elif operation['op'] == 'replace':
        store.replace_gateway_routing_entries(operation['entries'], scope=scope)
    else:
        store.delete_gateway_routing_entries(operation['keys'], scope=scope)
    operation['expected'] = {scope:store.load_gateway_routing_entries(scope=scope) for scope in ['', 'a', 'b']}
path = root / 'rust/tools/session-routing-db-goldens.json'
text = json.dumps(operations, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(operations)} SQLite routing transitions')

# Compare recovery against the actual SessionStore merge method. Baseline and
# current values are canonical entry dictionaries, as in the running store.
from types import SimpleNamespace
method = next(n for n in ast.walk(source) if isinstance(n, ast.FunctionDef)
              and n.name == '_reconcile_recovered_routing_locked')
exec(compile(ast.Module(body=[method], type_ignores=[]), 'routing-recovery-source', 'exec'), namespace)
entry_type = namespace['SessionEntry']
namespace['json'] = json
def record(identifier):
    return entry_type.from_dict(dict(base, session_id=identifier)).to_dict()
rows = []
for baseline_present, current_id in [(False, None), (False, 'created'), (True, None), (True, 'baseline'), (True, 'edited')]:
    for durable in [None, 'bad-json', 'false', '{}', json.dumps(record('durable'))]:
        baseline = {'key':record('baseline')} if baseline_present else {}
        current = {'key':record(current_id)} if current_id else {}
        durable_rows = {'key':durable} if durable is not None else {}
        harness = SimpleNamespace(_routing_fallback_baseline=baseline, _routing_db_loaded=False,
            _entries={k:entry_type.from_dict(v) for k,v in current.items()},
            _routing_db=SimpleNamespace(load_gateway_routing_entries=lambda **kw:durable_rows),
            _routing_scope=lambda:'scope')
        namespace['_reconcile_recovered_routing_locked'](harness)
        rows.append(dict(baseline=baseline, current=current, durable=durable_rows,
            expected={k:v.to_dict() for k,v in harness._entries.items()}))
# Python dictionary equality equates bool/int/float values recursively. This
# decides whether a fallback entry changed during an outage.
for before, after in [(False,0), (True,1), (1,1.0), (0,-0.0),
        ({'nested':[False, 1]}, {'nested':[0, 1.0]}),
        (9007199254740993,9007199254740992.0),
        (18446744073709551615,18446744073709551616.0),
        (-9223372036854775808,-9223372036854775808.0),
        (1,True), (False,None), (1,'1'), ([1],[1,2])]:
    baseline = {'key':record('baseline')}
    current = {'key':record('baseline')}
    baseline['key']['metadata'] = {'value':before}
    current['key']['metadata'] = {'value':after}
    durable_rows = {'key':json.dumps(record('durable'))}
    harness = SimpleNamespace(_routing_fallback_baseline=baseline, _routing_db_loaded=False,
        _entries={k:entry_type.from_dict(v) for k,v in current.items()},
        _routing_db=SimpleNamespace(load_gateway_routing_entries=lambda **kw:durable_rows),
        _routing_scope=lambda:'scope')
    namespace['_reconcile_recovered_routing_locked'](harness)
    rows.append(dict(baseline=baseline, current=current, durable=durable_rows,
        expected={k:v.to_dict() for k,v in harness._entries.items()}))
path = root / 'rust/tools/session-routing-recovery-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} routing recovery decisions')

# Full-writer ordering and primary/mirror failure decisions from source.
import itertools
method = next(n for n in ast.walk(source) if isinstance(n, ast.FunctionDef)
              and n.name == '_persist_routing_data')
exec(compile(ast.Module(body=[method], type_ignores=[]), 'routing-writer-source', 'exec'), namespace)
rows = []
for revision, persisted, fast_revision, db_ok, mirror_ok, mirror_enabled in itertools.product(
        [1, 4], [0, 5], [0, 3], [False, True], [False, True], [False, True]):
    calls = {}
    def replace(data, **kwargs):
        if not db_ok:
            raise OSError('injected primary failure')
        calls['database'] = {k:json.loads(v) for k,v in data.items()}
    def mirror(data):
        if not mirror_ok:
            raise OSError('injected mirror failure')
        calls['mirror'] = data.copy()
    fast = {'key':(fast_revision, json.dumps(record('fast')))} if fast_revision else {}
    harness = SimpleNamespace(_save_lock=contextlib.nullcontext(), _persisted_routing_generation=persisted,
        _fast_persisted_entries=fast, _routing_scope=lambda:'scope',
        _routing_db=SimpleNamespace(replace_gateway_routing_entries=replace),
        _write_sessions_json=mirror_enabled, _save_sessions_json=mirror)
    error = False
    try:
        namespace['_persist_routing_data'](harness, {'key':record('snapshot')}, revision)
    except OSError:
        error = True
    rows.append(dict(data=record("snapshot"), fast_data=record("fast"), revision=revision, persisted=persisted, fast_revision=fast_revision,
        db_ok=db_ok, mirror_ok=mirror_ok, mirror_enabled=mirror_enabled, error=error,
        expected_persisted=harness._persisted_routing_generation,
        expected_fast=bool(harness._fast_persisted_entries), calls=calls))
path = root / 'rust/tools/session-routing-writer-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} routing writer decisions')

schema_tree = ast.parse((root / 'hermes_state_schema.py').read_text())
method = next(n for n in ast.walk(schema_tree) if isinstance(n, ast.FunctionDef)
              and n.name == '_heal_gateway_routing_pk')
namespace['sqlite3'] = sqlite3
exec(compile(ast.Module(body=[method], type_ignores=[]), 'routing-schema-source', 'exec'), namespace)
rows = []
for primary in ['PRIMARY KEY (session_key)', '', 'PRIMARY KEY (scope, session_key)']:
    ddl = 'CREATE TABLE gateway_routing (scope TEXT, session_key TEXT, entry_json TEXT, updated_at REAL'
    ddl += (', ' + primary if primary else '') + ');'
    ddl += "INSERT INTO gateway_routing VALUES ('p', 'key', 'old', 10);"
    if not primary:
        ddl += "INSERT INTO gateway_routing VALUES ('p', 'key', 'new', 20), (NULL, 'other', 'legacy', 5);"
    conn = sqlite3.connect(':memory:')
    conn.executescript(ddl)
    namespace['_heal_gateway_routing_pk'](object(), conn.cursor())
    rows.append(dict(sql=ddl, rows=conn.execute('SELECT scope, session_key, entry_json, updated_at FROM gateway_routing ORDER BY scope, session_key').fetchall()))
    conn.close()
path = root / 'rust/tools/session-routing-schema-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} routing schema repairs')

# Recovery isolation gates are independent of SQL ranking, but must apply to
# exact and legacy-key candidates alike before either can be reopened.
for name in ['_profile_from_session_key', '_recovered_row_allowed_for_active_profile', '_recovered_row_matches_source_scope']:
    method = next(n for n in ast.walk(source) if isinstance(n, ast.FunctionDef) and n.name == name)
    method.decorator_list = []
    exec(compile(ast.Module(body=[method], type_ignores=[]), 'recovery-isolation-source', 'exec'), namespace)
profiles = []
for requested, recovered, multiplex, active in itertools.product(
        ['agent:main:slack:x', 'agent:work:slack:x', 'agent::slack:x', 'legacy'],
        [None, '', 'legacy', 'agent:main:slack:x', 'agent:work:slack:x', 'agent:other:slack:x', 'agent:', 12],
        [False,True], ['default','work']):
    harness = SimpleNamespace(config=SimpleNamespace(multiplex_profiles=multiplex),
        _profile_from_session_key=namespace['_profile_from_session_key'], _active_profile_name=lambda:active)
    result = namespace['_recovered_row_allowed_for_active_profile'](harness, requested_session_key=requested, recovered={'session_key':recovered})
    profiles.append(dict(requested=requested, recovered=recovered, multiplex=multiplex, active=active, result=result))
scopes = []
for platform, chat_type, scope, origin in itertools.product(['slack','discord'], ['dm','group','thread'],
        [None,'','T1'], [None, '', 'bad', 'null', '[]', '{}', '{"scope_id":"T1"}',
         '{"guild_id":"T1"}', '{"scope_id":null,"guild_id":"T1"}', '{"scope_id":"T2","guild_id":"T1"}', {'scope_id':'T1'}]):
    source_data = dict(platform=platform,chat_id='C',chat_type=chat_type,scope_id=scope)
    session_source = namespace['SessionSource'].from_dict(source_data)
    result = namespace['_recovered_row_matches_source_scope']({'origin_json':origin}, session_source)
    scopes.append(dict(source=source_data, origin=origin, result=result))
path = root / 'rust/tools/session-recovery-isolation-goldens.json'
text = json.dumps(dict(profiles=profiles, scopes=scopes), indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(profiles)} profile and {len(scopes)} workspace recovery gates')

# Controlled local timezone for the source's naive datetime.fromtimestamp.
import os, time
method = next(n for n in ast.walk(source) if isinstance(n, ast.FunctionDef) and n.name == '_create_entry_from_recovered_row')
exec(compile(ast.Module(body=[method], type_ignores=[]), 'recovered-entry-source', 'exec'), namespace)
previous_tz = os.environ.get('TZ')
recovered_entries = []
try:
    for tz, offset in [('UTC0',0), ('UTC-8',28800)]:
        os.environ['TZ'] = tz
        time.tzset()
        for started, activity, has_messages in itertools.product(
                [None, 'bad', 0, -0.0000015, 0.0000005, 1788690000.123456, '1_000.25', '１２.５', 'nan'],
                [None, 'bad', 0, 20.123456], [None,False,True]):
            row = dict(id='durable', started_at=started, last_activity_at=activity, _has_messages=has_messages, message_count=0)
            source_data = dict(platform='slack',chat_id='C',chat_type='group',chat_name='Source name',scope_id='T')
            entry = namespace['_create_entry_from_recovered_row'](object(), row=row, session_key='agent:main:slack:group:C',
                source=namespace['SessionSource'].from_dict(source_data), now=datetime(2026,9,6))
            recovered_entries.append(dict(row=row, source=source_data, offset=offset, expected=entry.to_dict()))
finally:
    if previous_tz is None: os.environ.pop('TZ',None)
    else: os.environ['TZ'] = previous_tz
    time.tzset()
path = root / 'rust/tools/session-recovered-entry-goldens.json'
text = json.dumps(recovered_entries, indent=2) + '\n'
if sys.argv[1:] == ['--check']: assert path.read_text() == text
else: path.write_text(text)
print(f'Verified {len(recovered_entries)} recovered entry cases')
