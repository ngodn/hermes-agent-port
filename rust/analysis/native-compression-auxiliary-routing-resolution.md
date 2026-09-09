# Native compression auxiliary routing resolution

Date: 2026-09-09

## Outcome

Full native compression summaries now use the frozen
`auxiliary.compression` configuration captured when the conversation client is
built. The route is isolated from the main conversation client, and the summary
request remains a single user message with no tool schema.

Startup resolves the configured provider, model, endpoint, direct key or
`key_env`, API mode, named-provider headers and body fields, reasoning setting,
timeout, and provider default auxiliary model. `provider: auto` inherits the
already resolved main route. The `openai` direct alias becomes the official
OpenAI-compatible custom route, while a bare `base_url` without a provider or
key is discarded like Python instead of silently becoming a custom route.

Config-derived compression timeouts have the Python 300 second floor. A hard
output cap is sent only when an exact concrete provider and model match a route
explicitly marked non-reasoning. The normal main-route retry remains uncapped
and does not inherit auxiliary reasoning or `extra_body` fields.

When the auxiliary request errors, is length-truncated, contains a tool call,
or has no usable visible answer, native compression retries once on the main
conversation route. A successful auxiliary response returns immediately and
does not touch the main endpoint. Each attempted route keeps its own usage
capture, recorded under task `compression` when a session database is present.

## Evidence and corrections

Claude produced a 129-case source-executed oracle covering task configuration,
fast-lane cap certification and forwarding, temperature selection, and the
compressor-level fallback predicates. AGY independently mapped the live Python
client selection, timeout, usage, startup, and fallback layers. These were
separate deliverables and neither helper edited Rust production code.

Source verification corrected two tempting but wrong simplifications:

- A bare auxiliary endpoint does not select a custom provider unless a key or
  named provider also selects that route.
- Python's compressor-level one-shot main fallback is armed only by a pinned
  distinct `summary_model`. Configured auxiliary routes normally fail over
  inside `call_llm`. Rust implements the final bounded main-route safety retry
  here, but does not claim the intermediate configured fallback chain yet.

The public startup test uses two real local HTTP endpoints. It proves the
auxiliary key, model, cap, reasoning body, and custom field reach only the
auxiliary endpoint; truncated output causes one clean main-route request; a
later successful auxiliary response causes no main request; and every summary
request is tool-free.

## Deliberate remaining work

- `auxiliary.compression.fallback_chain`, main provider fallback chains,
  discovery fallbacks, capacity screening, credential pool rotation, OAuth
  refresh, and provider health caches are not native yet.
- `codex_responses`, `anthropic_messages`, `bedrock_converse`, Gemini-native,
  and other auxiliary transports stay on the existing fallback boundary. An
  unsupported configured transport logs a warning and keeps main-route native
  compression available.
- Progress-aware stall detection, retry parameter stripping, the full cooldown
  ladder, and per-task concurrency limits remain separate runtime work.
- Mid-turn rotation, exact synthetic and multi-user handoff anchors,
  structural no-op backoff, required memory checkpoints, notifications, and
  overflow recovery remain in the compression cluster.

## Validation

- Rust workspace: 1,656 passed, two ignored.
- Selected Python auxiliary and compression suites: 113 passed.
- Claude source-executed oracle: 129 cases, regeneration check passed.
- Focused live auxiliary routing and compression tests passed.
- Formatting, Clippy with warnings denied, and diff hygiene passed.
