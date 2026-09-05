#!/usr/bin/env python3
"""Execute the Python STT language precedence resolver."""
import ast
import json
import sys
from pathlib import Path
from typing import Any, Optional, Dict
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'rust/tools/stt-language-goldens.json'
tree=ast.parse((ROOT/'tools/transcription_tools.py').read_text())
nodes=[n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name in {'_get_stt_section','_resolve_stt_language'}]
class Env:
    value=None
    def getenv(self,key): return self.value
env=Env()
ns={'os':env,'Optional':Optional,'Dict':Dict,'Any':Any,'LOCAL_STT_LANGUAGE_ENV':'fixture'}
exec(compile(ast.Module(body=nodes,type_ignores=[]),'language','exec'),ns)
rows=[]
for provider in [None,{},False,{'language':None},{'language':True},{'language':' '},{'language':' ms '},{'language_code':' ja '},{'language':'\u001cde\u001f','language_code':'ja'}]:
    for global_ in [None,False,'',' en ']:
        for legacy in [None,'',' zh ']:
            config={'openai':provider,'language':global_}
            env.value=legacy
            expected=ns['_resolve_stt_language']('openai',config,extra_keys=('language_code',))
            rows.append({'config':config,'env':legacy,'expected':expected})
text=json.dumps(rows,indent=2)+'\n'
if sys.argv[1:]==['--check']: assert OUT.read_text()==text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_stt_language_goldens.py [--check]')
print(f'Verified {len(rows)} STT language cases')
