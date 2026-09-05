# Native provider integration review (Claude)

Read-only review of `main.rs::build_agent_client`, `native_agent.rs`,
`provider_registry.rs`, `config_file.rs`, `reasoning_effort.rs` against
`providers/base.py`, the Upstage and Nebius hooks, and the bundled base
profiles. Not a parity claim: the reasoning hooks and profile loader are already
covered by generated goldens (812 Nebius / 138 Upstage / registration cases) and
I did not re-derive those. Findings below are at the wiring level, which the
goldens do not exercise. The main agent's output-cap work is in progress and is
not assessed here.

## Findings

### 1. Streaming decode corrupts multibyte UTF-8 at chunk boundaries (data corruption)
`native_agent.rs:359` decodes each network chunk independently:
`buf.push_str(&String::from_utf8_lossy(&chunk))`. `bytes_stream()` splits the
SSE body at arbitrary byte offsets, so any multibyte code point (emoji, CJK,
accented Latin, smart quotes) straddling two chunks has each half turned into
U+FFFD before it ever reaches the line buffer. The result is silently wrong
streamed text for non-ASCII replies. The fix is to buffer raw bytes and decode
only complete UTF-8 sequences (or decode the accumulated byte buffer, not the
per-chunk slice). Note the non-streaming tool path is unaffected: `step()` uses
`resp.json()` (`native_agent.rs:330`), which buffers the whole body first.
Severity: high, hit on the default plain-chat path for any non-ASCII output.

### 2. Generic key resolver inverts Python's dotenv-over-shell precedence (credential selection)
`config_file.rs:225-242` `resolve_provider_api_key` checks `std::env::var(name)`
first and returns before consulting the dotenv map, so a stale shell export
shadows a rotated key in `~/.hermes/.env`. Python's api-key path does the
opposite: `get_anthropic_key`/the shared resolver call
`get_env_value_prefer_dotenv` and prefer `~/.hermes/.env` precisely "so a
deliberate key rotation isn't shadowed by a stale shell export" (`auth.py:634`,
`auth.py:769`, ref #20591). This is also internally inconsistent:
`resolve_profile_api_key` (`config_file.rs:261-265`) correctly prefers dotenv per
name. This path is live in the native client for `provider = None` configs
(`main.rs:218`, i.e. the OpenRouter default), and the existing test
(`config_file.rs:471`) passes a single map so it never catches the inversion.
Severity: medium, wrong credential selected after a `.env` rotation.

## Boundaries confirmed clean (not regressions, recorded to bound the review)

- Unsupported transports are rejected correctly: `with_provider_profile`
  (`native_agent.rs:159`) errors on any `api_mode != "chat_completions"`, so the
  bundled `openai-codex` and `xai` (`codex_responses`) profiles fall back to the
  subprocess bridge (`main.rs:236`) rather than being spoken to over the wrong
  wire. `auth_type` is enforced for keys in `resolve_profile_api_key`
  (`config_file.rs:254`).
- Request projection for the currently bundled base profiles is complete for the
  fields that matter here: none set `fixed_temperature`, `default_max_tokens`, or
  `supports_prompt_cache_key` (checked against `bundled-base-profiles.json`), so
  the native client dropping those transport hooks causes no active divergence
  today. Provider `default_headers` (fireworks/gmi/xai User-Agent) are applied on
  both the streaming and tool paths (`native_agent.rs:248,318`).
- The reasoning value shape passed into the hooks matches Python: `resolve_config`
  yields `{"enabled":..,"effort":..}` / `{"enabled":false}` / `None`
  (`reasoning_effort.rs:161`) and is threaded through `apply_provider_extras` at
  the wire boundary for both streaming and every tool iteration
  (`native_agent.rs:191-202,243,312`), which is the split Python documents in
  `base.py:161`.

## Open scope worth tracking (documented as remaining, not counted as regressions)

- `build_api_kwargs_extras` in Python returns `(extra_body_additions,
  top_level_kwargs)` (`base.py:161`). The Rust `api_kwargs_extras` only produces
  the top-level map; the separate `build_extra_body` hook and any future
  `extra_body` additions are not projected. Harmless for Upstage/Nebius (both
  always return an empty `extra_body`) and for the base profiles (default empty),
  but any newly ported provider that uses `extra_body` (OpenRouter-style
  `extra_body.reasoning`) will silently lose it under the native client.
- Base-profile / OpenRouter reasoning is core-transport in Python, not a profile
  hook, so a `provider = None` native run against the default OpenRouter endpoint
  sends no reasoning fields even when `agent.reasoning_effort` is configured. The
  verification notes already call this out as unported; flagging only so it is not
  mistaken for parity.
- `xiaomi` sets `supports_vision_tool_messages: false`; the native client has no
  handling for it. Inert until native vision tool-results are wired.
