# Extension-host review resolution

Date: 2026-09-08

This records the verified disposition of
`extension-host-implementation-review-agy.md` and
`extension-host-implementation-review-claude.md`. The helper reports are
advisory snapshots from before the fixes below. Source and executable tests,
not the reports alone, determine this checkpoint's claims.

## Fixed before checkpoint

- Well-formed `ok: false` responses no longer poison the worker. Transport,
  framing, EOF, size, ID, and timeout failures remain fatal and terminate the
  process tree. The real host test sends an invalid call and then successfully
  snapshots through the same child.
- Multimodal plugin envelopes are unwrapped before provider projection.
  Text-only parts stay structured; image parts require both model vision and
  provider tool-message support, otherwise they fall back to a text summary.
  Unit and live HTTP tool-loop tests cover the wire content.
- Scoped profile values are re-applied after dotenv loading. Scoped children
  also start from a process-global environment allowlist, which removes foreign
  profile values even when their names do not look like credentials. The real
  subprocess test covers both a credential-suffix value and an arbitrary
  profile-local setting.
- The stable persisted `session_key` is forwarded as `gateway_session_key`,
  with the internal transcript ID used only for legacy rows that have no route
  key.
- Rust-native tool names are reserved in the Python registry before plugin
  discovery. Unapproved collisions are rejected by the existing Python gate;
  explicit `override=True` still requires operator consent.
- Plugin definitions and dispatch now share plugin-first precedence over a
  memory-provider name collision. The subprocess test checks both the chosen
  schema and handler.
- Enabled toolsets come from the real platform resolver after plugin discovery.
  Disabled toolsets come from `agent.disabled_toolsets`, and the same memory
  exposure decision gates both schemas and prompt text.
- Explicitly selected auto-loaded bundled backend toolsets open the host without
  adding a Python process to the default path. A repository-manifest test covers
  the Spotify backend.
- Session cwd is applied before config and project-plugin discovery. The real
  subprocess fixture executes inside that cwd.
- Plugin schemas preserve fields such as `strict`. Missing, null, or malformed
  parameter schemas are normalized to an object schema before model I/O.
- Fresh prompt snapshot failure removes both extension prompt content and
  extension tools before provider I/O, and corrects persisted tool names.
- Python and fd-level plugin stdout are redirected away from the protocol. The
  Rust reader independently skips bounded blank and diagnostic lines before a
  valid response.

## Deliberately deferred

- `ConversationAgent` still has no TTL, LRU, or reset-driven eviction. A cached
  conversation therefore pins its extension child. This is the next lifecycle
  checkpoint, not a completed production-lifetime claim.
- A crashed child is not respawned in the middle of a conversation. Transport
  failure is fail-closed for the remaining cached client lifetime.
- External-memory initialization, prompt text, tools, and shutdown are live.
  Turn-level prefetch, write, sync, and provider event hooks are not yet wired.
- `Message` still lacks `user_id_alt`, `user_name`, and `chat_name`, so those
  optional provider identity values cannot yet be forwarded.
- Unix process groups and normal Windows teardown cover descendants. Abrupt
  Windows owner cancellation still needs a Job Object for equivalent tree
  ownership.
- Route-specific toolset overlays, dynamic plugin registry drift, native
  compression reconstruction, and compression-triggered prompt invalidation
  remain part of the wider port.
