#!/usr/bin/env python3
"""Export the source command catalog and verify Slack bang-command recognition."""
import ast
import dataclasses
import json
from pathlib import Path
import sys

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / 'hermes_cli/commands.py').read_text())
registry_nodes = [n for n in tree.body if
    isinstance(n, ast.ClassDef) and n.name == 'CommandDef' or
    isinstance(n, ast.AnnAssign) and isinstance(n.target, ast.Name) and n.target.id in {'COMMAND_REGISTRY', 'GATEWAY_KNOWN_COMMANDS'} or
    isinstance(n, ast.FunctionDef) and n.name == 'is_gateway_known_command']
ns = {'dataclass': dataclasses.dataclass, '_iter_plugin_command_entries': lambda: [], '__name__': __name__}
constants = ast.parse((root / 'hermes_constants.py').read_text())
ns['INDICATOR_STYLES'] = ast.literal_eval(next(n.value for n in constants.body if isinstance(n, ast.AnnAssign) and isinstance(n.target, ast.Name) and n.target.id == 'INDICATOR_STYLES'))
exec(compile(ast.Module(body=registry_nodes, type_ignores=[]), 'command-registry-source', 'exec'), ns)

def emit(path, value):
    text = json.dumps(value, ensure_ascii=False, indent=2) + '\n'
    if sys.argv[1:] == ['--check']:
        assert path.read_text() == text, path
    else:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

emit(root / 'rust/crates/hermes-gateway/data/commands.json', [dataclasses.asdict(c) for c in ns['COMMAND_REGISTRY']])
slack = ast.parse((root / 'plugins/platforms/slack/adapter.py').read_text())
rewrite = next(n for n in slack.body if isinstance(n, ast.FunctionDef) and n.name == '_rewrite_known_bang_command')
# Use the actual registry predicate without importing the Python application.
class RemoveRegistryImport(ast.NodeTransformer):
    def visit_ImportFrom(self, node):
        assert node.module == 'hermes_cli.commands'
        return ast.copy_location(ast.Pass(), node)
rewrite = RemoveRegistryImport().visit(rewrite)
exec(compile(ast.fix_missing_locations(ast.Module(body=[rewrite], type_ignores=[])), 'slack-bang-source', 'exec'), ns)
texts = ['!', '!nice work', '!/help', '!unknown', 'hello!', ' !help', '! help', '!HELP@bot args', '!help\x1carg']
for command in ns['COMMAND_REGISTRY']:
    for name in (command.name, *command.aliases):
        texts.extend([f'!{name} argument', f'!{name.upper()}@bot argument'])
rows = [{'text': t, 'result': ns['_rewrite_known_bang_command'](t)} for t in texts]
emit(root / 'rust/tools/slack-bang-goldens.json', rows)
print(f'Verified {len(ns["COMMAND_REGISTRY"])} catalog entries and {len(rows)} bang-command cases')
