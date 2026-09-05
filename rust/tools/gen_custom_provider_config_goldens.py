#!/usr/bin/env python3
"""Execute the compatible custom-provider view from hermes_cli.config."""
import ast
import itertools
import json
import logging
import re
import sys
import types
from pathlib import Path
from typing import Any, Dict, List, Optional
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'rust/tools/custom-provider-config-goldens.json'
tree=ast.parse((ROOT/'hermes_cli/config.py').read_text())
names={'_canonical_api_mode','coerce_provider_id','_normalize_custom_provider_entry','providers_dict_to_custom_providers','get_compatible_custom_providers','normalize_extra_headers','is_provider_enabled'}
nodes=[n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name in names or isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='_API_MODE_ALIASES' for t in n.targets)]
logger=logging.getLogger('provider-config-fixture');logger.disabled=True
ns=dict(Any=Any,Dict=Dict,List=List,Optional=Optional,re=re,logger=logger,_warn_once_per_provider=lambda *args:None)
exec(compile(ast.Module(body=nodes,type_ignores=[]),'provider-config','exec'),ns)
variants=[{}, {'apiKey':' key ','apiMode':'openai','defaultModel':' m '},
 {'api_key_env':' KEY ','keyEnv':'other','baseUrl':'https://camel.example/v1'},
 {'api_key':None,'apiKey':'ignored','key_env':'','api_key_env':' KEY '},
 {'models':[' one ',{'id':'two','context_length':123},{'id':None,'name':'three','vision':True},None,2]},
 {'models':{'__discovered_model_catalog__':True,'__explicit_model_allowlist__':True,'m':{'vision':False}}},
 {'models':{'__discovered_model_catalog__':1},'models_discovered':False},
 {'extra_headers':{'x-key':' secret ', 'x-flag':True, 'x-number':3,'drop':None},'extra_body':{'routing':'fixture'}},
 {'contextLength':True,'rateLimitDelay':False,'capabilities':{'vision':True,'audio':False,'bad':1}},
 {'ssl_verify':' false ','ssl_ca_cert':' /tmp/test-ca.pem ','discover_models':False},
 {'transport':'Responses','model':'chosen','default_model':'ignored','unknown':'dropped'},
 {'name':2070,'api_key':9,'models':False}]
rows=[]
for variant,url,enabled in itertools.product(variants,['https://example.com/v1/','http://localhost:1234/v1','${ENDPOINT}/v1','invalid'],[True,'off',None]):
    entry=dict(name=' Example ',api=url,enabled=enabled,**variant) if 'name' not in variant else dict(api=url,enabled=enabled,**variant)
    for config in [{'providers':{'example':entry}}, {'custom_providers':[entry]}]:
        before=json.dumps(config,sort_keys=True)
        result=ns['get_compatible_custom_providers'](config)
        assert json.dumps(config,sort_keys=True)==before
        rows.append(dict(config=config,result=result))
for legacy in [None,[],{},'invalid',[{'name':'Shared','base_url':'https://shared.example/v1','extra_body':{'winner':'legacy'}}]]:
    config={'custom_providers':legacy,'providers':{'first':{'name':'Shared','api':'https://SHARED.example/v1/'},'second':{'name':'Shared','api':'https://shared.example/v1','default_model':'other'}}}
    rows.append(dict(config=config,result=ns['get_compatible_custom_providers'](config)))
for key in ['', ' ', '\x1c\t', ' lab ']:
    config={'providers':{key:{'name':'Lab','base_url':'https://lab.example/v1'}}}
    rows.append(dict(config=config,result=ns['get_compatible_custom_providers'](config)))
text=json.dumps(rows,indent=2)+'\n'
if sys.argv[1:]==['--check']: assert OUT.read_text()==text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_custom_provider_config_goldens.py [--check]')
print(f'Verified {len(rows)} custom-provider config cases')

runtime=ast.parse((ROOT/'hermes_cli/runtime_provider.py').read_text())
names={'_normalize_custom_provider_name','_get_named_custom_provider','_parse_api_mode','_filter_capabilities','_lift_max_output_tokens','_lift_extra_headers'}
nodes=[n for n in runtime.body if isinstance(n,ast.FunctionDef) and n.name in names or isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='_VALID_API_MODES' for t in n.targets)]
provider_tree=ast.parse((ROOT/'hermes_cli/providers.py').read_text())
nodes += [n for n in provider_tree.body if isinstance(n,ast.FunctionDef) and n.name in {'custom_provider_slug','custom_provider_aliases'}]
config_stub=types.ModuleType('hermes_cli.config'); config_stub._canonical_api_mode=ns['_canonical_api_mode']; config_stub.is_provider_enabled=ns['is_provider_enabled']; sys.modules[config_stub.__name__]=config_stub
ns['AuthError']=RuntimeError
exec(compile(ast.Module(body=nodes,type_ignores=[]),'named-provider','exec'),ns)
named_rows=[]
for requested,canonical,keyed,enabled,env_key in itertools.product(['lab','custom:lab','Fancy Lab','custom:fancy-lab','missing','auto','custom'],[None,'lab','builtin'],[False,True],[False,True],['','env-fixture']):
    entry=dict(name='Fancy Lab',base_url='https://lab.example/v1',api_key='inline-fixture',key_env='LAB_KEY',enabled=enabled,default_model='default',model='legacy-model',transport='responses',extra_body={'route':'named'},extra_headers={'x-key':'header-fixture'},max_tokens=256,capabilities={'vision':True,'bad':1},key_cmd='fixture-command')
    config={'providers':{'other':dict(api='https://other.example/v1',key_env='OTHER_KEY'),'lab':entry}} if keyed else {'custom_providers':[entry]}
    calls=[]
    def get_env(name,default=''):
        calls.append(name); return env_key
    def resolve(name):
        if canonical is None: raise RuntimeError('unknown provider')
        return canonical
    ns.update(load_config=lambda:config,_getenv=get_env,auth_mod=types.SimpleNamespace(resolve_provider=resolve))
    result=ns['_get_named_custom_provider'](requested)
    named_rows.append(dict(config=config,requested=requested,canonical=canonical,env_key=env_key,result=result,calls=calls))
