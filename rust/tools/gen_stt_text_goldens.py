#!/usr/bin/env python3
"""Execute the Python transcript normalizer for text-channel responses."""
import ast
import json
import re
import sys
from pathlib import Path
from typing import Any, Optional
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/stt-text-goldens.json'
tree = ast.parse((ROOT / 'tools/transcription_tools.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_extract_transcript_text')
ns = {'re':re, 'Any':Any, 'Optional':Optional}
exec(compile(ast.Module(body=[node], type_ignores=[]), 'stt-text', 'exec'), ns)
cases = ['', ' spoken words ', '\u001chello\u001f', 'language en<asr_text>hello', 'language en <audio_language>English</audio_language><asr_text> hello\nworld ', 'LANGUAGE ms <ASR_TEXT>hai', 'language en-US<asr_text>hi', 'language en.US<asr_text>hi', 'language en<asr_text>', 'language en <audio_language>x<y</audio_language><asr_text>keep', 'language en words', 'prefix language en<asr_text>keep', 'language en<asr_text>hi</asr_text>', '你好 猫', 'language 猫<asr_text>hello']
rows = [{'input':s,'expected':ns['_extract_transcript_text'](s)} for s in cases]
text=json.dumps(rows,ensure_ascii=False,indent=2)+'\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text() == text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_stt_text_goldens.py [--check]')
print(f'Verified {len(rows)} STT text cases')
