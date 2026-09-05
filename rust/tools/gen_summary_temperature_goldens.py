#!/usr/bin/env python3
"""Execute Python's auxiliary temperature policy for summary requests."""
import ast
import json
import logging
import sys
from pathlib import Path
from typing import Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/summary-temperature-goldens.json'
tree = ast.parse((ROOT / 'agent/auxiliary_client.py').read_text())
names = {'_is_kimi_model', '_is_arcee_trinity_thinking', '_fixed_temperature_for_model'}
nodes = [node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name in names]
omit = object()
namespace = {'Optional': Optional, 'OMIT_TEMPERATURE': omit, 'logger': logging.getLogger(__name__)}
exec(compile(ast.Module(body=nodes, type_ignores=[]), 'auxiliary-temperature', 'exec'), namespace)
models = ['', 'kimi', 'KIMI-K2', 'moonshot/kimi-k2.5', 'kimi-other', 'kimiko', 'moonshot-v1',
          'trinity-large-thinking', 'arcee-ai/trinity-large-thinking', 'ARCEE/TRINITY-LARGE-THINKING',
          'trinity-large-thinking:free', 'trinity-large-thinking-v2', 'prefix/trinity-large-thinking/',
          'trinity-large', 'gpt-5', '\u001cArcee/trinity-large-thinking\u001f',
          ' arcee/trinity-large-thinking ', '\u200btrinity-large-thinking', 'arcee/ trinity-large-thinking']
rows = []
for model in models:
    for url in [None, 'https://openrouter.ai/api/v1', 'http://localhost:8080', 'https://api.moonshot.ai/v1']:
        result = namespace['_fixed_temperature_for_model'](model, url)
        rows.append({'model': model, 'base_url': url, 'temperature': None if result is omit else result})
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit('usage: gen_summary_temperature_goldens.py [--check]')
print(f'Verified {len(rows)} summary temperature cases')
