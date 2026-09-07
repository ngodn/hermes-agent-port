#!/usr/bin/env python3
"""Execute Python's profile and provider prompt identity branches."""
import ast
import itertools
import json
import sys
from pathlib import Path
from types import SimpleNamespace

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/system_prompt.py").read_text())
function = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
                and node.name == "build_system_prompt_parts")
def branch(test):
    node = next(node for node in function.body if isinstance(node, ast.If) and ast.unparse(node.test) == test)
    return compile(ast.Module(body=[node], type_ignores=[]), "agent/system_prompt.py", "exec")
profile_code = branch("active_profile == 'default'")
provider_code = branch("agent.provider == 'alibaba'")
profiles = []
for profile, home, default_root in itertools.product(
    ["default", "red", "custom"], ["/hermes", "/hermes/profiles/red", "/自定义 home"], ["/hermes", "/default root"]
):
    scope = dict(active_profile=profile, _home_str=home, _root_str=default_root,
                 get_default_hermes_root=lambda: default_root, post_workspace_parts=[])
    exec(profile_code, scope)
    profiles.append(dict(profile=profile, home=home, root=default_root, expected=scope["post_workspace_parts"][0]))
providers = []
for provider, model in itertools.product(["alibaba", "Alibaba", "openai", ""],
                                         ["", "qwen", "org/qwen", "org/nested/qwen", "org/"]):
    scope = dict(agent=SimpleNamespace(provider=provider, model=model), stable_parts=[])
    exec(provider_code, scope)
    providers.append(dict(provider=provider, model=model, expected=scope["stable_parts"][0] if scope["stable_parts"] else None))
path = root / "rust/tools/prompt-identity-goldens.json"
text = json.dumps(dict(profiles=profiles, providers=providers), indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_prompt_identity_goldens.py [--check]")
print(f"Verified {len(profiles)} profile and {len(providers)} provider identity cases")
