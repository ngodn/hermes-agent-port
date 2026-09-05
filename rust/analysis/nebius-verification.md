# Native Nebius Token Factory

Nebius now registers its complete profile natively, including aliases, ordered
credential names, auxiliary model, fallback models and verbose model-catalog URL.
Its only overridden method projects top-level reasoning_effort fields through
the same request boundary used by streaming and every native tool iteration.

The hook strips vendor prefixes before checking its model-family allowlist.
It preserves explicit false, falsey effort fallback, Python string coercion,
none/off/disabled omission, nearest-weaker clamping and bespoke effort strings.
These rules remain separate from Upstage's defaults and disable behavior.

## Verification

The generator executes the real Python subclass and shared effort clamp. It
compares the complete profile and produces 812 request-hook cases, including
both values of the caller's supports_reasoning flag. It rejects newly added
class methods so additional overrides require a native implementation.

All Rust tests remain inline. The startup integration test adds 12 local HTTP
requests through the nebius alias, covering defaults, ultra/minimal, explicit
false and excluded models in streaming and tool modes. This checks that an
allowlist marker in the vendor prefix alone does not enable reasoning.
No paid provider inference was invoked.

Validation: 1,085 workspace tests passed, one existing Python-bridge test ignored.
Formatting, Clippy with warnings denied and diff whitespace checks pass.
The Python 3.12.13 generator also passes with --check. Logs are nebius-tests.log
and nebius-clippy.log; Gemini's audit is nebius-hook-source-review.md.

## Remaining integration

The native caller supplies supports_reasoning=false, matching the standard
Nebius endpoint and leaving its model allowlist authoritative. Explicitly
rerouting this profile through an aggregator with catalog-advertised reasoning
requires the still-unported core route capability resolver. The hook itself
supports that true flag and is tested against Python for it, but live aggregator
resolution is not claimed here. Dynamic provider discovery, other provider hooks,
full runtime/session reasoning changes and rich-runner wiring remain open.
