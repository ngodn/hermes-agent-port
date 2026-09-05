#!/usr/bin/env python3
"""Execute STT's local/private URL predicate under the pinned Python version."""
import ast
import json
import sys
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'rust/tools/stt-locality-goldens.json'
tree=ast.parse((ROOT/'tools/transcription_tools.py').read_text())
node=next(n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name=='_is_local_or_private_url')
ns={}
exec(compile(ast.Module(body=[node],type_ignores=[]),'stt-locality','exec'),ns)
hosts=['localhost','LOCALHOST','localhost.','whisper','server.local','server.lan','server.internal','server.local.evil','10.0.0.1','10.01.2.3','127.1','127.0.0.1','169.254.1.1','172.16.0.1','172.32.0.1','192.168.1.1','192.0.0.9','192.0.0.8','100.64.1.2','0.0.0.0','8.8.8.8','192.0.2.1','[::1]','[::]','[fc00::1]','[fe80::1%eth0]','[::ffff:10.0.0.1]','[::ffff:100.64.1.2]','[2001:4860:4860::8888]','[2001:db8::1]','[3fff::1]']
urls=[prefix+host+suffix for host in hosts for prefix in ['http://','https://','//',''] for suffix in ['',':8000/v1']]
urls+=['','http://','http://[broken','http://user:pass@localhost/v1','http://remote/v1?url=http://localhost','\u001chttp://localhost','http://local\thost','http://[fe80::1%]','http://[fe80::1%a%b]']
rows=[{'url':url,'expected':ns['_is_local_or_private_url'](url)} for url in urls]
text=json.dumps(rows,indent=2)+'\n'
if sys.argv[1:]==['--check']: assert OUT.read_text()==text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_stt_locality_goldens.py [--check]')
print(f'Verified {len(rows)} STT locality cases')
