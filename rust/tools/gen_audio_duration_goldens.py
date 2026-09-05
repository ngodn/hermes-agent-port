#!/usr/bin/env python3
"""Compare gateway duration formatting and CPython WAV header interpretation."""
import ast
import base64
import io
import json
import struct
import sys
import wave
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/audio-duration-goldens.json'
tree = ast.parse((ROOT / 'gateway/run.py').read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_format_duration')
ns = {}
exec(compile(ast.Module(body=[node], type_ignores=[]), 'duration', 'exec'), ns)
values = [-10, -0.5, 0, 0.49, 0.5, 0.51, 1.5, 2.5, 59.5, 60, 60.5, 3599.5, 3600, 3661, 36000, 999999.5]
formats = [{'seconds':v,'expected':ns['_format_duration'](v)} for v in values]
def chunk(name, data, size=None):
    return name + struct.pack('<I', len(data) if size is None else size) + data + (b'\0' if len(data)%2 else b'')
def riff(chunks, size=None):
    data=b'WAVE'+chunks
    return b'RIFF'+struct.pack('<I',len(data) if size is None else size)+data
files = []
for channels, rate, bits in [(1,16000,16), (2,44100,24), (1,0,8), (0,16000,16), (1,16000,0), (1,16000,12)]:
    for frames in [0,1,8000,24000,976000]:
        fmt=struct.pack('<HHIIHH',1,channels,rate,rate*channels*((bits+7)//8),channels*((bits+7)//8),bits)
        size=frames*channels*((bits+7)//8)
        files.append(riff(chunk(b'fmt ',fmt)+chunk(b'data',b'',size),36+size))
fmt=struct.pack('<HHIIHH',1,1,16000,32000,2,16)
files.extend([b'garbage',riff(chunk(b'data',b'0000')),riff(chunk(b'JUNK',b'x')+chunk(b'fmt ',fmt)+chunk(b'data',b'0000')),riff(chunk(b'fmt ',fmt[:5])),riff(chunk(b'fmt ',b'\3\0'+fmt[2:])+chunk(b'data',b'0000'))])
ext=struct.pack('<HHI',22,16,0)+bytes.fromhex('0100000000001000800000aa00389b71')
files.append(riff(chunk(b'fmt ',b'\xfe\xff'+fmt[2:]+ext)+chunk(b'data',b'0000')))
rows=[]
for data in files:
    try:
        with wave.open(io.BytesIO(data),'rb') as audio:
            seconds=audio.getnframes()/float(audio.getframerate() or 1)
    except Exception:
        seconds=None
    rows.append({'wav':base64.b64encode(data).decode(),'seconds':seconds,'bits':None if seconds is None else struct.unpack('<Q',struct.pack('<d',seconds))[0]})
text=json.dumps({'format':formats,'wav':rows},indent=2)+'\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text() == text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_audio_duration_goldens.py [--check]')
print(f'Verified {len(formats)} formats and {len(rows)} WAV headers')
