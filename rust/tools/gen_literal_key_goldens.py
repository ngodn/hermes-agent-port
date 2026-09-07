#!/usr/bin/env python3
"""Exercise CPython literal dictionary-key identity and retained key spelling."""
import ast
import json
from pathlib import Path
import sys
OUT=Path(__file__).with_name('literal-key-goldens.json')
keys=['True','1','1.0','1+0j','False','0','-0.0','0j','2','2.5','9007199254740992','9007199254740993','9007199254740992.0','10000000000000000000000000000000000000000','1e40','1e309',"'1'",'(True,)', '(1.0,)', '(1+0j,)']
cases=[]
keys.extend(['-1j','-0j','1e40j','1e-8j','1e309j','-1e309j','-0.0+1j','-0.0-1j'])
for left in keys:
    for right in keys:
        source=f"[{{{left}: 'first', {right}: 'last'}}]"
        cases.append(dict(source=source,expected=[str(item) for item in ast.literal_eval(source)]))
content=json.dumps(cases,indent=2)+'\n'
if sys.argv[1:]==['--check']:
    if OUT.read_text()!=content: raise SystemExit('Literal key fixtures differ')
elif not sys.argv[1:]: OUT.write_text(content)
else: raise SystemExit('usage: gen_literal_key_goldens.py [--check]')
print(f'Verified {len(cases)} Python literal key cases')
