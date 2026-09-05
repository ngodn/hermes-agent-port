#!/usr/bin/env python3
"""Execute Python's empty-message repair and payload detection."""
import ast
import copy
import json
import sys
from pathlib import Path
from typing import Any, Dict, List

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/empty-message-goldens.json'
tree = ast.parse((ROOT / 'agent/agent_runtime_helpers.py').read_text())
nodes = [node for node in tree.body if
         isinstance(node, ast.FunctionDef) and node.name in {'_msg_has_payload', 'repair_empty_non_final_messages'}
         or isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == '_INTERRUPTED_PLACEHOLDER' for t in node.targets)]
namespace = {'Any': Any, 'Dict': Dict, 'List': List, '_log_empty_non_final_heal': lambda count: None}
exec(compile(ast.Module(body=nodes, type_ignores=[]), 'empty-message-repair', 'exec'), namespace)
contents = [None, '', ' \u001c', '\u200b', 'visible', [], {}, False, 0,
            [{'type': 'text', 'text': ' '}], [{'type': 'text', 'text': 12}],
            [{'type': 'image_url'}], [{'type': 'thinking'}], [{}], [None, '', False], ['x']]
messages_cases = [[], [{'role': 'assistant', 'content': None}]]
for role in ['user', 'assistant', 'tool', 'system', 'developer']:
    for content in contents:
        messages_cases.append([{'role': role, 'content': content}, {'role': 'user', 'content': 'next'}])
for field in ['tool_calls', 'tool_call_id', 'reasoning', 'reasoning_content', 'reasoning_details', 'codex_message_items', 'codex_reasoning_items']:
    for value in [None, '', ' ', [], [None], {}, True, 'payload']:
        messages_cases.append([{'role': 'assistant', 'content': None, field: value}, {'role': 'user', 'content': 'next'}])
messages_cases += [[{'role': 'user'}, {'role': 'assistant', 'content': ''}],
                   [{'role': 'user', 'content': ''}, {'role': 'assistant', '_thinking_prefill': True}, {'role': 'user', 'content': 'next'}]]
rows = []
for messages in messages_cases:
    before = copy.deepcopy(messages)
    expected = namespace['repair_empty_non_final_messages'](messages)
    assert messages == before
    rows.append({'messages': messages, 'expected': expected})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_empty_message_goldens.py [--check]')
print(f'Verified {len(rows)} empty-message cases')
