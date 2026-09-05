#!/usr/bin/env python3
"""Execute read_selection with synthetic raw config, without merged defaults."""
import ast
import json
import sys
import types
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'rust/tools/tool-selection-goldens.json'
helper=types.ModuleType('hermes_cli.config')
sys.modules['hermes_cli.config']=helper
ns={}
utils=ast.parse((ROOT/'utils.py').read_text())
# Keep the function's annotation available without importing YAML utilities.
ns['Any']=object
nodes=[n for n in utils.body if (isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='TRUTHY_STRINGS' for t in n.targets)) or (isinstance(n,ast.FunctionDef) and n.name=='is_truthy_value')]
exec(compile(ast.Module(body=nodes,type_ignores=[]),'truthy','exec'),ns)
tree=ast.parse((ROOT/'tools/tool_backend_helpers.py').read_text())
names={'NOUS_MANAGED_PROVIDER','_SELECTION_NAME_KEYS','_DEFAULT_NAME_KEYS'}
nodes=[n for n in tree.body if (isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id in names for t in n.targets)) or (isinstance(n,ast.FunctionDef) and n.name=='read_selection')]
exec(compile(ast.Module(body=nodes,type_ignores=[]),'selection','exec'),ns)
rows=[]
for section in ['stt','browser','web','tts']:
    for value in [None,'',' local ',' OpenAI ',False,0,[],{},'\u001cNous\u001f']:
        for gateway in [None,False,True,'false',' YES ',[]]:
            raw={'provider':value,'backend':'backup','cloud_provider':'cloud','use_gateway':gateway}
            config={section:raw}
            helper.read_raw_config_readonly=lambda:config
            rows.append({'section':section,'config':config,'expected':ns['read_selection'](section)})
text=json.dumps(rows,indent=2)+'\n'
if sys.argv[1:]==['--check']: assert OUT.read_text()==text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_tool_selection_goldens.py [--check]')
print(f'Verified {len(rows)} raw provider selections')
