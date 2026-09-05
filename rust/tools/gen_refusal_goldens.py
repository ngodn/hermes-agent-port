#!/usr/bin/env python3
"""Execute the Python chat response normalizer for refusal payload comparisons."""
import ast
import json
import sys
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/refusal-goldens.json"
tree = ast.parse((ROOT / "agent/transports/chat_completions.py").read_text())
transport = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "ChatCompletionsTransport")
method = next(n for n in transport.body if isinstance(n, ast.FunctionDef) and n.name == "normalize_response")
module = ast.Module(body=[ast.ImportFrom(module="__future__", names=[ast.alias(name="annotations")], level=0), method], type_ignores=[])
ast.fix_missing_locations(module)
namespace = {"NormalizedResponse": SimpleNamespace, "ToolCall": SimpleNamespace, "Usage": SimpleNamespace}
exec(compile(module, "chat_completions.py", "exec"), namespace)
normalize = namespace["normalize_response"]
rows = []
for content in [None, "", " \t\n", "usable answer", "\u001c", "\u200b"]:
    for refusal in [None, "", " \t", "Provider declined this request."]:
        for tools in [False, True]:
            message = {"role": "assistant", "content": content, "refusal": refusal}
            kwargs = message.copy()
            if tools:
                call = {"id": "clock", "type": "function", "function": {"name": "current_time", "arguments": "{}"}}
                message["tool_calls"] = [call]
                kwargs["tool_calls"] = [SimpleNamespace(id=call["id"], function=SimpleNamespace(**call["function"]))]
            response = SimpleNamespace(choices=[SimpleNamespace(message=SimpleNamespace(**kwargs), finish_reason="stop")], usage=None)
            result = normalize(SimpleNamespace(_last_wire_aliases={}), response)
            rows.append({"message": message, "tool_calls": bool(result.tool_calls), "content": result.content if isinstance(result.content, str) else ""})
text = json.dumps(rows, ensure_ascii=False, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    if OUT.read_text() != text:
        raise SystemExit("Refusal fixtures differ from Python source")
    print(f"Verified {len(rows)} refusal normalization cases")
elif not sys.argv[1:]:
    OUT.write_text(text)
    print(f"Wrote {len(rows)} refusal normalization cases")
else:
    raise SystemExit("usage: gen_refusal_goldens.py [--check]")
