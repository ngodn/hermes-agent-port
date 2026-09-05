#!/usr/bin/env python3
"""Run Python audio validation against temporary files and sparse size limits."""
import ast
import json
import os
import sys
import tempfile
from pathlib import Path
from typing import Optional, Dict, Any
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/audio-validation-goldens.json'
tree=ast.parse((ROOT/'tools/transcription_tools.py').read_text())
names={'SUPPORTED_FORMATS','MAX_FILE_SIZE'}
functions={'_validate_audio_file_size','_validate_audio_source_file','_validate_audio_file'}
nodes=[n for n in tree.body if (isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id in names for t in n.targets)) or (isinstance(n,ast.FunctionDef) and n.name in functions)]
ns={'Path':Path,'os':os,'Optional':Optional,'Dict':Dict,'Any':Any}
exec(compile(ast.Module(body=nodes,type_ignores=[]),'audio-validation','exec'),ns)
rows=[]
with tempfile.TemporaryDirectory() as root:
    cases=[('voice'+ext,'file',0) for ext in sorted(ns['SUPPORTED_FORMATS'])]
    cases += [('voice.WAV','file',1),('voice.txt','file',1),('.wav','file',1),('voice.','file',1),('voice.wav','missing',0),('voice.wav','directory',0),('voice.wav','symlink',0)]
    cases += [('voice.wav','file',size) for size in [25*1024*1024,25*1024*1024+1,26*1024*1024]]
    for index,(name,kind,size) in enumerate(cases):
        directory=Path(root)/str(index);directory.mkdir();path=directory/name
        if kind=='file':
            with path.open('wb') as stream: stream.truncate(size)
        elif kind=='directory': path.mkdir()
        elif kind=='symlink': path.symlink_to(directory/'missing')
        for cap in [False,True]:
            result=ns['_validate_audio_file'](str(path),enforce_size_limit=cap)
            error=None if result is None else result['error'].replace(str(directory),'${ROOT}')
            rows.append({'name':name,'kind':kind,'size':size,'cap':cap,'error':error})
text=json.dumps(rows,indent=2)+'\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text()==text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_audio_validation_goldens.py [--check]')
print(f'Verified {len(rows)} audio validation cases')
