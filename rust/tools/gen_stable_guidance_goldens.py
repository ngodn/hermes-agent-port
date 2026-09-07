#!/usr/bin/env python3
"""Run Python's initial stable-tier selection against resolved agent fixtures."""
import ast
import hashlib
import itertools
import json
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import List, Optional

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/system_prompt.py").read_text())
function = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "build_system_prompt_parts")
prefix = []
for node in function.body:
    if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "has_skills_tools" for t in node.targets):
        break
    prefix.append(node)
prefix.append(next(n for n in function.body if isinstance(n, ast.If) and ast.unparse(n.test).startswith("_has_skill_view and")))
scope = json.loads((root / "rust/tools/system-prompt-guidance.json").read_text())
scope.update(List=List, Optional=Optional)
builder = ast.parse((root / "agent/prompt_builder.py").read_text())
execution = next(n for n in builder.body if isinstance(n, ast.FunctionDef) and n.name == "execution_guidance_text")
exec(compile(ast.Module(body=[execution], type_ignores=[]), "agent/prompt_builder.py", "exec"), scope)
module = ModuleType("agent.prompt_builder")
module.execution_guidance_text = scope["execution_guidance_text"]
sys.modules["agent.prompt_builder"] = module
code = compile(ast.Module(body=prefix, type_ignores=[]), "agent/system_prompt.py", "exec")
cases = []
toolsets = [[], ["terminal"], ["memory", "session_search", "skill_manage", "kanban_show"], ["skill_view", "web_search"]]
settings_list = [{}, {"_task_completion_guidance": False, "_parallel_tool_call_guidance": False},
                 {"_memory_enabled": False}, {"_memory_enabled": False, "_user_profile_enabled": False},
                 {"_kanban_worker_guidance": ""}, {"_kanban_worker_guidance": " custom worker guidance "},
                 {"_tool_use_enforcement": False, "_execution_guidance": True},
                 {"_tool_use_enforcement": True, "_execution_guidance": False}]
for tools, model, settings in itertools.product(toolsets, ["llama", "gemini-3", "gpt-5"], settings_list):
    for soul, skills_index in [(None, ""), (" scoped soul ", "- hermes-agent: reference")]:
        agent = SimpleNamespace(load_soul_identity=True, skip_context_files=False, valid_tool_names=tools,
                                model=model, _tool_use_enforcement="auto")
        agent.__dict__.update(settings)
        scope.update(agent=agent, system_message=None, skills_prompt=skills_index,
                     _ra=lambda: SimpleNamespace(load_soul_md=lambda *args, **kwargs: soul), _agent_home=lambda agent: None)
        exec(code, scope)
        # Hash the ordered raw sections, including Python's single-space tool
        # guidance join. This keeps fixtures compact without normalizing bytes.
        encoded = json.dumps(scope["stable_parts"], ensure_ascii=False, separators=(",", ":"))
        cases.append(dict(tools=tools, model=model, settings=settings, soul=soul, skills_index=skills_index,
                          sha256=hashlib.sha256(encoded.encode()).hexdigest()))
path = root / "rust/tools/stable-guidance-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_stable_guidance_goldens.py [--check]")
print(f"Verified {len(cases)} initial stable guidance assemblies")
