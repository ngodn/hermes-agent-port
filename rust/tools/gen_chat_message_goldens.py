#!/usr/bin/env python3
"""Generate golden cases for chat_message_projection.rs.

This executes the *real* `convert_messages` and `_model_consumes_thought_signature`
from ``agent/transports/chat_completions.py`` so the Rust port is checked against
CPython behaviour rather than a paraphrase. Importing the module would drag in the
whole agent runtime (httpx, provider profiles, ...), so instead we pull just those
two definitions out of the source with ``ast`` and exec them.

``convert_messages`` is a method on ``ChatCompletionsTransport`` but never touches
``self`` (it only reads ``kwargs["model"]`` and calls the module-level
``_model_consumes_thought_signature``), so we exec its ``def`` as a plain function
and call it with ``self=None``.

Run with the pinned interpreter:

    mise x python@3.12.13 -- python rust/tools/gen_chat_message_goldens.py

Writes ``rust/tools/chat-message-goldens.json`` next to this script.
"""
from __future__ import annotations

import ast
import json
import sys
from pathlib import Path
from typing import Any, Dict, List

REPO_ROOT = Path(__file__).resolve().parents[2]
SOURCE = REPO_ROOT / "agent" / "transports" / "chat_completions.py"
OUT = Path(__file__).resolve().parent / "chat-message-goldens.json"

WANTED_FUNC = "_model_consumes_thought_signature"
WANTED_METHOD = "convert_messages"
WANTED_CLASS = "ChatCompletionsTransport"


def load_functions() -> Dict[str, Any]:
    """Extract the two target definitions from the source and exec them.

    The source has no ``from __future__ import annotations``, so the signature
    annotations (``list[dict[str, Any]]``) are evaluated at def time; ``Any`` is
    placed in the namespace to satisfy that.
    """
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    func_src: str | None = None
    method_src: str | None = None
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name == WANTED_FUNC:
            func_src = ast.unparse(node)
        elif isinstance(node, ast.ClassDef) and node.name == WANTED_CLASS:
            for item in node.body:
                if isinstance(item, ast.FunctionDef) and item.name == WANTED_METHOD:
                    method_src = ast.unparse(item)
    missing = [
        name
        for name, src in ((WANTED_FUNC, func_src), (WANTED_METHOD, method_src))
        if src is None
    ]
    if missing:
        raise SystemExit(f"could not find in source: {sorted(missing)}")
    namespace: Dict[str, Any] = {"Any": Any, "Dict": Dict, "List": List}
    # The module-level helper must exist before convert_messages runs; execing
    # both defs into one namespace resolves the reference.
    exec(func_src + "\n\n" + method_src, namespace)  # noqa: S102 - trusted local source
    return namespace


