#!/usr/bin/env python3
"""Extract and execute the actual Python skills-index rendering block."""
import ast
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/skills-index-goldens.json'
TEMPLATE = ROOT / 'rust/tools/skills-index-template.json'


def render(categories, descriptions, compact, tools):
    tree = ast.parse((ROOT / 'agent/prompt_builder.py').read_text())
    function = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_build_skills_system_prompt_inner')
    start = next(i for i,n in enumerate(function.body) if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'demoted' for t in n.targets))
    end = next(i for i in range(start, len(function.body)) if isinstance(function.body[i], ast.With))
    scope = dict(skills_by_category=categories, category_descriptions=descriptions, compact_categories=compact, available_tools=tools)
    exec(compile(ast.Module(body=function.body[start:end], type_ignores=[]), 'agent/prompt_builder.py', 'exec'), scope)
    return scope['result']


def generate():
    cases = []
    for categories, descriptions, compact in [
        ({}, {}, []),
        ({'general': [('one', '')]}, {}, []),
        ({'coding': [('z', 'last'), ('a', 'first'), ('a', 'ignored')]}, {'coding': 'Build things'}, []),
        ({'social/twitter': [('z', 'hidden'), ('a', 'hidden'), ('a', '')], 'coding': [('fix', 'repair')]}, {}, ['social']),
        ({'你好': [('🦀', 'Café\nsecond line'), ('a', ' x ')]}, {'你好': ' Unicode '}, []),
        ({'empty': []}, {}, ['empty']),
        ({'org:team': [('same', 'first'), ('same', 'second')], 'general': [('same', 'personal')]}, {}, []),
    ]:
        for tools in [None, [], ['web_search']]:
            case = dict(categories=categories, descriptions=descriptions, compact=compact, tools=tools)
            case['expected'] = render(categories, descriptions, compact, tools)
            cases.append(case)
    normal = render({'__CATEGORY__': [('__NAME__', '__DESCRIPTION__')]}, {}, [], None)
    marker = '  __CATEGORY__:\n    - __NAME__: __DESCRIPTION__'
    prefix, suffix = normal.split(marker)
    compact = render({'x': [('a', '')]}, {}, ['x'], None)
    base = render({'x': [('a', '')]}, {}, [], None)
    # The explanatory note follows the same closing instruction in both forms.
    note = compact.split(base.split('</available_skills>')[1], 1)[1]
    return cases, dict(prefix=prefix, suffix=suffix, compact_note=note)


if __name__ == '__main__':
    if sys.argv[1:] not in ([], ['--check']):
        raise SystemExit('usage: gen_skills_index_goldens.py [--check]')
    cases, template = generate()
    for path, data in [(OUT, cases), (TEMPLATE, template)]:
        content = json.dumps(data, indent=2) + '\n'
        if sys.argv[1:]:
            if path.read_text() != content:
                raise SystemExit(f'{path.name} differs')
        else:
            path.write_text(content)
    print(f'Verified {len(cases)} actual Python skills-index cases')
