#!/usr/bin/env python3
"""Execute the upstream scrubber at each delta, including response flushes."""
import importlib.util
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/think-stream-goldens.json'
spec = importlib.util.spec_from_file_location('think_scrubber', ROOT / 'agent/think_scrubber.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
texts = ['hello 世界', '<', 'prose <think> mention', 'x <think>hidden</think> done', 'x\n  <think>hidden</think>done', 'x</think>\n done', '<think>unclosed', '<THINKING>hidden</tHiNkInG>answer', '<thought>hidden</reasoning>answer', '\u001c<think>hidden</think>done', '<think>x<thought>y</thought>z</think>answer']
for tag in ['think', 'thinking', 'reasoning', 'thought', 'REASONING_SCRATCHPAD']:
    texts.append(f'<{tag}>private\ntext</{tag}>answer 猫')
rows = []
sequences = []
for text in texts:
    sequences.append([text, None])
    sequences.append(list(text) + [None])
    sequences.extend([text[:i], text[i:], None] for i in range(len(text) + 1))
sequences.extend([['answer', '<', None, '<think>', 'private', '</think>next', None], ['<think>', 'private', None, 'next', None], ['', None, '', None]])
for sequence in sequences:
    scrubber = module.StreamingThinkScrubber()
    rows.append([{'input': part, 'output': scrubber.flush() if part is None else scrubber.feed(part)} for part in sequence])
text = json.dumps(rows, ensure_ascii=False, indent=2) + '\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text() == text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_think_stream_goldens.py [--check]')
print(f'Verified {len(rows)} streamed reasoning sequences')
