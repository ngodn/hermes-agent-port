#!/usr/bin/env python3
"""Execute Python's actual ordered visible-response cleanup for native strings."""
import ast
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/visible-response-goldens.json'
tree = ast.parse((ROOT / 'agent/agent_runtime_helpers.py').read_text())
namespace = {'re': re}
names = {'_REASONING_TAG_NAMES', '_TOOL_CALL_TAG_NAMES', '_REASONING_BLOCK_PATTERNS', '_TOOL_CALL_BLOCK_PATTERNS', '_NAMED_FUNCTION_BLOCK_PATTERN', '_UNTERMINATED_REASONING_BLOCK_PATTERN', '_ORPHAN_REASONING_TAG_PATTERN', '_STRAY_TOOL_CALL_CLOSER_PATTERN', '_UNTERMINATED_TOOL_CALL_PATTERN'}
nodes = [node for node in tree.body if (isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in names for t in node.targets)) or (isinstance(node, ast.FunctionDef) and node.name == 'strip_think_blocks')]
exec(compile(ast.Module(body=nodes, type_ignores=[]), 'visible-response', 'exec'), namespace)
cases = ['', 'hello', '  hello\u001c', '<function>prose</function>', 'Use <function name="x">prose</function>', 'Done. <function name="x">hidden</function> answer', '<function name = "x">hidden\ntext</function>', '<function name="x">unfinished', '<function anonymous>keep</function>', 'Done\narg <arg_key>x', 'Done\n<arg_value>private', 'literal <arg_key>x', '\u001c<think>x</think>\u001fanswer']
for tag in ['think', 'thinking', 'reasoning', 'REASONING_SCRATCHPAD', 'thought', 'tool_call', 'tool_calls', 'tool_result', 'function_call', 'function_calls']:
    for variant in [tag, tag.upper()]:
        cases.extend([f'<{variant}>private\ntext</{variant}>answer', f'prefix <{variant}>private</{variant}> tail', f'<{variant}>unclosed', f'answer\n  <{variant}>unclosed', f'Use <{variant}> in prose', f'</{variant}>\u001canswer', f'<{variant} id="x">private</{variant}>answer'])
cases.extend(['<THİNK>private</THİNK>answer', '<thınk>private</thınk>answer', '<functİon name="x">private</functİon>answer'])
rows = [{'input': s, 'expected': namespace['strip_think_blocks'](None, s)} for s in cases]
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text() == text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_visible_response_goldens.py [--check]')
print(f'Verified {len(rows)} visible-response cases')
