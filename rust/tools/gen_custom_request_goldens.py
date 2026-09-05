#!/usr/bin/env python3
"""Generate golden cases for custom_request_config.rs.

This executes the *real* selection logic from ``agent/agent_init.py`` so the
Rust port can be checked against CPython behaviour rather than a paraphrase.
Rather than importing the module (which drags in the whole agent runtime), we
pull the three pure functions out of the source with ``ast`` and exec just
those, so the goldens track the actual source of
``_normalized_custom_base_url`` / ``_custom_provider_model_matches`` /
``_custom_provider_extra_body_for_agent``.

Run with the pinned interpreter:

    mise x python@3.12.13 -- python rust/tools/gen_custom_request_goldens.py

Writes ``rust/tools/custom-request-goldens.json`` next to this script.
"""
from __future__ import annotations

import ast
import json
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]
SOURCE = REPO_ROOT / "agent" / "agent_init.py"
OUT = Path(__file__).resolve().parent / "custom-request-goldens.json"

WANTED = {
    "_normalized_custom_base_url",
    "_custom_provider_model_matches",
    "_custom_provider_extra_body_for_agent",
}


def load_functions() -> Dict[str, Any]:
    """Extract the three target functions from the source and exec them."""
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    segments: List[str] = []
    found: set[str] = set()
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name in WANTED:
            segments.append(ast.unparse(node))
            found.add(node.name)
    missing = WANTED - found
    if missing:
        raise SystemExit(f"could not find in source: {sorted(missing)}")
    namespace: Dict[str, Any] = {
        "Any": Any,
        "Dict": Dict,
        "List": List,
        "Optional": Optional,
    }
    exec("\n\n".join(segments), namespace)  # noqa: S102 - trusted local source
    return namespace


def cases() -> List[Dict[str, Any]]:
    """Hand-authored inputs covering the behaviours the port must preserve."""
    body = {"service_tier": "flex"}
    other = {"reasoning_effort": "high"}
    return [
        # provider gate: only custom / custom:<name> participate.
        {"name": "provider-not-custom",
         "provider": "openai", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"base_url": "http://x", "extra_body": body}]},
        {"name": "provider-empty",
         "provider": "", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"base_url": "http://x", "extra_body": body}]},
        # base_url gate: empty target short-circuits to None.
        {"name": "base-url-empty",
         "provider": "custom", "model": "gpt-4", "base_url": "   ",
         "entries": [{"base_url": "http://x", "extra_body": body}]},
        # plain fallback (entry has no model): returned as-is.
        {"name": "fallback-no-model",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"base_url": "http://x", "extra_body": body}]},
        # exact URL matching: trailing slash + whitespace normalized both sides.
        {"name": "url-trailing-slash",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x/",
         "entries": [{"base_url": "http://x", "extra_body": body}]},
        {"name": "url-many-slashes-and-ws",
         "provider": "custom", "model": "gpt-4", "base_url": "  http://x///  ",
         "entries": [{"base_url": "http://x", "extra_body": body}]},
        {"name": "url-mismatch",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"base_url": "http://y", "extra_body": body}]},
        # named filter matches provider_key or name.
        {"name": "named-matches-provider-key",
         "provider": "custom:foo", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"provider_key": "foo", "base_url": "http://x", "extra_body": body}]},
        {"name": "named-matches-name",
         "provider": "custom:foo", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"name": "Foo", "base_url": "http://x", "extra_body": body}]},
        {"name": "named-filter-ws-and-case",
         "provider": "custom:  Foo  ", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"provider_key": "FOO", "base_url": "http://x", "extra_body": body}]},
        {"name": "named-no-match",
         "provider": "custom:bar", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"provider_key": "foo", "base_url": "http://x", "extra_body": body}]},
        # model catalog priority: dict keys.
        {"name": "catalog-dict-hit",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"model": "other", "models": {"GPT-4": {}}, "base_url": "http://x", "extra_body": body}]},
        # model catalog: list, with numeric coercion of an element.
        {"name": "catalog-list-numeric",
         "provider": "custom", "model": "123", "base_url": "http://x",
         "entries": [{"model": "other", "models": [123], "base_url": "http://x", "extra_body": body}]},
        # provider_model equality path (no catalog), case-insensitive.
        {"name": "provider-model-ci",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"model": "GPT-4", "base_url": "http://x", "extra_body": body}]},
        # provider_model set but no match -> not returned, no fallback -> None.
        {"name": "provider-model-no-match",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"model": "gpt-3", "base_url": "http://x", "extra_body": body}]},
        # matching model wins over an earlier fallback entry.
        {"name": "model-match-beats-earlier-fallback",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [
             {"base_url": "http://x", "extra_body": other},
             {"model": "gpt-4", "base_url": "http://x", "extra_body": body},
         ]},
        # first matching fallback wins among several.
        {"name": "first-fallback-wins",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [
             {"base_url": "http://x", "extra_body": body},
             {"base_url": "http://x", "extra_body": other},
         ]},
        # models-only entry (no model key) counts as a fallback, not a match.
        {"name": "models-only-is-fallback",
         "provider": "custom", "model": "zzz", "base_url": "http://x",
         "entries": [{"models": {"gpt-4": {}}, "base_url": "http://x", "extra_body": body}]},
        # empty extra_body is skipped.
        {"name": "empty-extra-body-skipped",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [
             {"base_url": "http://x", "extra_body": {}},
             {"base_url": "http://x", "extra_body": body},
         ]},
        # non-dict extra_body is skipped.
        {"name": "non-dict-extra-body-skipped",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": [
             {"base_url": "http://x", "extra_body": "nope"},
             {"base_url": "http://x", "extra_body": body},
         ]},
        # non-dict entry is skipped.
        {"name": "non-dict-entry-skipped",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": ["nope", {"base_url": "http://x", "extra_body": body}]},
        # uppercase provider normalizes to custom.
        {"name": "provider-uppercase",
         "provider": "CUSTOM", "model": "gpt-4", "base_url": "http://x",
         "entries": [{"base_url": "http://x", "extra_body": body}]},
        # empty entries list -> None.
        {"name": "no-entries",
         "provider": "custom", "model": "gpt-4", "base_url": "http://x",
         "entries": []},
        # catalog present but miss, and provider_model empty -> matches() False,
        # but since provider_model is empty the entry is a fallback anyway.
        {"name": "catalog-miss-but-fallback",
         "provider": "custom", "model": "nope", "base_url": "http://x",
         "entries": [{"models": ["a", "b"], "base_url": "http://x", "extra_body": body}]},
    ]


def main() -> None:
    ns = load_functions()
    select = ns["_custom_provider_extra_body_for_agent"]
    out = []
    for case in cases():
        expected = select(
            provider=case["provider"],
            model=case["model"],
            base_url=case["base_url"],
            custom_providers=case["entries"],
        )
        out.append({
            "name": case["name"],
            "provider": case["provider"],
            "model": case["model"],
            "base_url": case["base_url"],
            "entries": case["entries"],
            "expected": expected,
        })
    content = json.dumps(out, indent=2) + "\n"
    if sys.argv[1:] == ["--check"]:
        assert OUT.read_text(encoding="utf-8") == content, "Custom request fixtures differ from Python"
    elif not sys.argv[1:]:
        OUT.write_text(content, encoding="utf-8")
    else:
        raise SystemExit("usage: gen_custom_request_goldens.py [--check]")
    print(f"Verified {len(out)} custom request cases")


if __name__ == "__main__":
    main()