def cases() -> List[Dict[str, Any]]:
    """Hand-authored inputs covering the behaviours the port must preserve."""
    return [
        # nothing to strip: returned unchanged.
        {"name": "clean-passthrough", "model": "gpt-4",
         "messages": [{"role": "user", "content": "hi"}]},
        # non-dict messages pass through untouched, kept in place.
        {"name": "non-dict-messages", "model": "gpt-4",
         "messages": ["raw", 42, None, {"role": "user", "content": "x"}]},
        # each sidecar key individually.
        {"name": "codex-reasoning-items", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "codex_reasoning_items": [1, 2]}]},
        {"name": "codex-message-items", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "codex_message_items": [{"x": 1}]}]},
        {"name": "tool-name", "model": "gpt-4",
         "messages": [{"role": "tool", "content": "res", "tool_name": "search", "tool_call_id": "c1"}]},
        {"name": "effect-disposition", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "effect_disposition": "applied"}]},
        {"name": "timestamp", "model": "gpt-4",
         "messages": [{"role": "assistant", "timestamp": 1700000000, "content": "a"}]},
        {"name": "platform-message-id", "model": "gpt-4",
         "messages": [{"role": "user", "content": "hi", "platform_message_id": "tg:42"}]},
        {"name": "api-content", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "api_content": [{"type": "text"}]}]},
        {"name": "anthropic-content-blocks", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "anthropic_content_blocks": [{"type": "thinking"}]}]},
        {"name": "bedrock-content-blocks", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "bedrock_content_blocks": [{"type": "text"}]}]},
        # all sidecars at once, order-preserving.
        {"name": "all-sidecars", "model": "gpt-4",
         "messages": [{
             "role": "assistant", "codex_reasoning_items": [1], "content": "a",
             "codex_message_items": [2], "tool_name": "t", "effect_disposition": "d",
             "timestamp": 1, "platform_message_id": "p", "api_content": [3],
             "anthropic_content_blocks": [4], "bedrock_content_blocks": [5],
         }]},
        # underscore scaffolding markers: single and multiple, order preserved.
        {"name": "underscore-single", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "_thinking_prefill": True}]},
        {"name": "underscore-multiple", "model": "gpt-4",
         "messages": [{
             "role": "assistant", "_empty_recovery_synthetic": True, "content": "a",
             "_empty_terminal_sentinel": False, "_thinking_prefill": "x",
         }]},
        # sidecar + underscore on the same message.
        {"name": "sidecar-and-underscore", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "timestamp": 1, "_marker": 1}]},
        # extra_content stripped for non-Gemini.
        {"name": "extra-content-stripped-non-gemini", "model": "gpt-4",
         "messages": [{"role": "assistant", "tool_calls": [
             {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"},
              "extra_content": {"thought_signature": "sig"}}]}]},
        # extra_content kept for Gemini.
        {"name": "extra-content-kept-gemini", "model": "gemini-3-pro",
         "messages": [{"role": "assistant", "tool_calls": [
             {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"},
              "extra_content": {"thought_signature": "sig"}}]}]},
        # extra_content kept for Gemma too.
        {"name": "extra-content-kept-gemma", "model": "google/gemma-2-27b",
         "messages": [{"role": "assistant", "tool_calls": [
             {"id": "c1", "extra_content": {"thought_signature": "sig"}}]}]},
        # extra_content present on Gemini but call_id also present: call_id still
        # popped, extra_content preserved.
        {"name": "gemini-strips-call-id-keeps-extra-content", "model": "gemini-3-pro",
         "messages": [{"role": "assistant", "tool_calls": [
             {"id": "c1", "call_id": "rc", "response_item_id": "ri",
              "extra_content": {"thought_signature": "sig"}}]}]},
        # codex tool_call fields stripped.
        {"name": "tool-call-codex-fields", "model": "gpt-4",
         "messages": [{"role": "assistant", "tool_calls": [
             {"id": "c1", "call_id": "rc", "response_item_id": "ri",
              "function": {"name": "f", "arguments": "{}"}}]}]},
        # only touched tool_calls entries rewritten; untouched entry byte-for-byte.
        {"name": "mixed-tool-calls", "model": "gpt-4",
         "messages": [{"role": "assistant", "tool_calls": [
             {"id": "keep", "function": {"name": "f", "arguments": "{}"}},
             {"id": "strip", "call_id": "rc"}]}]},
        # non-dict tool_call entry passes through.
        {"name": "non-dict-tool-call-entry", "model": "gpt-4",
         "messages": [{"role": "assistant", "tool_calls": [
             "raw", {"id": "x", "response_item_id": "ri"}]}]},
        # empty tool_calls on assistant: key dropped.
        {"name": "empty-tool-calls-assistant", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": None, "tool_calls": []}]},
        # empty tool_calls on non-assistant: left as-is.
        {"name": "empty-tool-calls-user", "model": "gpt-4",
         "messages": [{"role": "user", "tool_calls": []}]},
        # null tool_calls on assistant: key dropped.
        {"name": "null-tool-calls-assistant", "model": "gpt-4",
         "messages": [{"role": "assistant", "content": "a", "tool_calls": None}]},
        # null tool_calls on non-assistant: left as-is.
        {"name": "null-tool-calls-user", "model": "gpt-4",
         "messages": [{"role": "user", "tool_calls": None}]},
        # assistant with empty tool_calls AND sidecars: sidecars popped, key dropped.
        {"name": "empty-tool-calls-with-sidecars", "model": "gpt-4",
         "messages": [{"role": "assistant", "timestamp": 1, "tool_calls": [], "_m": 1}]},
        # tool_calls of a non-list, non-null type: untouched.
        {"name": "tool-calls-non-list", "model": "gpt-4",
         "messages": [{"role": "assistant", "tool_calls": "weird"}]},
        # empty model (mirrors kwargs.get('model') -> None -> ''): non-Gemini,
        # so extra_content stripped.
        {"name": "empty-model-strips-extra-content", "model": "",
         "messages": [{"role": "assistant", "tool_calls": [
             {"id": "c1", "extra_content": {"thought_signature": "s"}}]}]},
        # several messages: order preserved across the batch, mixed shapes.
        {"name": "multi-message-batch", "model": "gpt-4",
         "messages": [
             {"role": "system", "content": "sys"},
             {"role": "user", "content": "hi", "platform_message_id": "p1"},
             {"role": "assistant", "tool_calls": [{"id": "c1", "call_id": "rc"}]},
             {"role": "tool", "content": "res", "tool_name": "search", "tool_call_id": "c1"},
         ]},
    ]


def main() -> None:
    ns = load_functions()
    convert_messages = ns[WANTED_METHOD]
    out = []
    for case in cases():
        expected = convert_messages(None, case["messages"], model=case["model"])
        out.append({
            "name": case["name"],
            "model": case["model"],
            "messages": case["messages"],
            "expected": expected,
        })
    content = json.dumps(out, indent=2) + "\n"
    if sys.argv[1:] == ["--check"]:
        assert OUT.read_text(encoding="utf-8") == content, "Chat message fixtures differ from Python"
    elif not sys.argv[1:]:
        OUT.write_text(content, encoding="utf-8")
    else:
        raise SystemExit("usage: gen_chat_message_goldens.py [--check]")
    print(f"Verified {len(out)} chat message cases")


if __name__ == "__main__":
    main()
