#!/usr/bin/env python3
"""Execute the Python stored-prompt identity guard without loading the agent."""
import ast
import json
import sys
from pathlib import Path
from types import SimpleNamespace

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/conversation_loop.py").read_text())
method = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
              and node.name == "_stored_prompt_matches_runtime")
scope = {}
exec(compile(ast.Module(body=[method], type_ignores=[]), "agent/conversation_loop.py", "exec"), scope)
prompts = [
    [],
    ["Model: model", "Provider: provider", "Platform: cli"],
    ["Model: wrong", "Model: model", "Model: "],
    ["Model: wrong", "Model: model"],
    ["Model: model", "Provider: wrong", "Provider: provider", "Platform: desktop"],
    ["User home directory: /home/user", "Current working directory: /work",
     "project context", "Current working directory: /project/prose"],
    ["Current working directory: /project/prose"],
    ["User home directory: /home/user", "one", "two", "Current working directory: /work"],
    ["User home directory: /home/user", "one", "two", "three", "Current working directory: /wrong"],
    ["User home directory: /home/user", "Current working directory: /wrong",
     "User home directory: /other", "Current working directory: /work"],
    ["User home directory: /home/user", "one", "two", "three",
     "User home directory: /other", "Current working directory: /work"],
    [" Model: wrong", "Provider:\u001cprovider", "Platform:\u00a0cli\u00a0"],
]
baseline = dict(model="model", provider="provider", platform="cli", cwd="/work")
runtimes = [baseline, dict.fromkeys(baseline, "")]
for field in baseline:
    runtimes.append({**baseline, field: "different"})
runtimes.append({**baseline, "model": " model ", "cwd": " /work "})
cases = []
for separator in ["\n", "\r\n", "\r", "\v", "\f", "\x1c", "\x1d", "\x1e", "\x85", "\u2028", "\u2029"]:
    for lines in prompts:
        prompt = separator.join(lines)
        for runtime in runtimes:
            scope["resolve_agent_cwd"] = lambda: runtime["cwd"]
            matches = scope["_stored_prompt_matches_runtime"](SimpleNamespace(**runtime), prompt)
            cases.append(dict(prompt=prompt, runtime=runtime, matches=matches))
path = root / "rust/tools/stored-prompt-runtime-goldens.json"
text = json.dumps(cases, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit("usage: gen_stored_prompt_runtime_goldens.py [--check]")
print(f"Verified {len(cases)} stored prompt runtime decisions")
