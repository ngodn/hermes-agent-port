# Native main-provider credential-pool recovery resolution

## Outcome

The native chat-completions main provider now selects a profile-scoped static
API-key pool before dotenv or process credentials. A profile pool shadows the
root provider pool, while a profile without that provider borrows the root
pool. Explicit native API-key or base-URL overrides bypass pool attachment, and
empty or fully cooling pools retain the environment fallback.

The selected pool entry can supply its own endpoint. Provider aliases resolve
through the registered profile identity, disabled providers fail before client
construction, and OAuth, non-chat, named custom, and cross-provider fallback
paths remain outside this checkpoint.

## Shared request recovery

Streaming turns and tool-loop rounds now use one request dispatcher. It builds
the request body once, then applies this bounded recovery policy:

1. Authentication, billing, and unverified-billing failures persist the exact
   dispatched key's cooldown before rotating immediately.
2. An ordinary 429 retries the dispatched key once, then persists and rotates.
3. A pre-exhausted key or usage-limit signal rotates on the first 429 without
   the redundant retry.
4. Each new physical credential receives its own two-attempt 429 budget, so the
   existing Python outer loop can traverse the available pool without looping
   forever on one entry.
5. OpenRouter-wrapped upstream throttles, provider overload, entitlement
   failures, transport failures, and unrelated statuses do not mutate the main
   pool.

Each mutation reloads the durable pool. Exact wire-key attribution wins if a
stored ID is stale, duplicate rows sharing the key are quarantined together,
and the replacement is installed only if the conversation cursor still points
at the failed ID and key. Rotation and cursor installation complete inside one
blocking task, so cancellation cannot leave a persisted failure paired with a
stale conversation cursor.

The cursor and fresh `reqwest::Client` are shared across per-turn clones. A
rotated credential therefore survives later tool rounds and later turns in the
same frozen conversation.

## Endpoint and cache safety

Every replacement gets a fresh HTTP client and re-resolves its effective
endpoint. Route-specific headers are rebuilt from provider defaults plus only
the matching endpoint override, so headers from the failed route cannot reach a
different endpoint. Later headers replace earlier values rather than appending
duplicates.

The retry reuses the same serialized request bytes. It does not rebuild the
system prompt, tool schema, messages, or provider request body. Tool rounds
retain the same frozen prompt and schema after recovery. Streaming recovery is
limited to a non-success HTTP status before body consumption, so it cannot
replay after partial output has already reached the user.

## Helper review and corrections

AGY owned the source-executed Python behavior lane. Claude separately mapped
the Rust ownership seam, then reviewed the integrated implementation. They did
not repeat the same assignment.

Primary verification corrected several helper claims against executable
evidence. An explicit base URL disables pool attachment. The raw no-base-URL
golden did not itself prove final endpoint fallback. The auth ordering case had
two persistence callbacks before swap, which made its simplistic
`persisted_first` boolean false without violating durability. Python's helper
accepts numeric Retry-After values, not HTTP dates, and the duration corpus uses
4 hours 5 minutes, or 14,700 seconds.

Claude found two valid defects. A transient usage-limit 429 was receiving an
unnecessary same-key retry, and `HeaderMap::extend` appended duplicate override
headers. Both were fixed and covered. The primary lane also expanded the Python
corpus from 73 to 86 cases with 13 raw HTTP classification boundaries, then
made Rust consume those generated expectations directly.

## Verification

The production integration exercises streaming and tool-loop recovery for 401,
ordinary 429, and usage-limit 429 responses. It proves pool-over-environment
startup selection, exact failed-key request counts, persistence visible before
the replacement request, endpoint and authorization rotation, route-header
isolation, byte-identical retry bodies, and later tool-round cursor reuse.

The generated Python corpus has 86 cases across 11 sections and regenerates
byte for byte. The Rust classifier consumes the 13 raw status, body, header,
and provider cases instead of relying only on hand-written expectations.
The full Rust workspace passes 1,845 tests with two expected ignores. The 256
selected Python classifier, credential-pool, provider-boundary, and runtime
resolution tests pass. Rust and Python formatting, Ruff, workspace Clippy with
warnings denied, and diff hygiene also pass.

## Honest boundary

This checkpoint covers store-backed static API keys on the native main
chat-completions path. Mid-stream failures after a successful response status,
custom TLS re-derivation, OAuth refresh, Anthropic Messages, Codex Responses,
Gemini-native and other non-chat transports, general provider fallback,
dynamic provider plugins, prompt invalidation, and broader client-eviction
policy remain separate work.
