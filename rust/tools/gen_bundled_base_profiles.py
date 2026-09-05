#!/usr/bin/env python3
"""Translate fully declarative bundled profiles into embedded native definitions."""
import ast
from dataclasses import asdict
import importlib.util
import json
import sys
from types import ModuleType
from gen_managed_capability_goldens import REPO

OUT = REPO / "rust/tools/bundled-base-profiles.json"
MODULES = ["alibaba", "alibaba-coding-plan", "arcee", "azure-foundry", "fireworks", "gmi",
           "huggingface", "kilocode", "novita", "openai-codex", "stepfun", "xai", "xiaomi"]
VERSION = "__HERMES_NATIVE_VERSION__"


def generate():
    spec = importlib.util.spec_from_file_location("base_profile_reference", REPO / "providers/base.py")
    base = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = base
    spec.loader.exec_module(base)
    providers = ModuleType("providers")
    hermes_cli = ModuleType("hermes_cli")
    hermes_cli.__version__ = VERSION
    replacements = {"providers": providers, "providers.base": base, "hermes_cli": hermes_cli}
    previous = {name: sys.modules.get(name) for name in replacements}
    result = []
    try:
        sys.modules.update(replacements)
        for name in MODULES:
            path = REPO / "plugins/model-providers" / name / "__init__.py"
            tree = ast.parse(path.read_text())
            if any(isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) for node in tree.body):
                raise ValueError(f"{name} now defines behavior requiring a native hook port")
            registrations = []
            providers.register_provider = registrations.append
            exec(compile(tree, str(path), "exec"), {"__name__": f"native_definition_{name}"})
            definitions = []
            for profile in registrations:
                if type(profile) is not base.ProviderProfile:
                    raise TypeError(f"{name}: inherited provider behavior requires a native implementation")
                if set(vars(profile)) != set(profile.__dataclass_fields__):
                    raise ValueError(f"{name}: runtime extensions require a native implementation")
                row = asdict(profile)
                temperature = profile.fixed_temperature
                row["fixed_temperature"] = ({"kind": "inherit"} if temperature is None else
                                            {"kind": "omit"} if temperature is base.OMIT_TEMPERATURE else
                                            {"kind": "fixed", "value": temperature})
                definitions.append(row)
            result.append(dict(module=name, profiles=definitions))
    finally:
        for name, old in previous.items():
            if old is None: sys.modules.pop(name, None)
            else: sys.modules[name] = old
        sys.modules.pop(spec.name, None)
    return json.dumps(result, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content: raise SystemExit("Bundled native base definitions differ from source")
    elif sys.argv[1:]: raise SystemExit("Usage: gen_bundled_base_profiles.py [--check]")
    else: OUT.write_text(content)
    print("Verified", sum(len(item["profiles"]) for item in json.loads(content)), "profiles in", len(MODULES), "modules")
