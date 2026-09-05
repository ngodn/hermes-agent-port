# Provider-prefix resolution dependency

Source: providers/__init__.py, providers/base.py, and
agent/model_metadata.py::_strip_provider_prefix. Codex corrected Gemini's
draft after checking the implementation; raw output remains in
audit-provider-prefix-registry.agy.log.

## Reference contract

The prefix resolver checks the live provider registry, including plugin
registrations. It splits once on a colon, skips strings starting with lowercase
http, strips/lowercases the prefix for lookup, and preserves the original
suffix when stripping succeeds. A provider lookup failure leaves the model
unchanged.

The Ollama tag regex prevents stripping recognized prefixes when the suffix
starts with a model tag, including numeric size, latest, stable, quantization,
instruct, chat, coder, vision, or text. It is a prefix match, not a full-string
match. Python Unicode whitespace, digits, and case-insensitive matching need
source-derived tests.

Registration itself does not normalize names or aliases. get_provider_profile
checks the alias map before the canonical map. A later registration replaces
the canonical profile and adds its aliases; it does not remove older aliases.
Do not silently change that behavior during the port.

Discovery order is entry points first, bundled directories, user
model-providers directories, flat installed plugins declaring kind:
model-provider, then legacy provider modules. Directory scans are sorted.
The entry-point step also has enable/disable policy and callback handling;
read that complete code when porting discovery.

## What must not be substituted

The manifest and directory name do not establish a registered provider's
identity. Python imports plugin code, which can register arbitrary names,
aliases, and behavior. Guessing names from directory suffixes or merely reading
plugin.yaml would report providers that may not register and miss others.

A generated snapshot of bundled identities can be reference evidence but is
not full plugin lookup. Likewise a Python reflection bridge could be an
explicit intermediate strangler step, not completion of the native registry.

## Current implementation boundary

Registration, alias replacement, shared identity, declarative fields, and prefix
recognition are now implemented; see [verification](provider-registry-verification.md).
Next implement actual discovery and provider hooks for the supported plugin runtime. Reuse it for model resolution and transport as well as prefix lookup.
The current Rust URL-based credential helper in config_file.rs is not that
registry.

Only then move provider-prefix stripping into the native probe caller.
lookup_ollama_vision currently accepts an already resolved bare model name and
documents this remaining responsibility.

## Corrections to the draft

- Aliases are not lowercased by register_provider.
- Manifest or directory fallback identity is not equivalent to executing a
  provider plugin and must not be presented as faithful discovery.
- The suggested Rust trim does not cover all Python strip characters.
- Lowercasing allocates; the sample was not a zero-allocation implementation.
- The draft's source-line/call-site table was not reliable enough to retain.
