#!/usr/bin/env python3
"""Execute the full Python pre-call sanitizer and its real policy dependencies."""
import ast
import copy
import json
import logging
import random
import sys
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Tuple

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/tool-pairing-goldens.json'
namespace = {'Any': Any, 'Dict': Dict, 'List': List, 'Tuple': Tuple,
             '_log_empty_non_final_heal': lambda count: None}

def extract(path, names, class_name=None):
    tree = ast.parse((ROOT / path).read_text())
    body = tree.body
    if class_name:
        body = next(n.body for n in body if isinstance(n, ast.ClassDef) and n.name == class_name)
    nodes = []
    for node in body:
        if isinstance(node, ast.FunctionDef) and node.name in names:
            node.decorator_list = []
            nodes.append(node)
        elif isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in names for t in node.targets):
            nodes.append(node)
    exec(compile(ast.Module(body=nodes, type_ignores=[]), path, 'exec'), namespace)

extract('agent/message_sanitization.py', {'_expand_tool_id_variants', 'tool_call_id_variants', 'tool_result_id_variants', 'coalesce_tool_call_id'})
namespace['_sanitize_coalesce_tool_call_id'] = namespace['coalesce_tool_call_id']
extract('run_agent.py', {'_get_tool_call_id_static', '_get_tool_call_name_static', '_VALID_API_ROLES'}, 'AIAgent')
namespace['_ra'] = lambda: SimpleNamespace(AIAgent=SimpleNamespace(**{name: namespace[name] for name in ['_get_tool_call_id_static', '_get_tool_call_name_static', '_VALID_API_ROLES']}), logger=logging.getLogger('oracle'))
extract('agent/agent_runtime_helpers.py', {'_INTERRUPTED_PLACEHOLDER', '_msg_has_payload', 'repair_empty_non_final_messages', 'sanitize_api_messages'})
logging.disable(logging.CRITICAL)

def call(identifier='a', name='lookup', **extra):
    return {'id': identifier, 'type': 'function', 'function': {'name': name, 'arguments': '{}'}, **extra}

def assistant(*calls):
    return {'role': 'assistant', 'content': None, 'tool_calls': list(calls)}

def result(identifier='a', **extra):
    return {'role': 'tool', 'tool_call_id': identifier, 'content': 'result', **extra}

user = {'role': 'user', 'content': 'next'}
cases = [[], [user], [result()], [assistant(call())], [assistant(call()), user, result()],
         [result(), assistant(call())], [assistant(call()), result(), result()],
         [assistant(call()), result(), assistant(call()), result()],
         [assistant(call('z'), call('a'))], [assistant(call('a'), call('a')), result()],
         [assistant(call()), {'role': 'system', 'content': 'system'}, result()],
         [assistant(call()), {'role': 'developer', 'content': 'dev'}, result()],
         [assistant(call()), {'role': 'assistant', 'content': 'next'}, result()],
         [{'role': 'invalid'}, {'role': None}, {'role': 'function', 'content': 'legacy'}, user]]
for value in [None, [], {}, '', 'bad', False, 3]:
    cases.append([{'role': 'assistant', 'content': None, 'tool_calls': value}, user])
for function in [None, {}, {'arguments': '[]'}, {'name': ''}, {'name': ' \u001c'}, {'name': 12}, {'name': ' lookup '}, 'bad']:
    tc = call()
    tc['function'] = function
    cases.append([assistant(tc), result(name='internal')])
for identifier in ['a', ' a ', 'a|item', ' a | item ', '|item', 'a|item|other', '\u001ca\u001f', '', None]:
    for returned in ['a', 'item', 'a|item', 'missing', '', None]:
        cases.append([assistant(call(identifier)), result(returned, name='internal'), user])
for extra in [{'call_id': 'pair', 'response_item_id': 'item'}, {'call_id': 'pair'}, {'response_item_id': 'item'}, {'call_id': None, 'response_item_id': 12}]:
    for returned in ['a', 'pair', 'item', 'pair|item']:
        cases.append([assistant(call('a', **extra)), result(returned, name='internal')])
for malformed in [None, 3, 'call', [], {}]:
    cases.append([assistant(malformed), user])
for name in [None, '', 'lookup', 'internal']:
    cases.append([assistant(call()), result(name=name)])
cases.append([assistant(call()), result()])
# Deterministic mixtures exercise positional closing, alias collisions and
# re-arming. Every input is valid JSON; expectations come only from Python.
rng = random.Random(92471)
for _ in range(150):
    messages = []
    for _ in range(rng.randrange(1, 14)):
        kind = rng.randrange(5)
        if kind == 0:
            messages.append(copy.deepcopy(user))
        elif kind == 1:
            messages.append({'role': 'system', 'content': 'prefix'})
        elif kind == 2:
            messages.append({'role': 'assistant', 'content': 'answer'})
        elif kind == 3:
            messages.append(assistant(*(call(rng.choice(['a', 'b', 'a|item', 'item', '']), rng.choice(['lookup', 'clock', '']), call_id=rng.choice([None, 'a', 'pair'])) for _ in range(rng.randrange(1, 4)))))
        else:
            messages.append(result(rng.choice(['a', 'b', 'item', 'pair', 'a|item']), name=rng.choice(['lookup', 'clock', 'internal'])))
    cases.append(messages)
rows = []
for messages in cases:
    expected = namespace['sanitize_api_messages'](copy.deepcopy(messages))
    rows.append({'messages': messages, 'expected': expected})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text, 'tool-pairing fixtures differ from Python'
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_tool_pairing_goldens.py [--check]')
print(f'Verified {len(rows)} tool-pairing cases')
