# chat_message_projection.rs — port notes

Ports `ChatCompletionsTransport.convert_messages` and
`_model_consumes_thought_signature` from
`agent/transports/chat_completions.py` (both left unchanged in the source).

## Public surface

- `convert(messages: &[serde_json::Value], model: &str) -> Vec<serde_json::Value>`
  is the only public item. It corresponds to the transport call site
  `self.convert_messages(messages, model=model)`: the method takes `**kwargs` but
  only ever reads `kwargs["model"]`, so the port takes `model` directly.
- `model_consumes_thought_signature` and `sanitize_message` are private helpers,
  both reachable from `convert` (no dead code, no `#![allow(dead_code)]`).

## Behaviours preserved (all pinned by goldens and/or inline tests)

- **Sidecar stripping.** The nine persistence-only / transport-internal keys
  (`codex_reasoning_items`, `codex_message_items`, `tool_name`,
  `effect_disposition`, `timestamp`, `platform_message_id`, `api_content`,
  `anthropic_content_blocks`, `bedrock_content_blocks`) are removed from every
  object message. Python guards the block with a big `or` and then pops each with
  a default; popping an absent key is a no-op, so the port removes all nine
  unconditionally — same result, less branching.
- **Underscore scaffolding.** Every top-level key starting with `_` is dropped
  (`retain`, which keeps the surviving keys in order).
- **`extra_content` is model-sensitive.** Stripped from `tool_calls` entries
  unless the target model is Gemini-family (`gemini`/`gemma` substring, on the
  lowercased model). `call_id` and `response_item_id` are always stripped from
  tool-call entries. Test `gemini-strips-call-id-keeps-extra-content` proves the
  combination: on a Gemini target `call_id`/`response_item_id` still go, but
  `extra_content` stays.
- **Empty / null assistant `tool_calls`.** An assistant message with
  `tool_calls: []` or `tool_calls: null` has the key dropped entirely. The same
  shapes on a non-assistant role are left untouched (the `role == "assistant"`
  guard). The empty-array branch `return`s early, mirroring Python's `continue`,
  so the null branch is not also consulted.
- **Nested tool_calls are otherwise untouched.** Only entries that actually carry
  a stripped field are rewritten; a sibling entry (and its nested `function`
  block) passes through byte-for-byte. Non-object tool-call entries pass through.
- **Insertion order.** `serde_json` is built with `preserve_order`, so message
  and tool-call maps are `IndexMap`. All removals use `shift_remove` (not
  `swap_remove`/`remove`, which would reorder), and re-inserting `tool_calls`
  onto an existing key keeps its position. Faithful to CPython dict order.
- **Non-object messages** (`"raw"`, numbers, `null`) pass through in place.

## Notes on the two-pass Python original

The source runs a detection pass (`needs_sanitize`) purely to short-circuit and
return the original list when nothing needs changing. The mutation pass is a
no-op on any message the detection pass would not have flagged, so the port skips
the detection pass and always runs the transform: the output is identical. The
only observable effect of the Python early return is object identity of the
returned list, which is not part of the value contract.

## Generator

`rust/tools/gen_chat_message_goldens.py` extracts the module-level
`_model_consumes_thought_signature` and the `convert_messages` method (from the
`ChatCompletionsTransport` class body) with `ast`, `unparse`s them, and execs
both into one namespace. `convert_messages` never touches `self`, so it is called
as `convert_messages(None, messages, model=...)`. The source has no
`from __future__ import annotations`, so signature annotations evaluate at def
time; `Any`/`Dict`/`List` are placed in the exec namespace to satisfy them. No
agent runtime is imported. Run under the pinned interpreter:

    mise x python@3.12.13 -- python rust/tools/gen_chat_message_goldens.py         # write
    mise x python@3.12.13 -- python rust/tools/gen_chat_message_goldens.py --check # verify

30 cases in `rust/tools/chat-message-goldens.json`; `--check` passes.

## Verification

Cannot `cargo test` in-repo without registering `mod chat_message_projection;`
in `main.rs`, which is out of scope for this task (no `main.rs`/Cargo edits, no
commits). Verified instead in a throwaway crate that `#[path]`-includes the
module: `cargo test` → 9 passed (the 30-case golden test plus 8 targeted tests),
`cargo clippy --all-targets` → clean. The main agent registers the module and
builds the workspace.
