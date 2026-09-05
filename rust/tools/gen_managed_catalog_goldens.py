#!/usr/bin/env python3
"""Execute the Python curated catalog loader and check the packaged Rust copy."""
from copy import deepcopy
from dataclasses import asdict, dataclass, field
import json
from pathlib import Path, PurePosixPath
import re
import sys
import types
from gen_managed_capability_goldens import extracted, REPO

OUT = REPO / "rust/tools/managed-catalog-goldens.json"
COPY = REPO / "rust/tools/managed-catalog.json"


def generate():
    catalog = types.ModuleType("catalog_loader_oracle")
    sys.modules[catalog.__name__] = catalog
    catalog.__dict__.update(dataclass=dataclass, field=field, PurePosixPath=PurePosixPath,
                            _SCHEMA_VERSION=1, _PART_SUFFIX=re.compile(r"-\d{5}-of-\d{5}$"))
    extracted("hermes_cli/local_runtime/catalog.py",
              {"AssetFile", "QuantVariant", "CatalogEntry", "_asset_from", "_load_catalog"}, catalog.__dict__)
    packaged = json.loads((REPO / "hermes_cli/local_runtime/catalog.json").read_text())
    minimal = dict(schema_version=1, models=[dict(
        id="a", display_name="A", description="", repo="fixture",
        variants=[dict(quant="q", files=[dict(path="a.gguf", size_bytes=1)])],
        n_ctx_train=100, full_layers=1, recurrent_layers=0, per_layer_f16=1)])
    docs = [packaged, minimal, dict(schema_version=1, models=[]), {},
            dict(schema_version=2, models=[])]
    for field_name, values in {
        "n_ctx_train": ["12", "1_024", "١٢", 12.9, True, None, "oops", "1__2", "_1", "1_", "  +١_٢  "],
        "min_engine": [None, False, 123, "b123", ["x", True]],
        "moe": [None, False, True, "false", [], [0]],
        "sampling": [None, [], [["x", 1]], ["ab"], {"x": 1}],
        "decode_fraction": ["0.5", "1_0.5", "1__0", "١.٥", False, None],
    }.items():
        for value in values:
            doc = deepcopy(minimal)
            doc["models"][0][field_name] = value
            docs.append(doc)
    for value in [None, {}, [], {"path": "p.gguf", "size_bytes": "12", "local": ""}]:
        doc = deepcopy(minimal)
        doc["models"][0]["mmproj"] = value
        docs.append(doc)
    doc = deepcopy(minimal)
    doc["models"][0]["future_field"] = {"anything": 1}
    docs.append(doc)
    results = []
    for doc in docs:
        try:
            models = [asdict(entry) for entry in catalog._load_catalog(doc)]
            expected = dict(schema_version=1, models=models)
        except Exception:
            expected = None
        results.append(dict(input=doc, expected=expected))
    sys.modules.pop(catalog.__name__, None)
    return json.dumps(results, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    source = (REPO / "hermes_cli/local_runtime/catalog.json").read_text()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content or COPY.read_text() != source:
            raise SystemExit("Managed catalog fixtures or packaged copy differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_managed_catalog_goldens.py [--check]")
    else:
        OUT.write_text(content)
        COPY.write_text(source)
    print("Verified", len(json.loads(content)), "managed catalog cases")