# False-valued aliases must not trigger secret lookups. Exercise both alias
# precedence and fallback to an inline key using the actual Python getter.
for primary,secondary in itertools.product([None,False,0,'','LAB_KEY'],repeat=2):
    config={'providers':{'lab':{'api':'https://lab.example/v1','key_env':primary,'api_key_env':secondary,'api_key':' inline-fixture '}}}
    calls=[]
    ns.update(load_config=lambda:config,_getenv=lambda name,default='':calls.append(name) or '',auth_mod=types.SimpleNamespace(resolve_provider=lambda name:'builtin'))
    result=ns['_get_named_custom_provider']('lab')
    named_rows.append(dict(config=config,requested='lab',canonical='builtin',env_key='',result=result,calls=calls))
for primary,alias_cap,keyed in itertools.product([None,False,True,0,-1,256,'256'],[None,512], [False,True]):
    entry={'name':'lab','base_url':'https://lab.example/v1','max_output_tokens':primary,'max_tokens':alias_cap}
    config={'providers':{'lab':entry}} if keyed else {'custom_providers':[entry]}
    calls=[]
    ns.update(load_config=lambda:config,_getenv=lambda name,default='':calls.append(name) or '',auth_mod=types.SimpleNamespace(resolve_provider=lambda name:'builtin'))
    result=ns['_get_named_custom_provider']('lab')
    named_rows.append(dict(config=config,requested='lab',canonical='builtin',env_key='',result=result,calls=calls))
path=ROOT/'rust/tools/named-provider-goldens.json'
text=json.dumps(named_rows,indent=2)+'\n'
if sys.argv[1:]==['--check']: assert path.read_text()==text
else: path.write_text(text)
print(f'Verified {len(named_rows)} named-provider cases')

from urllib.parse import urlsplit, urlunsplit
route_tree=ast.parse((ROOT/'hermes_cli/route_identity.py').read_text())
route_nodes=[n for n in route_tree.body if isinstance(n,ast.FunctionDef) and n.name=='normalize_route_base_url']
route_nodes += [n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name=='get_custom_provider_extra_headers']
ns.update(urlsplit=urlsplit,urlunsplit=urlunsplit)
exec(compile(ast.Module(body=route_nodes,type_ignores=[]),'route-headers','exec'),ns)
urls=['', 'relative', ' https://EXAMPLE.com/v1', 'https://example.com/v1\n']
urls += [f'{scheme}://{host}{path}' for scheme,host,path in itertools.product(
    ['HTTP','https','custom'],
    ['EXAMPLE.com','example.com:80','example.com:443','User:Pass@EXAMPLE.com:00443','example.com:','example.com:65536','example.com:bad','[2001:DB8::1]','[fe80::1%ETH0]:443','éXAMPLE.com','example.com：443'],
    ['', '/', '/v1/', '/v1//', '/V1', '/v1/?', '/v1?x=1#fragment', '/a/../b', '/%2F'])]
route_rows=[dict(url=url,result=ns['normalize_route_base_url'](url)) for url in urls]
header_rows=[]
for target,saved in itertools.product(['https://example.com/v1','https://EXAMPLE.com:443/v1/','https://other.com/v1','https://example.com/v1?'],['https://EXAMPLE.com:443/v1/','https://example.com/v1//','https://example.com/V1','https://example.com/v1?']):
    for keyed in [False,True]:
        entry={'name':'lab','base_url':saved,'extra_headers':{'X-Fixture':'token-fixture','X-Bool':False,'Drop':None}}
        config={'providers':{'lab':entry}} if keyed else {'custom_providers':[entry]}
        header_rows.append(dict(config=config,url=target,result=ns['get_custom_provider_extra_headers'](target,config=config)))
path=ROOT/'rust/tools/provider-route-headers-goldens.json'
text=json.dumps(dict(routes=route_rows,headers=header_rows),indent=2)+'\n'
if sys.argv[1:]==['--check']: assert path.read_text()==text
else: path.write_text(text)
print(f'Verified {len(route_rows)} route identities and {len(header_rows)} header selections')
