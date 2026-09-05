#!/usr/bin/env python3
"""Execute Hermes override assembly and the installed SDK's final JSON merge."""
import json
from pathlib import Path
import sys
import textwrap
from openai._base_client import _merge_mappings
import openai
ROOT = Path(__file__).resolve().parents[2]
source = (ROOT / 'agent/transports/chat_completions.py').read_text()
a = source.index('        # Request overrides (user config)', source.index('        # extra_body assembly'))
b = source.index('        if extra_body:', a)
block = compile(textwrap.dedent(source[a:b]), 'request-overrides-reference', 'exec')
rows = []
for extra in [{}, {'reasoning': {'enabled': True, 'effort': 'medium'}, 'temperature': 0.5}]:
    for overrides in [{}, {'temperature': 1}, {'extra_body': {'temperature': None}}, {'extra_body': {'reasoning': {'enabled': False}}}, {'extra_body': {'extra_body': {'custom': True}}}, {'extra_body': None}, {'extra_body': []}, {'extra_body': 'bad'}, {'extra_body': {'temperature': 2}, 'temperature': 3}, {'reasoning': {'effort': 'low'}, 'extra_body': {'reasoning': {'enabled': False}}}]:
        body = {'model': 'fixture', 'temperature': 0.1, 'reasoning': {'enabled': True, 'effort': 'high'}}
        scope = {'api_kwargs': dict(body), 'extra_body': dict(extra), 'params': {'request_overrides': overrides}}
        exec(block, scope)
        if scope['extra_body']: scope['api_kwargs']['extra_body'] = scope['extra_body']
        prepared = scope['api_kwargs']
        additions = prepared.pop('extra_body', None)
        row = dict(body=body, profile_extra=extra, overrides=overrides)
        try:
            row['result'] = prepared if additions is None else _merge_mappings(prepared, additions)
        except Exception as e: row['error'] = type(e).__name__
        rows.append(row)
value = {'sdk_version': openai.__version__, 'cases': rows}
text = json.dumps(value, indent=2) + '\n'
path = ROOT / 'rust/tools/request-merge-goldens.json'
if sys.argv[1:] == ['--check']: assert path.read_text() == text
elif not sys.argv[1:]: path.write_text(text)
else: raise SystemExit('usage: gen_request_merge_goldens.py [--check]')
print(f'Verified {len(rows)} merge cases against SDK {openai.__version__}')
