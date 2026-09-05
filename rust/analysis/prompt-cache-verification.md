# Native prompt-cache integration

The native chat client applies the Python prompt-cache helpers after provider
hooks and caller overrides, before flattening SDK extra_body onto the wire.
Both caller key locations are bounded independently. Explicit empty values
suppress automatic replacement. Key removal preserves object insertion order.
The hash uses the original leading system/developer content and the resulting
tool definitions, without rewriting conversation history.

A cloned client carries the persisted platform/channel session identity for the
whole turn, including every tool iteration. No shared client scope is mutated.
Generic routes infer support only for the exact api.openai.com hostname;
profile routes use the profile capability flag exclusively.

Dispatcher leases now use the same session_db::session_id_for identity. A
regression with identical channel IDs on Telegram and Discord timed out before
the fix because the raw channel lock serialized distinct persisted sessions.
It passes after the fix. Existing case folding in persisted IDs is retained.
Full session resolution, compression lineage and cron runner integration remain
outside this change; the pure cron scope helper is ported and tested.

## Evidence

- The generator extracts and executes five Python functions from the reference
  transports. Regeneration with mise Python 3.12.13 verifies all 65 cases.
- A local HTTP server captures ten native requests: successive turns, concurrent
  conversations, changed prefixes, two tool iterations, oversized caller keys
  and explicit empty keys. Scope stability and isolation are asserted on actual
  outgoing JSON. The test failed with an absent automatic key before wiring.
- Endpoint tests cover exact and uppercase OpenAI hosts, trailing-dot and
  deceptive hosts, Azure, scheme-less URLs and both profile capability values.
- Workspace validation: 1,124 passed, one existing bridge test ignored.
  Logs: prompt-cache-tests.log and prompt-cache-clippy.log.
- Tests remain inline in their owning Rust modules.

The helper follows the existing JSON representation boundaries: arbitrary-size
Python integers, lone surrogates and nonfinite Python floats are not represented
faithfully by serde_json values. This is native chat routing, not completion of
the Responses transport or the full agent loop.
