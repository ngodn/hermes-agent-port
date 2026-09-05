#!/usr/bin/env python3
"""Execute the reference STT credential resolver with recorded external effects."""
import ast
import json
import sys
import types
from pathlib import Path
from urllib.parse import urljoin
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'rust/tools/stt-credential-goldens.json'
tree=ast.parse((ROOT/'tools/transcription_tools.py').read_text())
nodes=[n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name in {'_resolve_openai_audio_client_config','_is_local_or_private_url'}]
helper=types.ModuleType('tools.tool_backend_helpers')
helper.NOUS_MANAGED_PROVIDER='nous'
helper.selection_error=lambda section,name,failure: f"{section} is configured to use {name} (set via hermes tools), but {failure}. Run 'hermes tools' to change it."
sys.modules['tools.tool_backend_helpers']=helper
rows=[]
for selected in [None,'nous','openai','local','custom']:
    for cfg in [{},{'api_key':'config-key'},{'base_url':'http://localhost:8000/v1'},{'base_url':'https://custom.example/v1'},{'api_key':'config-key','base_url':'https://custom.example/v1'}]:
        for direct in ['', 'direct-key']:
            for managed in [False,True]:
                calls=[]
                helper.read_selection=lambda section: selected
                def resolve_direct():
                    calls.append('direct')
                    return direct
                def resolve_managed(vendor):
                    calls.append(vendor)
                    return types.SimpleNamespace(nous_user_token='managed-key',gateway_origin='https://gateway.example/vendor/') if managed else None
                ns={'urljoin':urljoin,'_load_stt_config':lambda:{'openai':cfg},'OPENAI_BASE_URL':'https://api.openai.com/v1','resolve_openai_audio_api_key':resolve_direct,'resolve_managed_tool_gateway':resolve_managed,'managed_nous_tools_enabled':lambda:False}
                exec(compile(ast.Module(body=nodes,type_ignores=[]),'stt-credentials','exec'),ns)
                try:
                    result=ns['_resolve_openai_audio_client_config']()
                    error=None
                except ValueError as exc:
                    result=None;error=str(exc)
                rows.append({'selection':selected,'openai':cfg,'direct_key':direct,'managed':managed,'result':result,'error':error,'calls':calls})
text=json.dumps(rows,indent=2)+'\n'
if sys.argv[1:]==['--check']: assert OUT.read_text()==text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_stt_credential_goldens.py [--check]')
print(f'Verified {len(rows)} credential selection scenarios')
