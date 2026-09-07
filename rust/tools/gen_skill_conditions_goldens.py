#!/usr/bin/env python3
"""Execute actual Python condition extraction and visibility for JSON inputs."""
import ast
import json
from pathlib import Path
import sys
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/skill-conditions-goldens.json'
ns = dict(Dict=dict, Any=object, List=list)
for path, name in [('agent/skill_utils.py','extract_skill_conditions'), ('agent/prompt_builder.py','_skill_should_show')]:
    nodes = ast.parse((ROOT/path).read_text()).body
    node = next(n for n in nodes if isinstance(n, ast.FunctionDef) and n.name==name)
    exec(compile(ast.Module(body=[node], type_ignores=[]),path,'exec'),ns)
cases = []
metadata = [None, False, 'bad', [], {}, {'hermes': None}, {'hermes': 'bad'}]
for key in ['fallback_for_toolsets','requires_toolsets','fallback_for_tools','requires_tools','session_platforms']:
    for value in [None, False, 1, '', 'x', [], ['x'], ['x','y'], [None,True], [[]], {'x':1}, [' CLI ', '\u001c'], ['x',[]]]:
        metadata.append({'hermes':{key:value}})
for md in metadata:
    frontmatter = dict(metadata=md)
    conditions = ns['extract_skill_conditions'](frontmatter)
    for tools, toolsets in [(None,None),([],None),(None,[]),(['x'],[]),([],['x']),(['x'],['x'])]:
        for platform in [None,'','cli','x',' ']:
            case = dict(frontmatter=frontmatter, conditions=conditions, tools=tools, toolsets=toolsets, platform=platform)
            try:
                case['expected'] = ns['_skill_should_show'](conditions, None if tools is None else set(tools), None if toolsets is None else set(toolsets), platform)
            except (TypeError, AttributeError):
                case['expected'] = 'error'
            cases.append(case)
content = json.dumps(cases,indent=2)+'\n'
if sys.argv[1:]==['--check']:
    if OUT.read_text()!=content: raise SystemExit('Skill condition fixtures differ')
elif not sys.argv[1:]: OUT.write_text(content)
else: raise SystemExit('usage: gen_skill_conditions_goldens.py [--check]')
print(f'Verified {len(cases)} Python skill condition cases')
