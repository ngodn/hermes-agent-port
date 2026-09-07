#!/usr/bin/env python3
"""Execute Python skill platform/environment filters with captured host inputs."""
import ast
import json
from pathlib import Path
import sys
from types import SimpleNamespace
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT/'rust/tools/skill-offer-goldens.json'
nodes = ast.parse((ROOT/'agent/skill_utils.py').read_text()).body
ns = dict(Any=object,Dict=dict)
selected = []
for node in nodes:
    if isinstance(node, ast.FunctionDef) and node.name in ['skill_matches_platform_list','skill_matches_environment']:
        selected.append(node)
    if isinstance(node, ast.Assign) and any(isinstance(t,ast.Name) and t.id in ['PLATFORM_MAP','_KNOWN_ENVIRONMENTS'] for t in node.targets):
        selected.append(node)
exec(compile(ast.Module(body=selected,type_ignores=[]),'agent/skill_utils.py','exec'),ns)
values = [None,False,0,1,'', ' ',[],{}, [''],[' '], ' MACOS ', 'windows','linux','android','termux',['linux','windows'],['future'], ['kanban','docker'], ['s6'], ['docker','future'],[None],[False],{'linux':True}]
cases=[]
for value in values:
    for host in ['linux','android','darwin','win32']:
        for termux in [False,True]:
            ns['sys']=SimpleNamespace(platform=host)
            ns['is_termux']=lambda:termux
            for active in [[],['kanban'],['docker'],['s6']]:
                calls=[]
                def detect(tag):
                    calls.append(tag)
                    return tag in active
                ns['_detect_environment']=detect
                platform=ns['skill_matches_platform_list'](value)
                environment=ns['skill_matches_environment']({'environments':value})
                cases.append(dict(value=value,host=host,termux=termux,active=active,platform=platform,environment=environment,calls=calls))
content=json.dumps(cases,indent=2)+'\n'
if sys.argv[1:]==['--check']:
    if OUT.read_text()!=content: raise SystemExit('Skill offer goldens differ')
elif not sys.argv[1:]: OUT.write_text(content)
else: raise SystemExit('usage: gen_skill_offer_goldens.py [--check]')
print(f'Verified {len(cases)} Python skill offer cases')
