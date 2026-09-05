#!/usr/bin/env python3
"""Execute Python's tool-call identity construction, including its real builder block."""
import ast
import copy
import hashlib
import json
import logging
import re
import sys
import uuid
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/call-identity-goldens.json'
namespace = {'Any': Any, 'Optional': Optional, 'hashlib': hashlib, 're': re, 'uuid': uuid, 'logger': logging.getLogger('oracle')}
logging.disable(logging.CRITICAL)
for file, names in [('agent/message_sanitization.py', {'deterministic_call_id', 'uniquify_tool_call_ids'}),
                    ('agent/codex_responses_adapter.py', {'_split_responses_tool_id', '_derive_responses_function_call_id'})]:
    tree = ast.parse((ROOT / file).read_text())
    nodes = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in names]
    exec(compile(ast.Module(body=nodes, type_ignores=[]), file, 'exec'), namespace)
tree = ast.parse((ROOT / 'agent/chat_completion_helpers.py').read_text())
function = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'build_assistant_message')
block = next(n for n in function.body if isinstance(n, ast.If) and isinstance(n.test, ast.Name) and n.test.id == 'assistant_tool_calls')
program = compile(ast.Module(body=[block], type_ignores=[]), 'assistant-tool-call-builder', 'exec')
agent = SimpleNamespace(_split_responses_tool_id=namespace['_split_responses_tool_id'],
                        _derive_responses_function_call_id=namespace['_derive_responses_function_call_id'],
                        _deterministic_call_id=namespace['deterministic_call_id'])
rows = []
for raw in [None, '', ' ', '\u001c\u001f', ' id ', 'call|item', '|item', 'fc_item', 12]:
    for explicit in [None, '', ' pair ', 'pair|item']:
        for arguments in ['{}', '{ "q": "猫" }', 'broken JSON', None]:
            calls = [{'id': raw, 'call_id': explicit, 'type': 'function', 'function': {'name': 'lookup', 'arguments': arguments}} for _ in range(2)]
            normalized = copy.deepcopy(calls)
            for call in normalized:
                if call['function']['arguments'] is None:
                    call['function']['arguments'] = '{}'
            namespace['uniquify_tool_call_ids'](normalized)
            sdk_calls = [SimpleNamespace(**{**call, 'function': SimpleNamespace(**call['function'])}) for call in normalized]
            scope = {'agent': agent, 'assistant_tool_calls': sdk_calls, 'msg': {}}
            exec(program, scope)
            rows.append({'calls': calls, 'ids': [call['id'] for call in scope['msg']['tool_calls']]})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_call_identity_goldens.py [--check]')
print(f'Verified {len(rows)} call-identity cases')
