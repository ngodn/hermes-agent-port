#!/usr/bin/env python3
"""Execute Python's tier finalization and model-guidance gate for Rust fixtures."""
import ast
import itertools
import json
import sys
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Optional

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/system_prompt.py").read_text())
functions = {node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)}
parts_fn = functions["build_system_prompt_parts"]
gate = next(node for node in parts_fn.body if isinstance(node, ast.If)
            and isinstance(node.body[0], ast.Assign)
            and any(isinstance(target, ast.Name) and target.id == "_enforce" for target in node.body[0].targets))
scope = dict(Any=Any, Optional=Optional, drain_truncation_warnings=lambda: [])
exec(compile(ast.Module(body=[functions["build_system_prompt"]], type_ignores=[]), "agent/system_prompt.py", "exec"), scope)
finalize = ast.parse("def finalize():\n    pass\n").body[0]
finalize.body = [parts_fn.body[-1]]
exec(compile(ast.fix_missing_locations(ast.Module(body=[finalize], type_ignores=[])), "agent/system_prompt.py", "exec"), scope)
gate_code = compile(ast.Module(body=[gate], type_ignores=[]), "agent/system_prompt.py", "exec")
tiers = []
for sections in itertools.product([[], [""], ["\u001c identity \u00a0", "", "  exact\n interior  "], ["\t \n", " context "]], repeat=3):
    scope.update(zip(["stable_parts", "context_parts", "volatile_parts"], sections))
    parts = scope["finalize"]()
    scope["build_system_prompt_parts"] = lambda *args, **kwargs: parts
    agent = SimpleNamespace()
    joined = scope["build_system_prompt"](agent)
    assert agent._cached_system_prompt_static == parts["stable"]
    tiers.append(dict(sections=sections, parts=parts, joined=joined))
gates = []
settings = [None, True, False, 0, 1, "auto", "TRUE", "never", "yes", "off", " true ", "unknown", [], [""], ["GPT", None, 3], ["qwen", "gemini"]]
for setting, model, defaults in itertools.product(settings, ["", "gpt-5", "Qwen3", "Gemini-3", "llama"], [[], ["gpt", "qwen"], ["GPT"]]):
    scope.update(agent=SimpleNamespace(valid_tool_names={"terminal"}, _tool_use_enforcement=setting, model=model),
                 stable_parts=[], TOOL_USE_ENFORCEMENT_MODELS=defaults,
                 TOOL_USE_ENFORCEMENT_GUIDANCE="enforcement", GOOGLE_MODEL_OPERATIONAL_GUIDANCE="google")
    exec(gate_code, scope)
    gates.append(dict(setting=setting, model=model, defaults=defaults, enabled="enforcement" in scope["stable_parts"]))
path = root / "rust/tools/system-prompt-assembly-goldens.json"
text = json.dumps(dict(tiers=tiers, gates=gates), indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_system_prompt_assembly_goldens.py [--check]")
print(f"Verified {len(tiers)} tier joins and {len(gates)} model-guidance gates")
