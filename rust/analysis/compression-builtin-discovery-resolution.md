# Native compression built-in discovery resolution

Date: 2026-09-10

## Result

Native full compression in auxiliary auto mode now reaches a third fallback
tier after the task-specific and top-level configured tiers. The tier preserves
Python's chain shape and executes the strict native subset that the current
gateway can represent safely:

1. OpenRouter, when a scoped `OPENROUTER_API_KEY` exists and `free_only` admits
   the selected model.
2. Nous is reserved in this position but omitted until native device-code token
   validation and refresh exist.
3. The active custom endpoint, when it uses the supported chat-completions wire.
4. Registered API-key profiles with a nonempty auxiliary model and the native
   chat-completions wire. The current ordered subset is `stepfun`, `gmi`,
   `ai-gateway`, `kilocode`, `fireworks`, and `novita`.

`openai-codex` remains excluded from automatic discovery. Native Gemini,
Anthropic Messages, Codex Responses, and other non-chat transports are not
silently routed over the wrong protocol.

## Ownership

Each conversation freezes its resolved candidate clients together with its
prompt and configured routes. Those clients contain profile secrets and are
never shared across profiles. The gateway owns one shared health table, and
every key includes both the profile home and normalized provider label. A 402
or equivalent quota failure on the main route, or an unrefreshable 401 on a
fallback candidate, quarantines only that profile's provider for 600 seconds.
Lookup uses monotonic time and lazily removes expired entries.

Python's health dictionaries are process-global without a profile key. The
native implementation intentionally adds the profile dimension because one
gateway can multiplex profiles with different credentials. Sharing a raw
provider-only mark would let one profile suppress another profile's healthy
account.

The codebase-design skill kept this as one deeper route plan: discovery clients
join the existing `summarize_history_on` executor, usage ledger, and prompt
bytes. No second request engine, prompt variant, or model-tool surface was
introduced.

## Execution rules

- Discovery runs only in auto mode and only after no configured candidate was
  selected, or after the selected configured candidate had a stale 401.
- The built-in chain slot matching the failed main route is skipped before I/O.
- Built-in candidates do not receive the 64,000-token configured-chain filter.
- Any non-auth failure or unusable response from the first discovered candidate
  stops immediately.
- An unrefreshable 401 marks the candidate unhealthy and permits exactly one
  more discovered candidate. A third candidate never performs model I/O.
- Every route receives the same already-built summary prompt byte for byte.
- Discovered clients stay tool-free, recursion-free, and account usage to their
  real provider and model as auxiliary compression.

## Production proof

The startup integration uses local main and GMI servers plus a real profile
`.env`. The main summary returns an unusable length result, both configured
tiers are absent, GMI is selected in native profile order, and the integration
asserts its bearer key, model, task request body, missing tools, and exact
message-byte equality with the main request.

A separate stateful HTTP test proves that an 8K discovered candidate is not
pre-filtered, a 401 permits one healthy successor, the unhealthy candidate is
skipped on the next compression, and a 502 prevents the next candidate from
being contacted. Unit tests pin profile isolation, lazy TTL expiry, label
normalization, native subset order, and the checked-in Python corpus.

## Explicit remaining work

This is not full built-in discovery parity yet. The following require the
native credential and transport managers and remain separate checkpoints:

- Nous device-code credentials, rate-guard state, and recommended-model lookup
- credential-pool selection, rotation, persistence, and refreshed-client retry
- 60-second missing-credential health marks with live same-conversation rebuild
- poisoned HTTP client eviction and bounded client caching
- native Gemini, Anthropic Messages, Codex Responses, and provider-specific
  transports and headers outside the registered chat-completions subset
- dynamic provider-plugin catalog discovery

The 97-case source-executed corpus records both the implemented subset and these
remaining behaviors. AGY produced the Python contract and oracle. Claude
independently mapped the Rust ownership seam. The primary lane corrected stale
model, endpoint, provider-count, and corpus-section claims in the helper report
before integration.
