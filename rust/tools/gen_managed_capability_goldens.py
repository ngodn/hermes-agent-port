#!/usr/bin/env python3
"""Execute staged-file and managed-vision source on temporary filesystem trees."""
import ast
from dataclasses import dataclass, field
import importlib.util
import itertools
import json
from pathlib import Path, PurePosixPath
import re
import sys
import tempfile
import types

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/managed-capability-goldens.json"


def extracted(path, names, scope):
    tree = ast.parse((REPO / path).read_text())
    nodes = [n for n in tree.body if isinstance(n, (ast.FunctionDef, ast.ClassDef)) and n.name in names]
    module = ast.Module(body=[ast.parse("from __future__ import annotations").body[0]] + nodes, type_ignores=[])
    exec(compile(module, path + " (oracle)", "exec"), scope)


def generate():
    catalog = types.ModuleType("managed_catalog_oracle")
    sys.modules[catalog.__name__] = catalog
    catalog.__dict__.update(dataclass=dataclass, field=field, PurePosixPath=PurePosixPath,
                            _SCHEMA_VERSION=1, _PART_SUFFIX=re.compile(r"-\d{5}-of-\d{5}$"))
    extracted("hermes_cli/local_runtime/catalog.py",
              {"AssetFile", "QuantVariant", "CatalogEntry", "_asset_from", "_load_catalog", "find_entry_for_model"}, catalog.__dict__)
    spec = importlib.util.spec_from_file_location("managed_vision_oracle", REPO / "hermes_cli/local_runtime/capabilities.py")
    capabilities = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(capabilities)
    bootstrap = types.ModuleType("hermes_cli.local_runtime.bootstrap")
    extracted("hermes_cli/local_runtime/bootstrap.py", {"staged_models", "staged_model_ids"}, bootstrap.__dict__)
    replacements = {"hermes_cli.local_runtime.bootstrap": bootstrap, "hermes_cli.local_runtime.catalog": catalog}
    originals = {name: sys.modules.get(name) for name in replacements}
    sys.modules.update(replacements)
    staging = []
    cases = []
    layouts = [[], ["a.gguf"], ["z.gguf", "a.gguf", "B.GGUF"],
               ["a-00001-of-00002.gguf"], ["a-00002-of-00002.gguf"],
               ["a-00001-of-00002.gguf", "a-00002-of-00002.gguf"],
               ["a-00001-of-00003.gguf", "a-00003-of-00003.gguf"],
               ["a-00001-of-00000.gguf"], ["a-00001-of-00001.gguf"],
               ["a-00001-of-0000٢.gguf", "a-00002-of-0000٢.gguf"],
               ["assets/projector.gguf", ".hidden.gguf", "nested/model.gguf"],
               ["a.gguf", "a-00001-of-00001.gguf"], ["dir.gguf/"], [".gguf", "a..gguf"]]
    try:
        for layout in layouts:
            with tempfile.TemporaryDirectory() as temp:
                models = Path(temp) / "models"
                models.mkdir()
                bootstrap.models_dir = lambda: models
                for name in layout:
                    target = models / name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    if name.endswith("/"):
                        target.mkdir()
                    else:
                        target.touch()
                staging.append(dict(files=layout, expected=bootstrap.staged_model_ids()))
        entry = dict(id="fixture", display_name="Fixture", description="", repo="fixture",
                     n_ctx_train=100, full_layers=1, recurrent_layers=0, per_layer_f16=1,
                     variants=[dict(quant="q4", files=[dict(path="weights/a.gguf", size_bytes=0)])])
        for staged, live, projector, present in itertools.product(
            [False, True], [None, False, True], [None, {"path": "path/p.gguf", "size_bytes": 0},
                                              {"path": "p.gguf", "size_bytes": 0, "local": "chosen.gguf"}], [False, True]
        ):
            doc = dict(schema_version=1, models=[dict(entry, mmproj=projector)])
            catalog.CATALOG = catalog._load_catalog(doc)
            with tempfile.TemporaryDirectory() as temp:
                models = Path(temp) / "models"
                models.mkdir()
                bootstrap.models_dir = lambda: models
                bootstrap.assets_dir = lambda: models / "assets"
                if staged:
                    (models / "a.gguf").touch()
                if projector and present:
                    target = bootstrap.assets_dir() / catalog.CATALOG[0].mmproj.local_name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.touch()
                calls = []
                def props(model):
                    calls.append(model)
                    return live
                capabilities._props_modalities = props
                result = capabilities.managed_model_supports_vision("a")
                cases.append(dict(staged=staged, live=live, catalog=doc, projector_present=present,
                                  expected=result, calls=calls))
    finally:
        for name, original in originals.items():
            if original is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = original
        sys.modules.pop(catalog.__name__, None)
    return json.dumps(dict(staging=staging, capabilities=cases), indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Managed capability fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_managed_capability_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified managed cases:", {key: len(value) for key, value in json.loads(content).items()})
