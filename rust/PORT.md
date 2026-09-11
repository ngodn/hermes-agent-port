# Hermes Rust rewrite

## Native main-provider retry notices: 2026-09-11

Ordinary native chat-completions retries now use the complete turn-local
presentation lifecycle. Same-route countdowns and fallback-attempt context are
buffered and dropped after successful recovery. Terminal failure flushes them
once in order, followed by a bounded, secret-redacted terminal status. Durable
fallback switches remain visible on recovery without duplicating their terminal
copies.

The existing Z.AI Coding overload schedule now exposes short waits through the
same buffer and sends adaptive long-backoff notices live before sleeping. Notice
state and the live sender follow frozen fallback route clones without entering
SQLite, model history, system prompts, tool schemas, or provider requests.

AGY ran four live `AIAgent.run_conversation()` cases for HTTP 500 recovery and
terminal exhaustion, ordinary HTTP 503 fallback exhaustion, and deterministic
format rejection. The oracle corrected an earlier inferred detail: Python's
format-rejection fallback banner says `provider failure` because the branch does
not forward its classifier reason. Claude separately mapped interrupted-wait
and stale accounting for the next checkpoint. See
[native-main-provider-retry-notices-resolution.md](analysis/native-main-provider-retry-notices-resolution.md).

The refreshed weighted full-port estimate is **59.55 points, reported as about
60%** (judgment range 56% to 62%). Native agent core moves from 85% to 86%; the
other area estimates are unchanged. Provider-response and local-load wait
notices, cooperative interrupted-wait accounting, non-chat and remaining OAuth
routes, dynamic providers, native plugin and external-memory managers, prompt
invalidation, and broader client eviction remain. The full workspace passes
**1,965 Rust tests with two ignored**. The live Python oracle and its 17 focused
tests pass, and Rust formatting, workspace Clippy with warnings denied, and diff
hygiene pass.

## Native main-provider fallback notices: 2026-09-11

Native chat-completions turns now emit Python-compatible presentation notices
for every provider fallback switch and for later primary-route restoration.
Recovered and exhausted fallback chains surface each switch exactly once and in
order. A compression preflight that restores the primary transfers the pending
notice into the admitted turn instead of losing it at turn-local reset.

Push dispatch sends every nonempty `GatewayNotice` through the selected
platform adapter before releasing the buffered assistant answer. Notice-only
turns are not suppressed. The synchronous HTTP `/message` schema remains
reply-only, and both sinks keep notice bytes out of SQLite, model history,
system prompts, tool schemas, and provider requests. Existing memory-recall
notices now use the same push delivery seam.

AGY owned a 37-case source-executed Python contract for fallback reason text,
ordered recovery, terminal duplicate suppression, callback failures, and
idempotent cleanup. Claude independently mapped the push and HTTP sinks, then
reviewed the finished state ownership and delivery design. See
[native-main-provider-fallback-notices-resolution.md](analysis/native-main-provider-fallback-notices-resolution.md).

The refreshed weighted full-port estimate is **59.35 points, reported as about
59%** (judgment range 55% to 61%). Native agent core moves from 84% to 85%; the
other area estimates are unchanged. Full transient retry and wait traces,
interrupted-wait accounting, non-chat and remaining OAuth routes, dynamic
providers, native plugin and external-memory managers, prompt invalidation,
and broader client eviction remain. The full workspace passes **1,956 Rust
tests with two ignored**. The 37-case corpus regenerates byte for byte, all 46
focused Python tests pass, and Rust/Python formatting, Ruff, workspace Clippy
with warnings denied, and diff hygiene pass.

## Native run-budget stale scaling: 2026-09-11

Native tool-enabled chat-completions turns now resolve the existing
`agent.run_budget_seconds` setting into Python-compatible buffered stale
deadlines. Each admitted turn stamps one wall-clock start before restore,
memory, or provider work. Primary and frozen fallback routes share that start,
and each buffered request caps implicit default, reasoning-floor, and
context-scaled patience at half the remaining budget with a 60-second floor.

Explicit model, provider, and legacy environment stale settings remain
authoritative. Plain local implicit routes remain unbounded, while local
reasoning floors stay finite and budget-cappable. Request deadlines, streaming
deadlines, retries, backoff, provider routing, prompt bytes, tool schemas, and
transcript state are unchanged. The exact request-time buffered deadline is
carried through body decoding so later wall-clock movement cannot alter timeout
attribution.

AGY owned the independent Python oracle lane. Its wrapper exited before its
background task completed, and primary review replaced the blocked repeated
full-agent construction with a sub-second source-executed harness. The final
32-case corpus runs real Python normalization, buffered-timeout, and streaming
derivation functions. Claude separately mapped operator notices as the next
independent seam. See
[native-main-provider-run-budget-resolution.md](analysis/native-main-provider-run-budget-resolution.md).

The refreshed weighted full-port estimate is **59.15 points, reported as about
59%** (judgment range 55% to 61%). Native agent core moves from 83% to 84%; the
other area estimates are unchanged. Operator notices, interrupted-wait
accounting, non-chat and remaining OAuth routes, dynamic providers, native
plugin and external-memory managers, prompt invalidation, and broader client
eviction remain. The full workspace passes **1,945 Rust tests with two
ignored**. The 32-case corpus regenerates byte for byte, all 28 focused Python
tests pass, and Rust/Python formatting, Ruff, workspace Clippy with warnings
denied, and diff hygiene pass.

## Native dropped-stream recovery: 2026-09-11

Native no-tools chat-completions streams now recover visible output after clean
EOF, protocol `[DONE]`, or a body transport error when no trustworthy terminal
evidence arrived. Provider finish reasons, zero-count usage objects, and
top-level or nested Nous `lastOne` frames prove completion. Pre-generation
transport failures still use the ordinary replay-safe retry and fallback path.

Recoverable fragments use Python's exact network continuation prompt on the
frozen serving route. The existing progressive output cap and four-attempt
ceiling remain authoritative. Visible fragments are delivered once, then the
assistant-fragment and user-nudge pair commits through the immediate SQLite
continuation transaction before the final suffix. Reasoning-only failures
suppress an invalid empty assistant row and preserve the established reasoning
configuration.

AGY produced a 47-case, 11-section Python contract. Primary review found copied
conversation-loop simulations in its first version, so AGY replaced them with
real `AIAgent.run_conversation()` execution and asserted every declared golden
field. Claude separately mapped the next run-budget scaling seam while the
oracle and implementation proceeded. See
[native-dropped-stream-recovery-resolution.md](analysis/native-dropped-stream-recovery-resolution.md).

The refreshed weighted full-port estimate is **58.95 points, reported as about
59%** (judgment range 55% to 61%). Native agent core moves from 82% to 83%; the
other area estimates are unchanged. Run-budget scaling, operator notices,
non-chat and remaining OAuth routes, dynamic providers, native plugin and
external-memory managers, prompt invalidation, and broader client eviction
remain. The full workspace passes **1,937 Rust tests with two ignored**. The
47-case corpus regenerates byte for byte, all 25 focused Python tests pass, and
Ruff passes for the generator. Rust and Python formatting, workspace Clippy
with warnings denied, and diff hygiene pass.

## Native local Ollama GLM stop correction: 2026-09-11

Local Ollama GLM routes now apply Python's conservative correction when a
post-tool response ends mid-sentence but reports `finish_reason="stop"`. The
pure classifier preserves the exact ordered backend, history, content,
minimum-length, whitespace, and natural-ending gates. Hosted Ollama, `:cloud`
models, unrelated local servers, active tool calls, short content, and natural
terminal boundaries remain ordinary completed responses.

Buffered tool-enabled turns and no-tools streams restored from durable tool
history both use the actual serving route identity. A corrected response enters
the existing bounded length-continuation path, so it inherits the exact prompt,
progressive output cap, suffix-only delivery, atomic fragment/nudge persistence,
and final durable suffix. Prompt bytes, tool schema, credentials, and route
selection remain frozen.

AGY produced a 113-case source-executed Python contract. Primary review repaired
its initially unenforced declared expectations and preserved raw fixture types;
the corrected corpus contains 43 positive and 70 negative cases. Claude mapped
the separate future dropped-stream seam, then reviewed this implementation. No
correctness finding survived its route, cache, transcript, or parity audit. See
[native-ollama-glm-truncation-resolution.md](analysis/native-ollama-glm-truncation-resolution.md).

The refreshed weighted full-port estimate is **58.75 points, reported as about
59%** (judgment range 55% to 61%). Native agent core moves from 81% to 82%; the
other area estimates are unchanged. Dropped-stream recovery, run-budget scaling,
operator notices, non-chat and remaining OAuth routes, dynamic providers,
native plugin and external-memory managers, prompt invalidation, and broader
client eviction remain. The full workspace passes **1,928 Rust tests with two
ignored**. The 113-case corpus regenerates byte for byte, all three focused
Python tests pass, and Ruff passes for the generator. Rust formatting,
workspace Clippy with warnings denied, and diff hygiene pass.

## Native truncation content guards: 2026-09-11

Provider-reported `finish_reason="length"` now passes through one shared
content-guard order in no-tools streaming and buffered tool-enabled turns.
Tagged inline reasoning with no visible answer stops immediately. Visible text
dominated by exact repetition is rejected before continuation. Structured tool
calls still bypass both checks and retain their separate safe retry lane.

Empty reasoning-only truncations no longer append invalid assistant rows. The
next request alone disables reasoning, uses the existing progressive output-cap
schedule, and then restores the user's original cache key. A
reasoning-mandatory 400 gets one immediate retry with that original config and
prevents future disables on the affected route. Four empty attempts return an
actionable delivery-only result, while mixed visible and empty attempts preserve
the accumulated model-authored partial.

The exact continuation nudge is durable without changing clean display text.
One immediate SQLite transaction either merges it into the current user's
model-facing `api_content` sidecar or inserts it after a completed tool group.
The transaction checks the live lineage lease and transcript phase, and
structured multimodal content remains an array at the provider boundary.

AGY produced a 41-case source-executed Python contract and passed 20 focused
Python tests. Claude independently mapped the separate future Ollama/GLM seam,
then reviewed this implementation. Its mixed-ceiling data-loss and dropped-
stream scope findings were reproduced and fixed. See
[native-main-provider-truncation-guards-resolution.md](analysis/native-main-provider-truncation-guards-resolution.md).

The refreshed weighted full-port estimate is **58.55 points, reported as about
59%** (judgment range 55% to 61%). Native agent core moves from 80% to 81%; the
other area estimates are unchanged. Local Ollama/GLM stop correction,
dropped-stream stub recovery, run-budget scaling, operator notices, non-chat and
remaining OAuth routes, dynamic providers, native plugin and external-memory
managers, prompt invalidation, and broader client eviction remain. The full
workspace passes **1,924 Rust tests with two ignored**. The 41-case corpus
regenerates byte for byte, all 20 focused Python tests pass, and Ruff passes for
the generator. Rust formatting, workspace Clippy with warnings denied, and diff
hygiene pass.

## Native main-provider liveness: 2026-09-11

Ordinary native chat-completions routes now freeze Python-compatible request
and stale timeout policy from the conversation config. No-tools calls bound the
wait for response headers, buffered tool calls bound the complete JSON read,
and streaming bodies use one rearmed meaningful-event inactivity deadline.
SSE comments and partial wire bytes cannot keep a silent generation alive.

Pre-visible stalls replay inside the exact two-layer budget before fallback.
Post-visible stalls never replay the original request; they enter the existing
durable length-continuation path and deliver only the missing suffix. Streaming
and buffered stale expiries share a route-local cross-turn circuit breaker that
stops new network work at its ceiling and resets on observable success,
fallback, or primary restoration. Timeout failures never penalize credential
pools or mutate the frozen prompt and tool schema.

AGY independently produced a 168-case, 13-section source-executed Python
contract and passed 68 focused Python tests. Claude separately mapped the next
recovery seam while implementation proceeded, then reviewed this checkpoint.
See
[native-main-provider-liveness-resolution.md](analysis/native-main-provider-liveness-resolution.md).

The refreshed weighted full-port estimate is **58.35 points, reported as about
58%** (judgment range 55% to 61%). Native agent core moves from 79% to 80%; the
other area estimates are unchanged. Thinking-only and provider-specific
continuation, dropped-stream recovery, repetition rejection, run-budget-aware
stale scaling, operator notices, non-chat and OAuth routes, dynamic providers,
native plugin and external-memory managers, prompt invalidation, and broader
client eviction remain. The full workspace passes **1,907 Rust tests with two
ignored**. The 168-case Python corpus regenerates byte for byte, all 68 focused
Python tests pass, and Ruff passes for the generator. Rust formatting, workspace
Clippy with warnings denied, and diff hygiene are checked after the focused
implementation review.

## Native buffered continuation and tool truncation: 2026-09-11

Tool-enabled native chat-completions turns now continue visible
`finish_reason="length"` responses on their frozen route with Python's exact
semantic nudge and progressive output caps. The no-tools stream and buffered
tool path both commit exact assistant-fragment/user-nudge pairs atomically, then
store only the final provider suffix while delivering the complete stitched
answer. Later turns therefore replay the same alternating semantics the provider
saw, without duplicating the visible prefix.

Length-terminated tool calls take a separate safe lane. Broken arguments never
execute or enter SQLite. The unchanged request retries four times, for five
total attempts, with bounded cap growth. Rejected attempts do not enter native
usage accounting. Ceiling exit delivers a failed diagnostic, and a prior
completed tool result receives a durable synthetic assistant closer so the next
turn cannot form an invalid role boundary.

AGY produced and regenerated an 81-case, 11-section source-executed Python
contract; 14 focused Python tests passed. Claude separately mapped timeout and
stall ownership for the next checkpoint, then reviewed this implementation.
See
[native-main-provider-buffered-continuation-resolution.md](analysis/native-main-provider-buffered-continuation-resolution.md).

The refreshed weighted full-port estimate is **58.15 points, reported as about
58%** (judgment range 55% to 61%). Native agent core moves from 78% to 79%; the
other area estimates are unchanged. Thinking-only and provider-specific
continuation, dropped-stream recovery, inactivity deadlines, non-chat and OAuth
routes, dynamic providers, native plugin and external-memory managers, prompt
invalidation, and broader client eviction remain. Validation is **1,895 Rust
tests passed, two ignored**, plus the 81-case corpus and 14 focused Python tests.
Rust and Python formatting, Ruff, workspace Clippy with warnings denied, and
diff hygiene pass.

## Native visible-text length continuation subset: 2026-09-10

The no-tools native chat-completions stream now continues successful responses
that end with `finish_reason="length"`. Each follow-up stays on the frozen route,
replays the original turn plus the received assistant fragment and Python's
exact continuation prompt, raises the output cap progressively, and streams only
the missing suffix. Fragment joins use Python's whitespace rule, and the fourth
truncation surfaces the accumulated partial answer without claiming success.

AGY produced a 104-case, ten-section source-executed contract and passed 53
focused Python tests. Claude independently audited the separate response-stall
and client-rebuild seam. Its finding is that Python's httpx retirement machinery
must not be copied to reqwest; the missing behavior is an inactivity deadline,
which remains a separate configuration-backed checkpoint. See
[native-main-provider-length-continuation-resolution.md](analysis/native-main-provider-length-continuation-resolution.md).

The weighted full-port estimate remains **57.95 points, reported as about 58%**.
This slice closes explicit visible-text streaming continuation, but buffered
tool-path continuation, truncated tool calls, thinking-only length handling,
repetition rejection, dropped-stream stubs, Ollama GLM stop correction, and the
stall deadline remain. Validation is **1,888 Rust tests passed, two ignored**. The
104-case Python corpus regenerates byte for byte, all 53 focused Python tests
pass, and Rust/Python formatting, Ruff, workspace Clippy with warnings denied,
and diff hygiene pass.

## Native main-provider successful-body subset: 2026-09-10

The ordinary native chat-completions path now validates successful HTTP bodies
after the shared pre-body dispatcher. Malformed buffered responses, empty
finished responses, zero-chunk streams, content-policy refusals, reasoning-only
output, valid tool-call payloads with empty content, and post-tool empty nudges
all follow bounded Python-compatible recovery paths across streaming and tool
rounds.

Retries reuse the same frozen request and fallback advances only the existing
sticky route cursor. Rejected bodies never penalize API-key pools, arm provider
cooldowns, or count rejected usage. Once visible streaming output crosses the
boundary, replay remains forbidden. Terminal refusal, `"(empty)"`, and
reasoning-excerpt diagnostics reach the user but never enter SQLite replay,
micro-compaction, or external-memory completion, including after same-turn
session rotation.

AGY produced the 114-case, eight-section source-executed Python corpus and
separately proved that cost-aware retry reduction must fail open until the
native pricing engine exists. Claude independently mapped the ownership seam
and reviewed the initial implementation. The primary lane fixed its verified
reasoning, persistence, nudge-scoping, and generation-detection findings. See
[native-main-provider-success-body-resolution.md](analysis/native-main-provider-success-body-resolution.md).

The refreshed weighted audit is **57.95 points, reported as about 58%**
(judgment range 55% to 61%). Native agent core moves from 76% to 78%; gateway,
tool/RPC, and state/search estimates are unchanged. Length continuation,
response-stall client rebuilding, cost pricing, operator notices, remaining
provider quirks, non-chat and OAuth routes, dynamic providers, native plugin
and external-memory managers, prompt invalidation, and broader client eviction
remain. Validation is **1,885 Rust tests passed, two ignored**. The Python corpus
regenerates byte for byte and the focused Python compatibility suite passes 161
tests. Rust and Python formatting, Ruff, workspace Clippy with warnings denied,
and diff hygiene pass.

## Native main-provider pre-body retry subset: 2026-09-10

The ordinary native chat-completions path now applies Python-compatible
`agent.api_max_retries` policy to replay-safe failures before advancing its
frozen provider chain. Connection errors, transient TLS failures, HTTP 408,
overload 429/503/529, deterministic 500/502 request-validation failures, and
generic 5xx responses use their class-specific thresholds. Each fallback route
starts a fresh bounded budget, while static credential pools remain responsible
only for auth, billing, and rate health.

Streaming and tool rounds still share one request dispatcher and one sticky
route cursor. Same-route attempts reuse the exact request value. A truncated
local HTTP stream proves that once a visible delta crosses the successful-status
boundary, the turn fails without replay or fallback. Deterministic certificate
verification failures also fail immediately instead of wasting the retry budget.

AGY owned the source-executed Python behavior lane and produced 157 cases across
11 contract sections. Claude separately mapped the ownership seam and reviewed
the implementation. Its review found a pooled HTTP 408 panic and a 503/529
empty-response threshold mismatch. Both were reproduced before repair. Primary
review also corrected active-route Z.AI backoff detection and separated
certificate failures from transient TLS errors. See
[native-main-provider-retry-resolution.md](analysis/native-main-provider-retry-resolution.md).

The refreshed weighted audit is **57.55 points, reported as about 58%**
(judgment range 55% to 61%). Native agent core moves from 75% to 76%; gateway,
tool/RPC, and state/search estimates are unchanged. Successful-body validation,
safety fallback, response stalls, primary-client rebuild, operator notices,
non-chat and OAuth routes, dynamic providers, native plugin and external-memory
managers, prompt invalidation, and broader client eviction remain. Validation is
**1,867 Rust tests passed, two ignored**. The 157-case Python corpus regenerates
byte for byte, and the focused Python compatibility suite passes 236 tests.
Rust and Python formatting, Ruff, workspace Clippy with warnings denied, and
diff hygiene pass.

## Native ordinary main-provider fallback subset: 2026-09-10

The native ordinary main chat-completions path now freezes and executes
`fallback_providers` followed by legacy `fallback_model`. Each route owns its
provider, model, endpoint, static credentials or credential pool, headers,
request shaping, context window, and prompt identity. The active provider's
credential pool always recovers first. Only a terminal auth, billing,
rate-limit, or upstream-rate-limit response advances to the next provider.

Streaming and tool rounds share one bounded dispatcher and one
conversation-scoped route cursor. Fallback is sticky through later tool rounds
and cooldown-gated turns. The primary system prompt remains byte-identical,
while every fallback receives a frozen copy that changes only its final model
and provider identity lines. Usage follows the serving route. Recovery stops at
the successful response-status boundary, so partial streams and tool side
effects are never replayed.

Claude's separate review found two lifecycle gaps. Both were reproduced before
repair. Turn-start restoration now reloads the primary pool's durable reset
deadline without blocking the async executor, and a fully exhausted non-rate
chain applies Python's actual five-second replay floor. AGY's independent
source-executed oracle was corrected from a stale 10-second claim and expanded
to 104 cases. See
[native-main-provider-fallback-resolution.md](analysis/native-main-provider-fallback-resolution.md).

The refreshed weighted audit is **57.35 points, reported as about 57%**
(judgment range 55% to 61%). Native agent core moves from 73% to 75%; the other
area estimates are unchanged. Transport retry triggers, malformed or safety
responses, operator notices, non-chat and OAuth routes, dynamic providers,
native plugin and external-memory managers, prompt invalidation, and broader
client eviction remain. Validation is **1,854 Rust tests passed, two ignored**,
plus **150 selected Python fallback tests passed**. The 104-case Python corpus
regenerates byte for byte. Rust and Python formatting, Ruff, workspace Clippy
with warnings denied, and diff hygiene pass.

## Native main-provider API-key recovery: 2026-09-10

The native main chat-completions route now selects profile-scoped static
API-key pools before environment credentials. Profile pools shadow root pools,
missing profile providers borrow root state, explicit native key or endpoint
overrides bypass the pool, and per-entry endpoints take effect without changing
the frozen conversation prompt.

Streaming and tool-loop requests share one bounded recovery dispatcher. Exact
failed-key attribution and request-local durable reloads protect concurrent
conversations. Auth and billing failures rotate immediately, ordinary 429
responses retry the same physical key once, and usage-limit or pre-exhausted
429 responses rotate on the first failure. Persistence completes before retry,
each replacement receives a fresh client, and the shared cursor survives later
tool rounds and turns.

Route headers are recomputed for each endpoint and later values replace earlier
defaults, preventing stale or duplicate credentials from crossing routes. The
retry reuses the same request bytes, prompt, messages, and tool schema. Recovery
stops at the response-status boundary so a partially emitted stream is never
replayed. See
[native-main-provider-pool-resolution.md](analysis/native-main-provider-pool-resolution.md).

AGY owned the Python behavior and source-executed corpus lane. Claude separately
owned the Rust seam analysis and post-implementation review. The primary lane
corrected the helper evidence, fixed the review's usage-limit retry and header
merge findings, and expanded the corpus to 86 cases across 11 sections with raw
HTTP classifier parity.

The refreshed weighted audit is **56.95 points, reported as about 57%**
(judgment range 55% to 60%). This moves state/search from 75% to 76% and native
agent core from 71% to 73%; gateway and tool/RPC estimates are unchanged.
OAuth, non-chat transports, general provider fallback, dynamic plugins,
external-memory managers, prompt invalidation, and broader client eviction
remain. Validation is **1,845 Rust tests passed, two ignored**, plus **256
selected Python tests passed**. The 86-case Python corpus regenerates byte for
byte. Rust and Python formatting, Ruff, workspace Clippy with warnings denied,
and diff hygiene pass.

## Native Nous OAuth compression recovery: 2026-09-10

Native auxiliary auto-discovery now includes the canonical Nous device-code
grant in its reserved position after OpenRouter and before static providers.
Resolution is lazy and profile-aware. It validates inference-scoped JWTs,
preserves root ownership for borrowed credentials, and seeds the owned
device-code pool row without putting secrets in shared process state.

Nous single-use refresh now runs inside a profile-to-root-to-shared lock
transaction. A waiter re-reads and adopts a peer's rotated pair before posting,
and a successful response is durably written before validation or inference
retry. Terminal reuse signals quarantine the singleton and shared mirror. The
retry uses a fresh HTTP client while reusing the exact tool-free summary body.

AGY owned the source-executed Python parity lane, which now has 87 cases across
12 sections. Claude independently reviewed concurrency and security after the
implementation. The primary lane corrected the helper reports, fixed every
verified review finding, and validated the production integration. See
[native-nous-oauth-recovery-resolution.md](analysis/native-nous-oauth-recovery-resolution.md).

The refreshed weighted audit is **56.40 points, reported as about 56%**
(judgment range 54% to 59%). This moves state/search from 74% to 75% and native
agent core from 70% to 71%; gateway and tool/RPC estimates are unchanged.
Pool-only Nous rows, interactive login, dynamic recommended-model selection,
rate guards, main-provider recovery, other OAuth providers, and non-chat
transports remain. Validation is **1,841 Rust tests passed, two ignored**.
The 87-case Python corpus regenerates byte for byte. Rust and Python formatting,
Ruff, workspace Clippy with warnings denied, and diff hygiene pass.

## Native compression API-key recovery: 2026-09-10

Native auxiliary auto-discovery now chooses profile-scoped store-backed API
keys before environment credentials and performs bounded request-time recovery
for the chat-completions provider subset. Exact failed-key attribution protects
healthy siblings from stale IDs, duplicate rows sharing one key are quarantined
together, and 401, payment, and rate-limit cooldowns are durable before any
replacement request begins.

Every mutation reloads a request-local pool from `auth.json`. Writes serialize
across threads and processes, merge concurrent additions plus newer live
cooldowns, preserve re-authenticated tokens, keep Unix mode `0600`, and update
symlink targets without replacing the link. A replacement key gets a fresh
`reqwest::Client`, so stale connection and authorization state cannot leak into
the retry. The frozen summary prompt, message body, tool-free request surface,
provider headers, and conversation prompt remain unchanged.

The production retry tree follows the current Python source: 401 and payment
failures rotate immediately, while an ordinary 429 first retries the dispatched
key once. One rotated-key request is allowed, and a second credential failure
is persisted without another request. A real local HTTP plus filesystem test
proves persistence-before-retry, byte-identical JSON bodies, fresh
authorization, later selection of the healthy key, and pool precedence over an
environment key.

AGY owned the 64-case source-executed pool and recovery corpus. Claude owned a
separate Rust ownership map and a post-implementation review. The primary lane
checked both against the live source, corrected helper summaries about default
health TTL and the ordinary-429 request count, and integrated the production
path. See
[native-compression-credential-recovery-resolution.md](analysis/native-compression-credential-recovery-resolution.md).

The refreshed weighted audit is **56.05 points, reported as about 56%**
(judgment range 54% to 59%). This moves native agent core from 68% to 70% and
state/search from 73% to 74%; gateway and tool/RPC estimates are unchanged.
OAuth and single-use credential refresh, Nous discovery, non-chat transports,
main-provider failover, dynamic provider plugins, and the general auxiliary
client cache remain. Validation is **1,827 Rust tests passed, two ignored**.
The 64-case Python corpus regenerates byte for byte. Rust and Python formatting,
Ruff, workspace Clippy with warnings denied, and diff hygiene pass.

## Native compression built-in discovery subset: 2026-09-10

Native full compression in auxiliary auto mode now continues from the frozen
task-specific and top-level configured fallback tiers into built-in provider
discovery. The production subset preserves Python's ordering for OpenRouter,
the active custom endpoint, and registered API-key providers whose auxiliary
models use the native chat-completions transport. Nous keeps its reserved
position but is not exposed until native device-code refresh exists. Codex
Responses, Anthropic Messages, Gemini native, and other unsupported wires are
also excluded instead of being sent through an incompatible client.

Each conversation freezes profile-scoped candidate clients while the gateway
shares a profile-qualified provider-health table. Main-route quota failures and
unrefreshable candidate 401s quarantine only the affected profile for 600
seconds. Discovered providers deliberately skip the configured-chain 64K
context floor. A non-auth failure stops traversal, while a 401 permits at most
one more discovered request. All attempts reuse the same summary messages byte
for byte and remain tool-free, recursion-free, and separately attributed. See
[compression-builtin-discovery-resolution.md](analysis/compression-builtin-discovery-resolution.md).

AGY owned the 97-case source-executed Python contract and oracle in one
serialized lane. Claude independently mapped the Rust ownership seam. The
primary lane verified both reports against the source, corrected stale model,
endpoint, catalog-count, and corpus claims, rejected a live resolver interface
that could not safely recover profile secrets, and integrated the narrow native
subset. The codebase-design skill kept discovery inside the existing frozen
compression route executor instead of creating a second request engine.

The weighted audit is now **55.50 points, reported as about 56%** (judgment
range 53% to 58%). Credential-pool rotation, OAuth and Nous refresh, poisoned
client eviction, bounded cross-conversation client caching, non-chat provider
transports, and dynamic provider-plugin discovery remain. Validation is
**1,820 Rust tests passed, two ignored**. The 97-case Python corpus regenerates
byte for byte. Rust and Python formatting, Ruff, workspace Clippy with warnings
denied, and diff hygiene pass.

## Native compression top-level fallback chain: 2026-09-10

Native full-compression summaries in auxiliary auto mode now freeze and honor
the main agent's top-level `fallback_providers` and legacy `fallback_model`
policy after the task-specific chain and before future built-in discovery.
Modern and legacy containers merge in Python order, deduplicate by normalized
route identity, skip the failed and configured main providers by raw label,
enforce the 64,000-token context floor, and execute at most one configured
candidate across both fallback tiers.

Top-level routes reuse the existing profile-aware compression client builder.
Their credentials and transport are entry-specific, while timeout, reasoning,
and output-cap controls deliberately remain task-scoped to match the current
Python request path. Route clients stay tool-free and recursion-free, and all
tiers reuse one summary prompt byte for byte.

The live HTTP integration also found and fixed a route-state bug: auto mode was
previously inferred from whether a dedicated client was needed. Auto routes
with task request settings do need a frozen client, but remain auto-routed. The
route plan now separates those concepts and uses that dedicated client in the
main-first slot, pinned to the active conversation model, before falling
through to the two configured tiers. See
[compression-main-fallback-chain-resolution.md](analysis/compression-main-fallback-chain-resolution.md).

AGY owned the Python contract and source-executed corpus. Claude owned a
separate Rust seam review. The primary lane corrected scalar and base-URL
oracle gaps, extended the corpus to 76 cases, rejected the conflated auto-mode
predicate after the live integration exposed it, and integrated the production
path. The codebase-design skill kept the change inside the existing frozen
route plan without adding a second resolver or prompt surface.

Built-in auxiliary discovery, provider health state, credential refresh and
rotation, non-chat transports, and stall-triggered route pinning remain. The
weighted audit is now **55.30 points, reported as about 55%** (judgment range
53% to 57%). Validation is **1,815 Rust tests passed, two ignored**, plus **20
selected Python fallback tests passed**. The 76-case corpus regenerates byte
for byte. Rust and Python formatting, Ruff, Clippy with warnings denied, and
diff hygiene pass.

## Native auxiliary compression fallback chain: 2026-09-10

Native full-compression summaries now resolve and freeze the ordered
`auxiliary.compression.fallback_chain` at conversation startup. Explicit mode
tries its auxiliary route, one eligible configured candidate, then the main
conversation model. Auto mode tries main first, then one eligible candidate.
The one-candidate bound matches the current Python call path and prevents a
long chain from accumulating every per-entry timeout.

Entry parsing, transport aliases, direct and env-backed credentials,
independent unfloored timeouts, and exact-route fast-lane controls follow the
source-executed Python corpus. Runtime selection skips the failed credential
surface after 401 or 402, skips only the exact deployment for model-scoped
failures, preserves sibling models and distinct endpoints, rejects known
context windows below 64,000 tokens, and allows unknown windows. A primary
route that cannot be built seeds the same credential-scoped selection.

Fallback reasoning controls require independent provider and model
certification. Task-level vendor request extensions retain Python behavior,
while primary fast-lane reasoning is stripped from uncertified candidates.
Each route remains tool-free, recursion-free, independently accounted in the
auxiliary usage ledger, and redacted in logs. The compression prompt is built
once and reused byte for byte, so the frozen system prompt and provider tool
prefix remain unchanged. See
[compression-fallback-chain-resolution.md](analysis/compression-fallback-chain-resolution.md).

AGY owned the Python contract and deterministic corpus in one serialized lane.
Claude owned the isolated Rust configuration model in a different lane. A
later serialized AGY run adversarially reviewed production integration. The
primary lane fixed its verified reasoning, boundedness, failure-scope,
context-window, and redaction findings, and rejected two extra-body findings
that contradicted the live Python request path. The codebase-design skill kept
the result as a frozen route plan with no new prompt or core-tool surface.

Validation is **1,810 Rust tests passed, two ignored**, plus **20 selected
Python fallback tests passed**. The 112-case fallback corpus regenerates byte
for byte. Rust formatting, Ruff, Clippy with warnings denied, and diff hygiene
pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **55.10
points, reported as about 55%** (judgment range 53% to 57%). Top-level and
built-in auxiliary discovery, credential rotation and OAuth refresh, non-chat
auxiliary transports, and stall-fence route pinning remain.

## Native interactive terminal approval: 2026-09-09

Manual approval is live for native Unix-local terminal conversations on
Telegram, Discord, and Slack when Tirith is explicitly disabled. One bounded
gateway-owned broker routes immutable approval requests and decisions by the
stable conversation route. Replies resolve before session admission and before
the transcript lease, so a waiting tool turn resumes without deadlock and
neither the prompt nor the control reply enters model history.

Every request is bound to the current turn's sender plus the gateway's normal
`/approve` authorization. Malformed and unauthorized replies do not consume the
request. Once, session, permanent, deny, timeout, cancellation, overload, and
batch decisions are implemented. Session grants survive frozen-client eviction
and clear on conversation boundaries. Permanent grants merge through the
shared lossless config writer. The hardline, sudo-stdin, and user-deny floors
still run before every approval mode.

The native dangerous-command classifier is pinned to a source-executed
251-case Python corpus. A separate 76-case corpus covers prompts, reply parsing,
authorization, scopes, and lifecycle outcomes. A dispatcher integration proves
approval replies bypass the held transcript lease and stay out of SQLite
history. A real provider and SQLite test proves one approval prompt, session
reuse, durable cwd, audit-note replay, and byte-stable schemas across five
requests. See
[native-interactive-approval-resolution.md](analysis/native-interactive-approval-resolution.md).

AGY owned the two independent Python contract oracles in serialized runs.
Claude owned an isolated broker draft and a later security and concurrency
review. The primary lane verified and integrated them, corrected contract and
review errors, removed speculative APIs, and fixed route identity, dropped
waiter, sender identity, and session-lifetime findings. The codebase-design
skill kept broker lifecycle, terminal policy, and stream presentation behind
separate narrow interfaces.

Validation is **1,795 Rust tests passed, two ignored**, plus **305 selected
Python approval and gateway tests passed** across isolated commands. The
optional Slack Python adapter test could not collect because `aiohttp` is absent
from this checkout's virtual environment; native Slack routing is covered in
Rust. Both 76-case and 251-case oracles regenerate byte-for-byte. Rust and
Python formatting, Ruff, Clippy with warnings denied, and diff hygiene pass.

The refreshed [weighted full-port audit](analysis/progress-audit-2026-09-08.md)
is **54.90 points, reported as about 55%** (judgment range 53% to 57%). Smart
approval and Tirith findings, PTY and notifications, remote execution, file and
browser tools, and native plugin and memory managers remain.

## Native static approval deny rules: 2026-09-09

Native Unix-local terminal conversations now accept nonempty `approvals.deny`
lists when approval mode is off. Every foreground and background request passes
the hardline security floor first, then case-insensitive Python-compatible deny
matching over normalized and shell-carrier variants. A denied command returns
the frozen Python terminal envelope without starting a process.

The conversation-owned terminal reloads approval policy on every call. Valid
edits apply immediately, malformed YAML retains the last-known-good rules, and
a live mode change away from off fails closed. The provider-visible terminal
and process schemas stay byte-identical throughout the conversation.

A 59-case source-executed Python oracle pins parsing, normalization, glob
semantics, boundary behavior, precedence, envelopes, reload, and last-known-good
behavior. The live provider integration now exercises the terminal with a
nonempty deny list. See
[native-approval-deny-resolution.md](analysis/native-approval-deny-resolution.md).

AGY owned the Python contract oracle. Claude's first wiring audit predicted a
turn-lease deadlock. The later production integration proved that resolving
control replies before admission and lease acquisition avoids that deadlock;
the current interactive checkpoint above supersedes the earlier deferral.

The refreshed [weighted full-port audit](analysis/progress-audit-2026-09-08.md)
is **53.65 points, reported as about 54%** (judgment range 52% to 56%).

Validation is **1,771 Rust tests passed, two ignored**, plus **151 selected
Python approval and terminal tests passed**. The 59-case oracle regenerates
byte-for-byte. Rust and Python formatting, Ruff, Clippy with warnings denied,
and diff hygiene pass.

## Native managed background processes: 2026-09-09

The production native Unix-local terminal now starts managed non-PTY background
commands and exposes owner-scoped `list`, `poll`, `log`, `wait`, and `kill`
through `process_manage`. One gateway-owned registry survives frozen-client
eviction and compression rotation, while profile home plus stable gateway
session key prevent cross-conversation access. Active work now protects its
session route from pruning under the configured maximum-age policy.

The registry owns process groups, null stdin, concurrent output draining,
incremental UTF-8 decoding, a rolling 200,000-character buffer, exact status
publication, unique-prefix lookup, bounded wait and termination escalation, and
retention measured for 30 minutes after exit. Gateway shutdown signals all
groups in two shared grace windows. Background commands inherit the
conversation's isolated profile environment, persisted exports, cwd, redaction,
and unconditional terminal security floor.

The frozen model schema advertises only implemented capability. PTY input,
notifications and watchers, remote execution, restart adoption, systemd cgroup
isolation, and delegation attribution remain explicit later slices. A live
provider and SQLite integration performs foreground state changes, starts a
delayed background command, waits by returned process ID, observes its output,
and proves byte-identical schemas across all five provider requests. See
[native-background-process-resolution.md](analysis/native-background-process-resolution.md).

AGY owned the 71-case source-executed Python oracle and a separate parity
review. Claude owned an isolated Rust draft and a separate process-safety
review. The primary lane rejected incompatible draft choices, integrated the
gateway-owned boundary, and fixed the review's long-running-process retention
bug. The codebase-design skill informed the registry ownership and narrow
provider-facing interface.

Validation is **1,767 Rust tests passed, two ignored**, plus **197 selected
Python process, terminal, and session-reset tests passed, seven platform skips**.
The 71-case oracle regenerates byte-for-byte. Rust and Python formatting, Ruff,
Clippy with warnings denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is
**53.35 points, reported as about 53%** (judgment range 51% to 55%). Native
approval workflows, PTY and notification support, remote execution, file and
browser tools, and native plugin and memory managers remain.

## Native local foreground terminal: 2026-09-09

The first production native execution tool is live. Explicit Unix-local profiles
with `approvals.mode: off`, no user deny rules, and the existing native-tool
opt-in receive one session-bound `terminal` tool. Smart, ask, manual, cron,
single-query, and unattended approval modes, plus non-local backends, do not
advertise the native tool. The Python agent path remains required for those
policies until they are fully ported.

The foreground runner streams stdout and stderr concurrently into bounded head
and tail windows, creates unique private overflow spills, distinguishes exit,
spawn, and timeout outcomes, reaps its child, and kills the process group on
timeout or inherited-pipe linger. The terminal layer merges output, strips ANSI,
redacts secrets before returning or exposing a spill, preserves partial timeout
output, and returns structured command failures through the tool channel.

Each frozen conversation owns a serialized cwd and private shell environment
snapshot. Exported variables and completed `cd` changes survive later calls;
explicit `workdir` remains transient. SQLite cwd persistence resolves the
current route inside the update, so compression rotation moves later writes to
the child without touching its ended parent. Profile subprocess environments
now share one isolated builder with lifecycle hooks.

The unconditional security floor blocks destructive filesystem, raw-device,
fork-bomb, process-kill, shutdown, and unconfigured `sudo -S` commands even
when approvals are off. A source-executed Python oracle pins 233 argument,
validation, workdir, hardline, sudo, and approval-classification cases without
executing candidate commands. A real local provider integration proves two
same-turn terminal calls, exported-environment and cwd reuse, durable route cwd,
and byte-stable tool schemas across three requests. See
[native-local-terminal-resolution.md](analysis/native-local-terminal-resolution.md).

AGY owned only the Python contract oracle under its single-flight auth lock.
Claude owned only an independent foreground-runner draft. The primary lane
replaced its unbounded capture and fixed-name spill behavior, added process and
security hardening, implemented the terminal runtime, integrated it, and owned
validation. The codebase-design skill kept foreground lifecycle behind one
typed `run` interface and conversation state behind one tool.

Validation is **1,752 Rust tests passed, two ignored**, plus **167 selected
Python terminal and approval tests passed**. The 233-case oracle regenerates
cleanly. Rust and Python formatting, Ruff, Clippy with warnings denied, and diff
hygiene pass.

The refreshed [weighted full-port audit](analysis/progress-audit-2026-09-08.md)
is **52.10 points, reported as about 52%** (judgment range 50% to 54%). Native
approval workflows, background process management, remote execution backends,
file/browser/MCP tools, and native plugin and memory managers remain.

## Native same-turn compression rotation: 2026-09-09

Rotation-mode full compression now commits during a live native tool turn. A
shared `TurnSession` advances the physical session only after the existing
SQLite and routing transaction publishes the child, then aliases the held
process-local transcript lease to that child. The durable lease continues under
the unchanged compression-lineage root.

Every later tool persistence and maintenance pass reads the shared child ID.
HTTP and push owners refresh their message identity before final assistant
persistence, usage accounting, memory completion, and cache release. The
conversation wrapper observes the committed boundary and moves the exact frozen
client to the child key, preserving prompt bytes, tool ordering, plugin state,
provider routing, and the extension process. Multi-rotation finalization searches
the committed lineage newest-first, so a later notification failure cannot leak
an intermediate cached client.

Current-turn memory capture now anchors on the live user payload instead of a
prefix length that becomes stale after compaction. A full-suite failure also
exposed and corrected eager lease extraction on uncoordinated turns, preserving
the detached HTTP and push cancellation contract.

A public HTTP integration test runs both in-place and rotation modes through
real SQLite, a local provider, a Python extension host, a memory provider, and a
user hook. It executes one tool before compression and a second afterward,
proving parent closure, child routing, compacted follow-up input, child-only
post-boundary writes, child usage and callbacks, exact memory tool history, and
frozen-client reuse on the next turn. Focused tests cover CAS rejection,
two-rotation lease aliasing, and cache-observer failure after an intermediate
rekey. See
[native-same-turn-rotation-resolution.md](analysis/native-same-turn-rotation-resolution.md).

Validation is **1,729 Rust tests passed, two ignored**, plus **78 selected Python
rotation and persistence tests passed, one skipped**. Formatting, Clippy with
warnings denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **50.90
points, reported as about 51%** (judgment range 49% to 53%). Native terminal,
file and browser execution, plugin and memory managers, provider breadth,
remaining gateway platforms, interruption and overflow recovery remain larger
open areas.

## Frozen extension-host recovery: 2026-09-09

Conversation-scoped plugin and external-memory hosts now recover after a fatal
transport failure without rebuilding the Rust client, prompt, provider route,
or frozen tool schema. The ambiguous failed operation returns an error and is
never replayed. A replacement child repeats only initialization, then serves
later queued requests.

Recovery reuses the original profile-cleared process launch and selected secret
snapshot. It retains a successfully acknowledged compression-rebound session
ID and accepts the child only when its complete plugin and memory capability
projection exactly matches the first initialization. Capability drift kills the
replacement instead of mutating the conversation's provider-visible prefix.
Failed recovery is rate-limited, and teardown failures never initialize a fresh
provider merely to shut it down.

A crash-after-side-effect fixture proves no request replay, queue continuity,
session rebinding, scoped secret reuse, and foreign-profile secret isolation.
Other tests reject changed capabilities, reject malformed switch
acknowledgements, and prove flush failure does not respawn. The existing real
plugin and memory integration now also recovers from a timed-out plugin call. See
[extension-host-recovery-resolution.md](analysis/extension-host-recovery-resolution.md).

Full workspace validation is **1,724 passed, two ignored** (1,723 gateway plus
one core test). The selected Python extension-host protocol suites are **101
passed**. Formatting, Clippy with warnings denied, and diff hygiene pass.

The weighted native-replacement estimate remains **50.30 points, reported as
about 50%**. This checkpoint closes a production reliability gap but still uses
the Python compatibility host, so it would be misleading to count it as native
replacement scope. AGY and Claude prepared distinct Python-contract and
Rust-seam audits for native terminal work. They confirm that approval,
process-tree, persistent environment, output-spill, and single-owner routing
must land before registering a native shell.

## Native compression lifecycle hooks: 2026-09-09

Every committed native full-compression path now emits the generic
`session:compress` event through the selected profile's real user-hook
registry. The exact five-key Python payload is preserved: rotation reports the
active child and archived parent, while in-place compression reports an empty
`old_session_id`. A clone-shared one-based counter follows the frozen client
across cache rekeys.

The event is scheduled only after SQLite publication and after the external
memory callback attempt. It still runs when that callback returns a transport
error, but no hook can delay or roll back compression. The handler set is
discovered during conversation initialization without entering prompt or tool
bytes.

Python function hooks keep their `handle(event_type, context)` ABI through a
small compatibility runner. Handler processes start from a cleared environment,
receive only process-global and selected-profile values, and get a forced
`HERMES_HOME`. Timeout cleanup kills the whole process group. Nonzero exits and
malformed collected output are isolated so later handlers still run.

AGY owned only the Python runner, then separately reviewed the Python ABI.
Claude owned only Rust registry hardening, then separately reviewed Rust
lifecycle and profile safety. The primary lane owned production wiring,
boundary semantics, integration, corrections, documentation, validation, and
publication. See
[native-compression-hook-resolution.md](analysis/native-compression-hook-resolution.md).

Validation is **1,720 Rust tests passed, two ignored**, plus **157 selected
Python tests passed**. Formatting, Ruff lint and format checks, Clippy with
warnings denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **50.30
points, reported as about 50%** (judgment range 48% to 52%). Context-engine and
relay-boundary adoption, repeated-compression status, transparent extension-host
recovery, same-turn rotation, overflow recovery, and native plugin and memory
managers remain.

## Native compression-boundary rebinding: 2026-09-09

Every committed native full-compression path now notifies the real Python
`MemoryManager` through the persistent extension host. Rotation sends the
published child ID and archived parent ID. In-place publication sends the same
ID on both sides. Both use `reset=false`, omit a false `rewound` value from
provider kwargs, and report `reason="compression"` exactly like Python.

Notification happens after the SQLite commit and never rolls it back. The
bounded conversation cache pins the initialized client while the callback is
in flight, then atomically rekeys that exact frozen client to a rotating child.
Transport failure or an occupied target releases the stale parent. A pending
hard retirement follows a successful rekey and finalizes the child identity
once. This preserves the immutable prompt, tool/plugin snapshot, provider
route, and extension process across physical compression segments.

A live HTTP, SQLite, Python-child, and temporary memory-provider test proves
the same-turn callback and exact provider arguments. Manual and automatic
rotation tests independently read the committed checkpoint inside the observer.
Focused cache tests cover success, in-place retention, failure, target safety,
and the retirement race.

AGY ran once behind its auth lock and owned only the Python host endpoint and
tests. Claude owned only the Rust protocol client. The primary lane owned cache
transfer, caller integration, race handling, live proof, review, and validation.
See
[native-compression-boundary-rebind-resolution.md](analysis/native-compression-boundary-rebind-resolution.md).

Validation is **1,713 Rust tests passed, two ignored**, plus **110 selected
Python tests passed**. Formatting, Ruff lint, Clippy with warnings denied, and
diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **49.75
points, reported as about 50%** (judgment range 48% to 52%). Context-engine,
relay, and generic compression-event notifications, transparent extension-host
recovery, same-turn rotation, overflow recovery, and native plugin and memory
managers remain.

## Native pre-compression memory checkpoint: 2026-09-09

Native manual, automatic pre-turn, and same-turn full compression now call the
versioned external-memory checkpoint before summary provider I/O. The Python
extension host delegates to the real `MemoryManager`: legacy providers receive
the raw transcript, version 2 providers receive the shared direct-evidence
projection, and returned context is sanitized before crossing JSONL.

Every caller checkpoints the complete exact durable snapshot while summarizing
only its selected compression region. Required capability or callback failures
stop summary and publication without changing the transcript. Optional failures
continue without provider context. Successful context is encoded as a JSON
string in a source-material fence, not trusted as prompt instructions.

The wide-row lifecycle projection preserves tool, reasoning, replay, API-content,
and derivative-summary metadata. Preflight, checkpoint, summary, and normal turn
work reuse the same bounded per-conversation client, so this boundary does not
rebuild the frozen prompt or tool snapshot.

AGY ran once behind its auth lock and owned only the Python host endpoint and
tests. Claude owned only the Rust client protocol. The primary lane integrated
the three compression paths, corrected strict decoding, timeout, summary-marker,
and error-disclosure issues, and completed validation. See
[native-pre-compress-checkpoint-resolution.md](analysis/native-pre-compress-checkpoint-resolution.md).

Validation is **1,704 Rust tests passed, two ignored**, plus **78 selected
Python tests passed**. Both relevant source oracles, Rust formatting, Ruff lint,
Clippy with warnings denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **49.55
points, reported as about 50%** (judgment range 48% to 52%). Session-switch and
compression-boundary notifications, mid-turn rotation, overflow recovery,
auxiliary fallback breadth, and native plugin and memory managers remain.

## Native compression handoff assembly and tail anchors: 2026-09-09

Native manual, automatic pre-turn, and same-turn full compression now publish
the Python-compatible transcript plan instead of a fixed summary and
acknowledgment pair. The pure planner keeps protected head and tail rows,
normalizes old handoffs, selects the summary role against template-visible
neighbors, merges unavoidable collisions into the correct retained carrier,
and restores a real user anchor or the exact continuation placeholder.

Previous summary bodies are rehydrated separately from newly selected turns,
including structured content, without summarizing retained live carrier text a
second time. SQLite clones retained rows in planned order, preserves every
durable wide column, and rewrites only content, API content, and the summary
marker. Its exact CAS now includes replay, display, and timestamp fields.
In-place archive/search and immutable-parent rotation behavior remain atomic.

The live tool loop suppresses a follow-up provider request when only a
reference handoff would drive it. Real user input and in-flight tool exchanges
continue normally. Stored adjacent summary and anchor user rows are admitted
because the provider-bound repair merges them, while incomplete tool groups
remain rejected.

AGY implemented only the pure planner under its exclusive auth lock. Claude
worked only on the SQLite publisher and store interface, timing out after a
substantial draft. The primary lane reviewed and corrected that draft,
integrated every caller, and added prompt rehydration, suppression, and full
validation. See
[native-compression-handoff-tail-resolution.md](analysis/native-compression-handoff-tail-resolution.md).

Validation is **1,698 Rust tests passed, two ignored**, plus **73 selected
Python tests passed**. The 60-case oracle, formatting, Clippy with warnings
denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **48.85
points, reported as about 49%** (judgment range 47% to 51%). Mid-turn rotation,
memory checkpoints, extension notifications, overflow recovery, and auxiliary
fallback breadth remain in the compression cluster. Native plugin, memory,
tool, provider, and platform breadth remains the larger port.

## Native compression structural backoff checkpoint: 2026-09-09

Native full compression now treats an absent compressible window as a
structural no-op, not a failed summary. The per-conversation native client arms
a 300 second monotonic, process-local guard shared by its clones. Both pre-turn
and same-turn automatic compression honor it before summary I/O, while the
durable cooldown, ineffective strike count, and recovery deadline remain
untouched.

A committed compression boundary clears the guard. Manual `/compress` clears
it before its forced attempt and rearms it only if the transcript is still too
short. The bounded conversation cache forwards the state to the initialized
client keyed by profile home and session ID, so one conversation never blocks
another and no transient deadline enters SQLite.

Claude produced only a 24-case source-executed backoff oracle and golden
corpus. AGY separately mapped only the synthetic-user, multi-user tail,
reference-handoff, todo, role-alternation, and restart contracts for the next
checkpoint. The primary lane implemented the shared Rust state and live
integration. See
[native-compression-structural-backoff-resolution.md](analysis/native-compression-structural-backoff-resolution.md).

Validation is **1,659 Rust tests passed, two ignored**, plus **46 selected
Python tests passed**. Oracle regeneration, focused live tests, formatting,
Clippy with warnings denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **48.10
points, reported as about 48%** (judgment range 46% to 50%). Exact synthetic
and multi-user tail anchors, reference-only handoff suppression, overflow
recovery, memory checkpoints, notifications, and mid-turn rotation remain in
the compression cluster. Native plugin, memory, tool, provider, and platform
breadth remains the larger port.

## Native auxiliary compression routing checkpoint: 2026-09-09

Native full-compression summaries now resolve the frozen
`auxiliary.compression` route during per-conversation client construction.
Provider, model, endpoint, scoped credentials, named-provider headers and body,
reasoning control, timeout, and API mode are isolated from main conversation
requests. Auto routing inherits the resolved main route. Unsupported native
transports fail open to main-route compression instead of breaking startup.

Config-derived compression timeouts use Python's 300 second floor. A configured
output cap is admitted only for an exact concrete provider/model route that is
explicitly non-reasoning. Auxiliary failures, truncation, tool calls, and empty
answers get one uncapped retry on the main route. A live two-endpoint test proves
request isolation, tool-free summaries, fallback count, and successful
auxiliary short-circuit behavior.

AGY mapped runtime client selection, usage, startup, and fallback behavior.
Claude separately produced a 129-case source-executed config, cap, temperature,
and fallback oracle. The primary lane implemented and verified the Rust route.
See
[native-compression-auxiliary-routing-resolution.md](analysis/native-compression-auxiliary-routing-resolution.md).

Validation is **1,656 Rust tests passed, two ignored**, plus **113 selected
Python tests passed**. Oracle regeneration, formatting, focused live tests,
Clippy with warnings denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **47.90
points, reported as about 48%** (judgment range 46% to 50%). Configured
multi-provider fallback chains, credential refresh and rotation, non-chat
auxiliary transports, stall detection, and the remaining compression hooks are
still open. Native plugin, memory, tool, provider, and platform breadth remains
the larger port.

## Native same-turn full compression checkpoint: 2026-09-09

Native full LLM compression now runs after a complete tool-result batch is
durable and before the next provider request in that same turn. It prefers real
provider prompt usage, falls back to provider-visible request sizing including
tool schemas, preserves Python's post-compaction sentinel, and rearms its
per-turn attempt budget only after real usage proves the request is below the
threshold.

An admitted pass applies token-budget Phase 1 pruning, chooses a complete
partial-turn-safe summary window, makes one tool-free auxiliary request outside
SQLite, rejects partial or non-shrinking output, and publishes through the
existing exact-snapshot, route, live-session, and lineage-lease transaction.
The loop then adopts the durable active generation without duplicating rows.
The immutable system prompt remains byte-identical across main requests.

The default in-place mode is live. Rotation mode remains deferred because a
mid-turn rotation must rebind immutable client and lease identities safely.
Required memory checkpoints fail closed until their native hook exists.
Cooldown, breaker, prompt-usage precedence, completed-tool-batch validation,
and attempt rearming have focused coverage. A public HTTP and SQLite test proves
tool-result persistence before summary I/O and summary adoption before the
same-turn follow-up request.

AGY produced only the runtime contract map, running once behind its auth lock.
Claude separately produced only the 25-case source-executed Python oracle. The
primary lane owned Rust code, integration, corrections, validation,
documentation, and publication. See
[native-same-turn-full-compression-resolution.md](analysis/native-same-turn-full-compression-resolution.md).

Validation is **1,650 Rust tests passed, two ignored**, plus **251 selected
Python tests passed**. Oracle regeneration, formatting, Clippy with warnings
denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **47.50
points, reported as about 48%** (judgment range 46% to 50%). This percentage is
an inventory of production capability, not test coverage. Remaining compression
work includes mid-turn rotation, tail and handoff edge cases, structural
backoff, auxiliary routing and caps, required memory checkpoints, notifications,
and overflow recovery. Native plugin, memory, tool, provider, and platform
breadth remains the larger port.

## Native micro-compaction checkpoint: 2026-09-09

Opt-in rolling micro-compaction now runs on the native conversation client
after a successful assistant reply is durable, before external-memory
completion and before the next provider request. A due pass makes at most one
tool-free auxiliary call. It absorbs one complete assistant/tool exchange,
preserves every user byte, keeps tool groups whole, advances past poison
exchanges after three failures, rehydrates cumulative state on resume, and
defragments only a proven-contained marker.

The auxiliary boundary uses bounded Python-compatible exchange serialization,
secret and media-directive redaction, reasoning removal, a 1,500-token ceiling,
provider-aware temperature rules, and `compression` usage attribution. Empty,
reasoning-only, tool-calling, and length-truncated outputs are discarded. The
configuration remains disabled by default and is forcibly disabled when
checkpoint-required policy is armed.

SQLite publication runs in one immediate transaction after provider I/O. It
requires the exact active snapshot and unexpired lineage turn lease, validates
the complete candidate sequence, clones retained wide rows byte-exact, keeps
absorbed assistant/tool originals searchable, hides carried-forward duplicate
originals, persists the summary marker, and reconciles counters. An injected
failure proves the entire rewrite rolls back. A live HTTP and SQLite test proves
persistence-before-summary, one-call behavior, search semantics, usage
attribution, and marker adoption on the next provider request.

Claude produced only the 21-case source-executed Python oracle. AGY ran once
behind its exclusive auth lock and produced only the runtime contract map. The
primary lane owned production code, integration, corrections, validation,
documentation, and publication. The full disposition is recorded in
[native-micro-compaction-resolution.md](analysis/native-micro-compaction-resolution.md).

Validation is **1,647 Rust tests passed, two ignored**, plus **249 selected
Python tests passed, one skipped**. Oracle regeneration, formatting, Clippy with
warnings denied, and diff hygiene pass. The refreshed
[weighted full-port audit](analysis/progress-audit-2026-09-08.md) is **46.75
points, reported as about 47%** (judgment range 45% to 49%). The next
compression seam is same-turn LLM summary compression, followed by remaining
tail anchors, configurable auxiliary summary caps, routing/fallback,
checkpoints, notifications, overflow recovery, and structural backoff. Native
plugin, memory, tool, provider, and platform breadth remain the larger port.

## Native token-budget compression checkpoint: 2026-09-09

Native full compression now runs Python-compatible deterministic Phase 1
pruning before its summary request. The estimator accounts for ASCII, dense
CJK/Hangul, other UTF-8 text, structured and image content, complete tool-call
envelopes, generic thinking according to provider replay behavior, and Codex
reasoning/message sidecars. Its strict token boundary keeps the capped count
floor, and its three pressure stages can demote protected tool output, including
fresh skill bodies, only as context pressure requires.

Automatic summary selection now uses the configured lean or legacy token
budget instead of only a message count. It applies the Python 1.5x soft ceiling,
raw-budget retry, complete tool-group alignment, and recent user/assistant
anchors. Phase 1 publishes through the existing route, exact-snapshot, and
lineage-lease SQLite transaction, reloads the new active generation, and
finishes the already-admitted compression attempt without another provider
request or threshold decision between phases. Compression snapshots and their
publish guard now include all reasoning and Codex replay columns.

The source-executed Python differential corpus covers 17 estimator cases, 14
complete prune cases, and 3 tail-cut cases. Live push and HTTP tests prove
maintenance occurs before the triggering turn, preserves paired tool IDs,
leaves compacted originals searchable, and still executes and persists the
turn. Full workspace validation is **1,633 passed, two ignored** (1,632 gateway
plus one core test). The selected Python compressor suites are **200 passed**.
Formatting, Clippy with warnings denied, oracle drift checking, and diff hygiene
pass.

Helper work was split by independent deliverable. Claude produced only the
Python oracle and golden corpus. AGY ran once behind its exclusive auth lock and
produced only the live push/SQLite test. The primary lane owned production code,
shared types, integration, corrections, validation, and this checkpoint.

The refreshed [weighted full-port audit](analysis/progress-audit-2026-09-08.md)
is **46.20 points, reported as about 46%** (judgment range 44% to 49%). The next
compression seams are micro-compaction and defragmentation, same-turn LLM
summary compression, remaining synthetic/multi-user anchors, auxiliary routing
and fallback, memory checkpoints, extension notifications, overflow recovery,
and structural backoff. Native plugin/tool/provider/platform breadth remains
the larger port after this cluster.

## Same-turn proactive pruning checkpoint: 2026-09-09

Native tool-result pruning now runs inside the active tool loop after every
complete result batch is durably committed and before the next provider
request. The live conversation client receives its frozen compression policy
at construction. It sizes the provider-visible request, applies trigger,
protected-head/tail, durable rearm, and minimum-reclaim gates, then publishes
the candidate through the existing exact-snapshot, route, live-session, and
lineage-lease transaction.

The publication validator now accepts either a complete turn or a partial turn
ending in a fully answered tool-call group. Dangling calls remain rejected. On
commit, the loop replaces its in-memory transcript with the new durable active
generation while preserving the immutable system prompt. Prune discovery and
commit errors fail open, matching Python. There is no explicit client eviction:
the changed request bytes implicitly establish a new provider-cache prefix,
while reclaim and rearm gates keep cache breaks episodic.

The public HTTP integration test executes a real large-output tool and proves
that the second provider request in the same turn contains the compact summary,
not the original result. SQLite proves the summary is active, the original is
searchable, and final reply delivery still completes. Full workspace validation
is **1,621 passed, two ignored** (1,620 gateway plus one core test). The selected
Python usage, pruning, restart-safety, loop-wiring, and incremental-persistence
oracle is **91 passed**. Formatting, Clippy with warnings denied, and diff
hygiene pass.

AGY and Claude worked on separate lanes through the requested scripts. AGY
mapped only token-budget boundary and protected-tail pressure behavior for the
next pure-function slice. Claude mapped only same-turn runtime ordering,
adoption, failure, and cache contracts. The primary lane wrote and integrated
the Rust changes and caught one stale claim from the prior resolution: pruning
does not evict the cached conversation client.

The refreshed [weighted full-port audit](analysis/progress-audit-2026-09-08.md)
is **45.60 points, reported as about 46%** (rough range 44% to 48%). Token-budget
tail selection and pressure demotion are now source-mapped but not yet
implemented. Micro-compaction, exact estimator parity, same-turn full summary
compression, auxiliary routing and fallback, memory checkpoints, extension
notifications, and the larger plugin/tool/provider/platform surfaces remain.

## Native usage, tool history, and proactive pruning checkpoint: 2026-09-09

Native provider responses now normalize fresh input, output, cache-read,
cache-write, reasoning, and request counts with Python-compatible provider and
API-mode precedence. Main-turn usage updates both session totals and the model
ledger atomically. Compression usage is recorded under its auxiliary task and
does not inflate the session's main totals. Older Python stores with the stale
five-column usage primary key are rebuilt transactionally with `task` in the
key before any upsert. Streaming requests ask for usage chunks except on the
native Gemini host, matching the Python exclusion.

Native tool history is now durable and replayable. The assistant tool-call row
is validated and committed before any tool side effect. Each result is then
committed before another tool dispatch or provider request can occur. Later
turns rebuild the provider request from the full active wide transcript,
including call IDs, names, reasoning fields, and API-content sidecars. An
HTTP, SQLite, provider, and real-tool integration test proves both ordering
points and complete-group replay on the following turn.

Count-based proactive pruning now runs at the next admitted pre-turn boundary.
It summarizes old large tool results, deduplicates exact repeated output,
truncates stale tool-call arguments, and retires old images while preserving
complete tool groups and protected skill content. Publication uses one SQLite
transaction to verify the exact transcript snapshot and lineage turn lease,
archive the original active generation, clone every wide column, rewrite only
the selected fields, merge the durable rearm watermark, and reconcile live
counters. End-to-end coverage proves the provider sees the summary and the
archived original remains searchable.

Helper work was divided into separate dependency lanes. AGY mapped provider
usage only and was run singly behind its auth lock. Claude separately owned the
pure pruning function, the SQLite publication contract, and the Python tool
history ordering trace. The primary lane owned shared types, migrations,
runtime integration, source verification, and end-to-end validation. The full
disposition is recorded in
[native-provider-usage-pruning-resolution.md](analysis/native-provider-usage-pruning-resolution.md).

Full workspace validation is **1,620 passed, two ignored** (1,619 gateway plus
one core test). The selected Python
usage, pruning, restart-safety, loop-wiring, and incremental-persistence oracle
is **91 passed** under the project virtual environment. Formatting, Clippy with
warnings denied, and `git diff --check` pass.

The refreshed [weighted full-port audit](analysis/progress-audit-2026-09-08.md)
is **45.40 points, reported as about 45%** (rough range 43% to 47%). This is not
full compression parity. Same-turn post-tool pruning, token-budget pressure
demotion, micro-compaction, exact estimator parity, final savings measurement,
auxiliary model routing and fallback, memory checkpoints, and extension
notifications remain. The larger remaining systems are native plugin and
external-memory managers, extension-host recovery, the built-in tool runtime,
approval and delegation, provider failover, platform breadth, and native CLI
parity.

## Native automatic compression checkpoint: 2026-09-08

Automatic compression now runs on native HTTP and push ingress after stable
route, transcript, and cross-process lineage admission, but before the inbound
user message is persisted. The triggering message contributes to request
pressure without being included in the older history sent to the summarizer.

Pressure sizing uses the native provider-visible request after the immutable
system prompt, full active transcript, inbound structured content, frozen tool
schemas, request hooks, cache fields, and output cap are applied. Model context
comes from explicit overrides or the native models.dev resolver. Threshold
policy matches Python's output reservation, small-window 75% adjustment, 64K
floor, 85% degenerate cap, absolute token cap, longest model override, and
configured attempt limit. The old Rust-only 40-message history cutoff is gone.

The first pass preserves a complete initial turn and the configured recent
tail, then decays head protection after a durable checkpoint. SQLite clones
the protected prefix before the summary and the protected or concurrently
appended tail after it, retaining every wider message column. Default in-place
mode keeps the session and cached client. Explicit rotation publishes a child,
rebinds the process lease, closes the parent, and continues the triggering turn
on the child.

Summary failures arm a durable 600-second cooldown. Non-shrinking results add
an ineffective strike and the second strike arms a 300-second recovery window.
An expired breaker allows one recovery probe, and successful publication clears
the guards. HTTP and push integration tests prove ordering and both publication
modes.

AGY and Claude were used as different team lanes through the requested helper
scripts. AGY owned pure policy parsing and decisions. Claude owned durable
SQLite guard state. The main lane verified both against Python, corrected
mistakes in each, and owned request sizing, locking, publication, ingress, and
end-to-end tests. The disposition is recorded in
[native-automatic-compression-resolution.md](analysis/native-automatic-compression-resolution.md).

Full workspace validation is **1,589 passed, two ignored**. The selected Python
automatic threshold and guard oracle is **26 passed** under Python 3.11.15.
Formatting, Clippy with warnings denied, and `git diff --check` pass.

The refreshed [weighted full-port audit](analysis/progress-audit-2026-09-08.md)
is **44.05 points, reported as about 44%** (rough range 42% to 46%). This is a
real production vertical slice, not complete compressor parity. Provider usage
recalibration, token-budget tail selection, exact raw-count role-collision
handling, deterministic pruning, micro-compaction, structural backoff, final
request savings measurement, overflow recovery, auxiliary summary routing,
memory checkpoints, extension notifications, and aggressive deletion remain.
After those, the largest seams are native plugin and external-memory managers,
extension-host recovery, and the broader tool runtime and agent loop.

## Native in-place compression checkpoint: 2026-09-08

Manual `/compress` and `/compact` now match Python's default
`compression.in_place: true` behavior on HTTP and push ingress. A successful
compression keeps the same session ID, route, frozen system prompt, transcript
lease identity, and native conversation client. Setting
`compression.in_place: false` keeps the existing atomic rotation behavior.

The new SQLite publication path performs one `BEGIN IMMEDIATE` transaction. It
verifies the open session, exact durable route, and optional lineage-root turn
lease, then soft-archives summarized originals as searchable
`active=0, compacted=1` rows. Retained and concurrently appended rows are
re-sequenced after the summary pair through a byte-exact SQL clone, with their
superseded originals hidden from recall as `active=0, compacted=0`. Complete
tool groups and every wider Python message column survive the clone.

Live `message_count` and `tool_call_count` are recomputed at commit. Fresh Rust
databases now include the Python-compatible tool counter, and older/shared
databases add it safely. FTS search includes compacted originals but continues
to exclude rewind/undo rows. An injected insert failure proves archival flags,
new rows, and counters roll back together.

Tests prove the Python-default config, explicit rotation, same-ID continuation,
redaction, partial-tail preservation, concurrent tool-call tail cloning,
searchability, rollback, and that in-place compression does not evict the
cached client. Full workspace validation is **1,572 passed, two ignored**. The
Python in-place compaction oracle is **15 passed** under Python 3.11.15.
Formatting, Clippy with warnings denied, and `git diff --check` pass.

AGY and Claude were used through the requested helper scripts on different,
non-overlapping work: AGY mapped automatic trigger/pruning behavior, while
Claude mapped auxiliary routing, checkpoint/hook, cooldown, and in-place
policy. The main implementation and source-verified disposition are recorded
in [native-in-place-compression-resolution.md](analysis/native-in-place-compression-resolution.md).

The [weighted full-port audit](analysis/progress-audit-2026-09-08.md) is
**42.25 points, reported as about 42%** (rough range 40% to 44%). In-place
publication raises the state and native-core inventory, but not enough to
honestly round the overall port upward. Automatic request-pressure triggering,
provider usage capture, retry/rearm state, pruning/micro-compaction, auxiliary
summary routing/fallback, memory checkpoints, extension notifications,
aggressive deletion, and the broader tool/plugin runtime remain. This is not
completion of the full port.

## Native manual compression checkpoint: 2026-09-08

Native `/compress` and `/compact` now run on both HTTP and push ingress without
falling through to the ordinary agent turn. The parser supports preview and
dry-run aliases, partial `here` and `up-to-here` boundaries, `--keep`, and
focus text. Preview never calls a provider or mutates the route. Aggressive
mode and `compression.checkpoint_required` fail closed because their native
memory consumers are not connected yet.

Live compression uses one non-streaming, tool-free call through the current
native model. Its bounded prompt treats transcript text as untrusted data and
includes structured/API-sidecar content plus tool-call identity. A mandatory
redactor scrubs provider/token prefixes, auth headers, URL credentials and
secret query keys, config/JSON secret assignments, and private keys from input,
focus, model output, and logged provider failures. The shrink guard measures
the complete structured source rather than plain content alone.

Publication is child-first and atomic under one `BEGIN IMMEDIATE` transaction.
It verifies the durable route and turn-lease owner, copies the immutable prompt
and frozen tool/plugin fields, transfers `title` and `title_source`, writes the
checkpoint pair, validates and clones complete retained/concurrent turns
including tool groups, repoints the route, and closes the parent last. Stale
routes, lost leases, incomplete tool calls, competing compressors, and injected
write failures leave the conversation unchanged.

All native turns now use Python's shared `session_turn_leases` schema, keyed by
the compression-lineage root and refreshed across slow model calls. Waiters use
Python's 1,800-second budget instead of losing ingress after five seconds. They
reload the exact SQLite route after acquiring, so a compression committed by a
different process cannot orphan a queued turn on the ended parent. Release is
owner-checked, expired/dead and inactive same-process holders are reclaimable,
and manual mutation includes a short teardown grace.

Provider cache scope also follows the compression-lineage root. Publishing a
child releases the old physical client without firing true session-end hooks,
so later turns reuse the logical conversation's stable cache identity while the
obsolete client is evicted.

Full workspace validation is **1,570 passed, two ignored**. The selected Python
compression and cross-process lease oracle is **67 passed** under Python
3.11.15. Formatting, Clippy with warnings denied, and `git diff --check` pass.
Gemini and Claude were both used through the requested helper scripts for
source mapping, implementation review, and fix re-review. The maps, reviews,
and source-verified disposition are indexed under `rust/analysis/`.

The [weighted full-port audit](analysis/progress-audit-2026-09-08.md) is now
**about 42%** (rough range 40% to 44%). This checkpoint is deliberately manual
and rotation-only. Automatic threshold compression, configurable in-place
compression, auxiliary summary-model routing/fallback, pre-compression memory
and context-engine hooks, aggressive deletion, and the broader native tool
runtime remain. Next: finish those compression policies and hooks, then
transparent extension-host recovery, native plugin and external-memory
managers, and the remaining tool runtime and agent loop. This is not completion
of the full port.

## Native session title checkpoint: 2026-09-08

Native `/title` now reads or writes the current session title on both HTTP and
push ingress without reaching the model. `/new <title>` preserves the raw title
through destructive confirmation and applies it only after the new session is
durable. Bare `/title` reports the immutable session ID and current title. A
title command before the first model turn materializes the session row and its
full gateway peer identity immediately, including when validation rejects the
requested title.

Title cleanup matches `SessionDB.sanitize_title`: unsafe ASCII and Unicode
controls are removed, whitespace runs collapse, cleaned-empty input is rejected
at the command surface, and titles are capped at 100 Unicode characters. The
database API repeats sanitization as its own invariant and stores manual writes
with `title_source=user`.

Manual title mutation is one `BEGIN IMMEDIATE` transaction containing the
hidden canonical Bot Chat guard, exact uniqueness lookup, compression-ancestor
title transfer, and NULL-safe compare-and-swap update. The Python-compatible
partial unique index is the final cross-process guard. Old Rust databases repair
duplicate titles by retaining the newest row and drop the obsolete non-unique
index. `SessionDb` now uses Python's five-second SQLite busy wait so shared
Python/Rust profiles do not fail immediately under a concurrent writer.

Title metadata never enters the transcript, bumps the durable conversation
generation, changes prompt bytes, or evicts the client keyed by profile home and
session ID. Tests prove cached-client reuse, zero model turns for control
traffic, cold-row creation, route and transcript lease ordering, two-connection
title contention, canonical Bot Chat refusal, lineage transfer, legacy index
repair, duplicate and invalid reset titles, and HTTP/push symmetry.

Full workspace validation is **1,547 passed, two ignored**. The selected Python
title oracle is **21 passed** under Python 3.11.15. Formatting, Clippy with
warnings denied, and `git diff --check` pass. Gemini and Claude were used through
the requested helper scripts for pre-implementation mapping and independent
post-implementation review. The maps, reviews, and verified disposition are
indexed under `rust/analysis/`.

The [weighted full-port audit](analysis/progress-audit-2026-09-08.md) remains
**about 41%** (rough range 39% to 43%). This checkpoint closes manual title
lifecycle behavior but does not change the much larger remaining tool, plugin,
memory, provider, adapter, UI, and agent-loop inventory. Next: real native
compression boundaries, then transparent extension-host respawn, native plugin
and external-memory managers, and the remaining tool runtime and agent loop.
Automatic derived/LLM titles, Telegram topic rename, desktop/web title state,
and `/sessions search` remain explicit follow-ups. This is not completion of the
full port.

## Native secure resume checkpoint: 2026-09-08

Native `/resume` and `/sessions` now execute as gateway control commands on
both HTTP and push ingress. Direct IDs win over titles, exact title lookup
follows the newest numbered continuation, numeric choices use the recent named
list, and `/sessions full` exposes untitled native sessions with discoverable
IDs and first-message previews. Shell quoting, literal wrapper removal,
one-based range errors, current-session no-op replies, and non-admin widening
notices follow the Python command surface. `/sessions search` remains explicit
about being unavailable instead of falling through to the model.

The authorization boundary treats every ID, title, and list row as a routing
handle rather than authority. Normal resume requires the same profile database,
stable route, platform, chat, thread, and DM user. Rows owned by a different
HTTP sender on the same channel are neither listed nor resumable. Cross-origin
access requires an explicit `--all` or `--cross-room` from an admin configured
under an enabled slash policy. An ungated platform grants command use but never
grants the data-access override.

The durable transition is one `BEGIN IMMEDIATE` SQLite transaction. It closes
the outgoing routing epoch as `session_switch`, bumps its conversation
generation, stamps legacy reset children, reopens the existing target, updates
the target and compression ancestors with the live peer, and upserts the stable
route. In-memory routing is published only after commit. An injected trigger
failure proves the session rows, generation and route all roll back together.

The async path acquires the stable-route lease followed by both transcript
leases in sorted ID order. It rechecks the route and authorization after
locking, then retries lost compare-and-swap races. A real held HTTP turn proves
resume waits for the assistant reply to persist into the outgoing transcript
before switching.

Conversation clients remain keyed by profile home plus immutable session ID.
Resume does not hard-retire the switched-away client or run end-of-session
extraction. A real HTTP round trip resumes a still-warm prior client without a
rebuild and supplies its original transcript, while reset still performs its
true hard boundary.

Coverage includes ID/title/number selection, wildcard escaping, titled and full
listing, first-message previews, same-channel foreign-user and foreign-chat
rejection, explicit-admin widening, non-admin downgrade, current-target no-op,
compression-tip selection, stale CAS, atomic rollback, warm-client reuse,
in-flight ordering, and model exclusion across real HTTP and push paths. Full
workspace validation is **1,541 passed, two ignored**. The selected Python
resume contract suite is **35 passed** under the checkout Python 3.11.15.
Formatting, Clippy with warnings denied, and `git diff --check` pass.

Gemini and Claude were both used through `rust/tools/agy.sh` and
`rust/tools/claude.sh` for source mapping and post-implementation review. Their
reports and the independently checked resolution are indexed under
`rust/analysis/`. The transaction design also followed the repository's ACID
discipline: one minimal durable write set, deterministic locking, and no
provider or network I/O inside the transaction.

The [weighted full-port audit](analysis/progress-audit-2026-09-08.md) remains
**about 41%** (rough range 39% to 43%). This closes one important gateway
lifecycle seam but does not materially change the much larger remaining agent,
tool, plugin, memory, adapter and UI inventory. Next: native `/title` and
`/new <title>` persistence, then real
compression boundaries, transparent extension-host respawn, native plugin and
external-memory managers, and the remaining agent loop and tool runtime.
`/sessions search`, automatic titles, the full reset/branch/delegate-aware
descendant resolver, Matrix room semantics, and adapter picker UI remain
explicit follow-ups. This is not completion of the full port.

## Native destructive slash confirmation checkpoint: 2026-09-08

Native `/new` and `/reset` now use the Python-compatible destructive-command
confirmation flow across HTTP and push ingress. Confirmation is enabled by
default, reads `approvals.destructive_slash_confirm` from the active config for
every command, and supports the exact Once, Always and Cancel text forms. The
prompt and its answers are gateway control traffic, so they never enter the
transcript, reach the model, mutate the frozen conversation prompt, or create a
provider client.

Pending state is keyed by the stable route and shared across ingress paths. A
new prompt supersedes the prior one for that route, entries expire after 300
seconds, and a recognized response removes the entry under the lock before any
asynchronous reset work. Concurrent duplicate replies therefore execute at
most once. Authorization is checked against the original `/new` command, and
the resolver already exposes the precedence input required when native blocking
tool approvals arrive. Automatic rotation clears old pending state at the
session boundary.

No admission lease is held while waiting for confirmation. Approval enters the
existing compare-and-swap reset path only after the reply, so it serializes
behind an in-flight turn and retires the predecessor after durable route
progression. Cancel leaves both the route and model untouched. A confirmed
reset error is consumed once and returns the same handler-error form as Python.

Always Approve persists `approvals.destructive_slash_confirm: false` before the
reset. The new native round-trip writer preserves comments, ordering and quoted
values, validates its result, writes and flushes a private temporary file,
atomically replaces the target, flushes the parent directory, and preserves a
managed config symlink. A failed preference write still runs the approved reset
and reports that the prompt will return.

Coverage includes the exact parser table, Python truthiness, live reload, null
config, timeout, supersession, route isolation, concurrent exactly-once
resolution, authorization and tool-precedence behavior, comment-preserving and
symlink-safe writes, real HTTP prompt/Cancel/Always/immediate-next-reset flows,
push confirmation, and reset behind an in-flight turn. Full workspace
validation is **1,537 passed, two ignored**. The selected Python confirmation
oracle is **22 passed**. Formatting, Clippy with warnings denied, and
`git diff --check` pass.

Gemini and Claude were both used through `rust/tools/agy.sh` and
`rust/tools/claude.sh`. Their reports and the independently verified disposition
are indexed under `rust/analysis/`.

The evidence-based full-port estimate remains **about 41%** (rough range 39% to
43%). This checkpoint closes a small part of the gateway/session slice and does
not justify rounding the overall estimate upward. Next: add the titled-session
index and ownership checks required for secure native `/resume`, then continue
real compression boundaries, transparent extension-host respawn, native plugin
and external-memory managers, and the remaining agent-loop and tool runtime.
Adapter confirmation buttons arrive with the native adapter callback surface;
`/undo` and `/clear` join this shared primitive when their operations are
ported. This is not completion of the full port.

## Explicit native session rotation checkpoint: 2026-09-08

`/new` and its `/reset` alias now execute as native gateway lifecycle commands
on both HTTP and push ingress. Alias canonicalization happens before slash
access checks, the commands never reach the model, and a first-message reset
creates one fresh session without an empty phantom predecessor. Title arguments
are acknowledged as unavailable until the native title index lands instead of
being silently discarded.

The session store now owns an explicit compare-and-swap reset transition. It
preserves route origin and display identity, marks the new entry as a fresh
reset, promotes the predecessor row to `session_reset`, bumps its durable
conversation generation, and creates the child with both `parent_session_id`
and `model_config._reset_from`. The predecessor conversation client is retired
through the existing deferred, transcript-aware hard teardown path.

Turn admission is now stable across ID rotation. A separate per-route admission
lease spans source resolution through acquisition of the existing session-ID
transcript lease. The observed route is checked again before any history read;
a waiter that resolved an old ID releases it and resolves the new route. This
keeps both invariants: one stable route cannot rotate during admission, and
multiple routing keys that point at one transcript still serialize on that
transcript's ID.

`/compress`, `/compact`, `/resume`, and `/sessions` are recognized lifecycle
commands and return explicit native-backend availability messages. They cannot
fall through to the model. This avoids pretend compression and avoids exposing
a session switch before the titled-session index, compression-tip selection,
and cross-user/cross-chat ownership checks are complete.

Coverage includes alias and command classification, reset publication fencing,
empty-route creation, SQLite lineage and conversation generation, stale waiter
re-resolution, real HTTP reset and cached-client retirement, real in-flight
persistence ordering, push reset delivery, and unported-command non-forwarding.
Full workspace validation is **1,523 passed, two ignored**. Conversation prompt,
plugin prompt, session lifecycle, and session reset source checks pass, as do
Python compilation, formatting, Clippy with warnings denied, and
`git diff --check`.

Gemini and Claude were both used through `rust/tools/agy.sh` and
`rust/tools/claude.sh`. Their source audits and the independently verified
implementation disposition are indexed under `rust/analysis/`.

Next: port the destructive slash-confirmation state machine and live config
write used by `/new`, then add the titled-session and ownership surfaces needed
for secure `/resume`. Real auxiliary-model compression, durable transcript
compaction/rotation, prompt snapshot transition, native plugin and external
memory managers, transparent extension-host respawn, and remaining agent-loop
behavior still follow. This is a session-boundary checkpoint, not completion of
the full port.

## Bounded native conversation lifecycle checkpoint: 2026-09-08

Native per-conversation clients are now bounded owners instead of an
ever-growing prompt cache. The cache keeps Python-compatible defaults of 128
entries and a 3,600-second idle threshold, enforces LRU capacity on checkout,
uses the existing pressure policy, and runs expiry, idle and pressure
maintenance on the gateway lifecycle. Profile home plus immutable conversation
ID remains the key, so a reset cannot silently reuse a stale system-prompt
prefix.

One pending counter now covers the full turn, including assistant persistence
and external-memory finalization. Capacity, idle and pressure paths skip active
entries. Reset attaches hard retirement until the finalizer completes, expiry
returns for a later watcher pass, and shutdown gives active finalizers a bounded
grace before deliberately forcing the remaining clients. Cache mutation is
synchronous and small; construction, SQLite transcript loading, provider calls
and process teardown all happen outside the leaf mutex.

Capacity and pressure retirement eagerly extracts finite-session memory before
dropping the owning extension child, matching Python's pre-eviction
`commit_memory_session` intent. Mode `none` idle entries use release without a
session transcript, while finite unexpired sessions remain owned until expiry
or another pressure boundary. Linux pressure measurement now includes the
gateway's descendant process tree, so per-conversation Python hosts cannot hide
behind parent-only RSS.

Automatic resets consume and persist their predecessor marker exactly once,
then retire the old conversation for both HTTP and push dispatch. Policy expiry
loads the durable structured transcript, runs provider teardown, and atomically
marks `expiry_finalized`, promotes the session end reason, and bumps the
conversation generation. The database operation is idempotent and preserves an
existing explicit compression boundary.

Extension-host close is now awaitable. It drains queued writes, optionally
delivers `session_end` with the SQLite transcript, sends `shutdown`, and waits
for the child worker to exit. Each stage is bounded, later cleanup still runs
after an earlier best-effort error, and gateway shutdown joins or aborts every
tracked retirement task within its total budget. A real Python-child test proves
the session transcript event and shutdown sentinel.

Coverage includes LRU and pending-finalizer protection, idle and pressure
policy, retryable construction, reset deferral, policy expiry, ordinary and
forced shutdown, failed-turn single release, descendant RSS, transcript shape,
idempotent database boundaries, routing-marker consumption, a real extension
child, and a real two-request HTTP auto-reset. Full workspace validation is
**1,515 passed, two ignored**. Conversation prompt, plugin prompt, session
lifecycle, and session reset generators pass their source checks. Formatting,
Clippy with warnings denied, Python compilation, and `git diff --check` pass.

Gemini and Claude were both used through `rust/tools/agy.sh` and
`rust/tools/claude.sh` for design and post-implementation audits. The four raw
reports and the independently verified disposition are indexed under
`rust/analysis/`.

Next: port explicit `/new`, `/reset`, resume and compression command boundaries
so manual rotation drives the same cache retirement and prompt invalidation
contract. Transparent extension-host respawn, built-in memory-tool write
mirroring, native plugin and memory managers, route-specific capability
overlays, plugin session-finalize hooks, active-process expiry suppression, and
remaining agent-loop behavior still follow. This is a bounded ownership
checkpoint, not the completion of the full port.

## Native external-memory turn lifecycle checkpoint: 2026-09-08

Native conversations now drive the configured Python external-memory provider
on every eligible turn without returning model ownership to Python. Turn start
calls `on_turn_start`, applies Python's trivial-prompt gate and bounded prefetch,
uses the canonical memory-context composer, and emits a structured recall
notice. Dynamic recall never changes the frozen system prompt or inserts a
synthetic role.

SQLite now creates and migrates the nullable `messages.api_content` sidecar.
The clean user content and exact API-bound bytes stay separate through history
loading, and the existing wire projection substitutes the sidecar only on the
provider request copy. The current row is updated atomically before model I/O,
with a newest-row identity check that cannot reach backward to an older matching
message. Existing stores inspect the schema read-only and take an immediate
write transaction only for the one-time migration.

Successful turns capture the real current-turn transcript, including assistant
tool calls and tool results, then defer external-memory sync until the gateway
has durably appended the final assistant reply. HTTP and push dispatch share the
new post-persist finalizer. Python's existing single-worker queue retains FIFO
write ordering and keeps provider latency off the turn path; nontrivial turns
also warm the provider's next prefetch. Model errors, interruptions and empty
responses do not sync.

The compatibility protocol also exposes bounded session-end and queue-drain
operations for the next ownership checkpoint. Turn-start transport allows
headroom around Python's 8-second provider prefetch, while a truly wedged child
still fails open for the model turn and is terminated rather than pinning the
JSONL worker forever.

Real Python-provider, SQLite and local HTTP coverage proves recall persistence
before the first model request, byte-stable wire replay, full tool transcript
sync, durable assistant persistence before provider observation, multimodal
flattening, trivial/interrupted/empty gates, FIFO drain, and provider shutdown.
Full workspace: **1,500 passed, two ignored**. The conversation and plugin
prompt generators pass their source checks. Formatting, Clippy with warnings
denied, Python compilation, and `git diff --check` pass.

Gemini and Claude mapped and reviewed the lifecycle and cache seams through the
required helper wrappers. Their four source reports and the verified review
disposition are indexed under `rust/analysis/`.

Next: bound `ConversationAgent` with the existing cache configuration and
pressure planner, protect in-flight clients, and wire TTL, LRU, reset, expiry
and shutdown paths to session-end extraction, queue drain and child teardown.
Structured lifecycle-notice rendering, transparent host respawn, compression
checkpoints and built-in memory-tool write mirroring remain later milestones.

## Live extension-host compatibility checkpoint: 2026-09-08

Native conversations can now use existing Python plugin prompt sections,
plugin tools, and one configured external-memory provider without handing the
model turn back to Python. A persistent child is owned by each cached
conversation and is started only when memory, an enabled standalone plugin, or
an explicitly selected bundled backend toolset requires it. The default path
still starts no Python process and produces the same prompt bytes and native
tool surface.

The child binds the selected profile home and session cwd before discovery,
uses the real plugin manager, tool resolver, dispatch middleware, memory
manager, availability checks, and shutdown drain, and carries the stable
gateway route key separately from the transcript session ID. Fresh construction
renders external memory and plugin sections in Python order before persisting
the complete prompt. Stored prompts skip those callbacks and reuse their exact
bytes. Snapshot failure drops the extension prompt and tools coherently before
provider I/O.

The JSONL boundary is serialized and bounded. It rejects oversized responses
and mismatched IDs, scans past a limited amount of stdout contamination,
distinguishes recoverable application errors from fatal transport failures,
and kills the child process tree on timeout or teardown. Scoped conversations
start from an explicit process-global environment allowlist, then install only
the selected profile snapshot over the private stdin pipe. Rust-native tool
names are reserved in the Python registry, so its existing explicit override
and operator-consent checks also protect cross-runtime collisions.

Native tools are now asynchronous, preserve provider-specific schema fields,
and return structured JSON values. Supported multimodal plugin results are
converted to provider-valid content arrays or text fallbacks before request
projection. Real subprocess, SQLite, and local HTTP tests cover prompt
persistence before model I/O, callback-free resume, plugin and memory toolset
gates, schema preservation, collision routing, profile isolation, route-key and
cwd propagation, stdout defense, recoverable errors, fatal timeouts, graceful
shutdown, and multimodal replay. Full workspace: **1,498 passed, two ignored**.
The 12-case conversation restore oracle and existing plugin prompt tests pass.
Formatting, Clippy with warnings denied, Python compilation, and
`git diff --check` pass.

Gemini and Claude mapped and reviewed this seam through the required helper
wrappers. Their four reports and the verified disposition are indexed under
`rust/analysis/`.

Next: add external-memory turn lifecycle calls such as prefetch and write/sync,
then implement conversation-client TTL/LRU and reset-driven teardown so cached
sessions cannot pin extension children indefinitely. Mid-conversation host
respawn, richer gateway identity fields, compression-triggered prompt
invalidation, dynamic registry drift, route-specific toolset overlays, and a
Windows Job Object for abrupt descendant cleanup remain. This checkpoint is a
production compatibility host, not the final native plugin/provider manager.

## Frozen native conversation state checkpoint: 2026-09-08

Continuing native conversations now retain the exact tool prefix that was
accepted with their stored prompt. Fresh construction persists ordered tool
names after the prompt write and before provider I/O. Resume parses that state,
replaces saved slots with current registered definitions, keeps registered
tools that are temporarily unavailable, drops removed tools, preserves Python's
duplicate-name behavior, and appends newly available tools in fresh order.
Invalid persisted JSON falls back to the fresh surface without breaking the
turn.

SessionDb now creates and migrates the `tool_names` column and exposes the same
nullable versus stored-empty distinction as Python. The write is a short
single-statement SQLite boundary with no network or model work inside it. The
ACID transaction checklist guided that persistence boundary.

The native conversation owner also carries a callback-free plugin prompt
snapshot restored from the accepted system-prompt bytes. Native plugin callbacks
are not wired yet, so fresh native prompts still contain no plugin sections.
Owning the restored snapshot now prevents later compression work from silently
consulting live plugin state. Static-prefix reconstruction remains deferred
until the native request path has a segmented cached prefix to reconstruct.

The real SQLite and local HTTP integration test now changes `agent_tools` from
enabled to disabled between process-level initializers. It proves that prompt
and tool state are present before the first request, exact prompt bytes are
reused, and the frozen `current_time` schema remains identical on both model
requests. Full workspace: **1,492 passed, two ignored**. The 12-case conversation
restore oracle and 39-case plugin prompt oracle pass. Formatting, Clippy with
warnings denied, and `git diff --check` pass.

Gemini and Claude mapped and reviewed this seam. Their reviews caught the
full Python restore function's two-stage duplicate behavior, which is covered
by the final tests. Reports are indexed under `rust/analysis/`.

Next: add native plugin and external-memory managers, then connect their live
rendering and tool registration to this frozen conversation owner. Dynamic
registry, `check_fn`, MCP-derived tool availability, message-count drift
eviction, compression-triggered invalidation, and conversation-client TTL/LRU
policy remain. Do not rebuild a live prompt for ordinary file or config drift.

## Live native conversation prompt checkpoint: 2026-09-08

Native routed conversations now initialize asynchronously and attach one
immutable system prompt before provider I/O. ConversationAgent uses a per-key
async single flight, so unrelated profile/session builds do not block each
other, failed builds remain retryable, and no client-map lock reaches model
I/O. The selected profile home, resolved message, prior history and actual
SessionDb all reach the initializer.

The live fresh builder now drives the ported three-tier assembler in Python
order: configured skills and stable identity, provider/environment/coding
guidance, the bounded local Python toolchain probe, Bot Chat protocol and
capability epoch, profile/platform guidance, context files, frozen built-in
memory and the captured timezone/session footer. Inputs that can drift are
captured once by the process initializer. The executable native tool list is
the prompt's advertised tool surface, so unavailable skills/tools are not
claimed. Native plugin callbacks and external-memory providers remain absent
because those native capability managers are not wired yet.

conversation_prompt::restore_or_build now accepts the asynchronous fresh build.
Stored runtime and Bot Chat decisions happen before construction; a rebuild
receives database metadata captured before its first await, then persists after
assembly and before NativeAgentClient can issue a request. Matching stored
prompts skip all fresh-prompt source reads and are attached byte-for-byte.
Lineage metadata can now be supplied to the footer without querying SQLite
inside prompt assembly.

A real SQLite plus local-model integration test proves the prompt row is
already populated when the HTTP handler sees the first request. It then changes
SOUL.md, constructs a new initializer and proves both requests carry the exact
stored bytes. The existing 12-case Python restore oracle still passes its
generator check. The bounded environment-probe renderer has a focused test.
Full workspace: **1,488 passed, two ignored**. Formatting and Clippy with
warnings denied pass. Gemini and Claude independently mapped the seam in
analysis/async-conversation-prompt-map-agy.md and
analysis/async-conversation-prompt-map-claude.md; their lock and ordering
findings were applied.

Next: restore persisted native tool-prefix and plugin snapshot state into a
conversation owner, connect native plugin/external-memory managers, and add
message-count drift eviction plus compression-triggered prompt invalidation.
Conversation-client TTL/LRU policy also remains. Do not rebuild a live prompt
merely because files or ordinary config changed mid-session.

## Stored conversation prompt decision checkpoint: 2026-09-07

`conversation_prompt::restore_or_build` now ports the orchestration around
persisted prompt state. It reads only when prior history exists, distinguishes
missing/null/empty/present rows, reuses exact bytes only after runtime checks,
supports capability and title-gated Bot Chat refresh decisions, and persists a
fresh build best-effort. Reuse reports the pending frozen plugin/tool/static
prefix restoration hooks explicitly. Reads and writes complete outside prompt
construction and model I/O; the existing conversation lease supplies local
same-session serialization. The ACID transaction review guided this boundary.

Twelve cases generated by actual Python `_restore_or_build_system_prompt` pass:
first turn, missing/null/empty, exact reuse, model/provider/platform/cwd drift,
capability refresh, Bot Chat legacy migration and ordinary legacy reuse. The
generator passes `--check`. ConversationAgent's factory now receives prior
history as well as the resolved message and selected database. Full workspace:
**1,486 passed, two ignored**. The module is marked staged because production
does not yet provide a complete fresh-prompt builder; this checkpoint does not
claim live restoration. Clippy passes with warnings denied.

Next: make conversation construction asynchronous, assemble the complete fresh
prompt from captured session inputs, invoke this decision module, attach the
result to NativeAgentClient, and persist it before the model call. Then restore
the frozen tool/plugin/static-prefix state on reuse and add eviction/runtime
drift handling. Do not attach partial prompt content as a production fallback.

## Conversation initialization context checkpoint: 2026-09-07

Both live ingress paths now pass TurnContext through run_turn_with_context,
replacing the home-only boundary. It retains a borrow of the actual selected
SessionDb and its home; the conversation factory receives that database and the
resolved Message. It can inspect persisted session data without reopening an
assumed home/state.db path. The HTTP/local-model test verifies the constructor
can read the selected durable session, and push/profile-routing tests still pass.
Full workspace: **1,485 passed, two ignored**. Clippy passes with warnings denied.

Reference inspection found that stored-prompt adoption must also preserve
_restore_or_build_system_prompt behavior (agent/conversation_loop.py): it reads
stored state only with prior history, checks runtime identity, considers Bot
Chat capability/legacy upgrades, and restores plugin/tool snapshots on reuse.
Missing/null/empty states and read/write failures have distinct warning behavior.
Existing system_prompt::stored_prompt_matches_runtime and bot_mode guards cover
parts of this, but the orchestration is not yet ported. Do not blindly attach a
persisted string in main. The factory still needs prior-history presence and an
async initialization path before assembling and persisting a new prompt; current
factory construction only selects provider/config. No helper is active.

## Routed native conversation clients checkpoint: 2026-09-07

Production push and HTTP turns now pass the selected SessionDb's owning home
through AgentClient::run_turn_in_home. Its default preserves existing CLI/Python
backend behavior. Native startup wraps the client in ConversationAgent, whose
factory calls build_agent_client_for_home with the selected profile config/model.
Clients are retained by (profile home, resolved conversation ID), so distinct
profiles and reset IDs no longer share the same native configuration object.
A failed selected-profile build returns an error rather than using another
profile's credentials. Turns with no database retain the existing fallback path.

The initialization lock protects client creation and is released before provider
I/O. No DB transaction logic was changed. The ACID skill's lock review was used.
Inline tests cover reuse, profile separation, new IDs, returning to an existing
ID, and failed builds without cross-profile fallback. The real HTTP/local-model
and push/store-restart tests additionally verify the profile context reaches the
agent call. Full workspace: **1,485 passed, two ignored**; strengthened ingress
tests pass afterward, and Clippy passes with warnings denied.

This is live agent selection, not completed prompt initialization. The retained
map currently has no eviction/TTL or runtime config-drift policy. Python's full
agent-cache lifecycle, prompt restoration/persistence, session-captured source
inputs, and immutable prompt construction remain next. The factory currently
receives home only; it must gain the remaining internal conversation context
before attaching the assembled prompt. Do not inject one global project prompt.
Gemini review 63300 finished; analysis/native-prompt-lifetime.md is advisory and
predates this routing change. No helper remains active.

## Native session integration trace: 2026-09-07

Verified both production ingress paths: dispatch.rs and message.rs resolve a
session/profile database, then still invoke the shared startup agent. Native
run_turn changes cache_scope by resolved session ID but does not select a new
profile or initialize/restore its prompt. build_agent_client_for_home exists,
but production only calls it through the startup wrapper; routed calls are
currently test-only. This makes routed agent selection a prerequisite alongside
prompt initialization, not just a matter of calling load_skills in main.

Concrete findings and required live-path tests are recorded in
analysis/native-session-integration-findings.md. No source edits were made in
this trace; prior validation remains 1,484 passed, two ignored, and Clippy clean.
Gemini's independent lifetime review is running through the existing agy.sh lock:
session 63300, log /tmp/hermes-native-prompt-lifetime.log, expected artifact
analysis/native-prompt-lifetime.md. Poll that handle before launching more work.
Next implement a shared conversation initialization path after routing, preserving
selected profile credentials, stored prompt reuse and per-conversation lifetime.

## Skills prompt initialization checkpoint: 2026-09-07

`ResolvedPromptSections::load_skills` now calls the configured retained loader
only when the captured tools include skills_list, skill_view or skill_manage.
A closed gate clears the section and performs no skill/config scan or snapshot
write. Errors from an enabled loader propagate, matching Python's initialization
path. Stable initialization derives its help-pointer input from the loaded
skills section, so the hermes-agent pointer requires its actual admitted entry.
The rendered index enters the existing volatile assembly slot.

All 17 prompt tests pass, including real configured loading, the no-I/O tool
gate, stable help-pointer selection and assembled skill content. Full workspace:
**1,484 passed, two ignored**. Clippy passes with warnings denied.

Production integration still requires session-level prompt initialization. The
main factory currently builds a shared NativeAgentClient, registers at most
CurrentTimeTool, and has no conversation-specific prompt inputs at that point.
Do not attach a project-specific prompt globally there. Next inspect dispatch
session creation and native agent ownership, then connect captured session/profile
inputs and immutable prompt bytes at the correct lifetime boundary. Other native
core/tool/runtime requirements in this plan remain incomplete.

## Configured skills prompt loader checkpoint: 2026-09-07

`PromptLoader::render_configured` now connects its retained SourceCache with
captured expansion, project trust resolution, disabled-name config, admission,
profile/external ingestion and the rendered LRU. SourceContext explicitly carries
config path, profile home, skills root, OS home, launch cwd, project start and
captured Unicode environment. Relative external paths anchor to profile home;
relative trusted project entries anchor to launch. Platform is supplied by the
caller or resolved from the captured HERMES_PLATFORM/HERMES_SESSION_PLATFORM.
Disabled names are derived from the same owning profile config.

The real-files configured test passes with distinct launch/profile/project
roots, creation and external directories, relative trust, platform-disabled
skills, profile-scoped snapshots and attestations, and repeat rendering. The
same scenario through Python's public build_skills_system_prompt passes with
real config/imports and temporary HERMES_HOME/TERMINAL_CWD. Clippy passes with
warnings denied.

Next: wire the configured loader into system-prompt initialization with skills
tool gating and the complete captured session inputs. Preserve the retained
owner and synchronization and freeze the resulting prompt per conversation.
Production startup still does not invoke this path. Unicode-only expansion and
existing YAML/scan parity limits remain; this is not full native core completion.

## External skill source cache checkpoint: 2026-09-07

`skill_discovery::SourceCache` now owns RawConfigCache plus resolved external
paths keyed by config path and modification time only. Creation roots are
resolved and checked every call, using raw config's independent time/size cache.
External hits return a copy without rerunning expansion or existence checks.
Falsy external settings cache an empty list; truthy malformed types and invalid
config mappings do not. Clear resets both caches.

The real-files test passes for removed directories, size-only config edits,
creation-root refresh, explicit clearing and missing config. Direct Python
get_all_skills_dirs calls confirm the same distinctions. Clippy passes with
warnings denied.
The existing profile_extra_dirs resolver is shared, avoiding duplicated path
normalization. No helper is active.

Next: connect SourceCache and captured path expansion to PromptLoader with
explicit config path, profile home, OS home, project start and launch cwd.
Resolve disabled names from the same config, then integrate into native prompt
initialization once its complete inputs and tool gating are available.

## Raw skill config cache checkpoint: 2026-09-07

`skill_discovery::RawConfigCache` reads explicit config paths with a single-entry
cache keyed by path, modification time and size. Successful mappings replace
the cached entry; missing, invalid UTF-8, malformed YAML and non-mapping reads
return an empty mapping without replacing the previous success. Explicit clear
is available. It reuses skill_yaml's PyYAML scalar handling, including yes/off.

The real-files test passes for same-time/same-size reuse, size invalidation at
an unchanged timestamp, explicit clearing and invalid-read retention. A direct
Python import probe confirms the cache contracts. Clippy passes with warnings
denied. Existing
skill_yaml JSON representation limits apply, and Rust returns owned values rather
than exposing Python's mutable cached mapping identity.

Next: external-directory cache keyed only by config path/mtime, keeping uncached
create_dir resolution separate. Then connect both caches and captured expansion
to the retained prompt loader. Startup integration remains outstanding; do not
count this helper as a live production path.

## Captured skill path expansion checkpoint: 2026-09-07

`skill_discovery::expand_source_path` expands variables from an explicit string
map, then tilde using the captured OS home. It does not read missing variables
from the process environment. Unknown variables and unknown named users remain
literal, matching the os.path helpers used by skills. The webhook POSIX variable
parser now accepts a lookup callback; its existing wrapper retains its original
process-environment behavior. The shared tilde helper also now handles HOME=/
correctly: ~/x becomes /x, without a doubled slash.

Python 3.12.13 confirms braced/unbraced names, unknown and empty variables,
nonrecursive replacement, unknown users and root-home expansion. Full workspace:
**1,480 passed, two ignored**. The profile-directory test additionally composes
the real captured expander with source resolution and passes. Clippy passes
with warnings denied.
The current expansion API covers Unicode strings; runtime OsString conversion
and non-Unicode environment/path behavior remain explicit integration gaps.

Next: owning config-file read/mtime cache semantics and a source-resolution
caller that passes captured launch cwd for relative trusted project paths,
profile home for relative external paths, and OS home for tilde expansion.
Then wire the retained PromptLoader into production prompt initialization.

## Profile skill source resolution checkpoint: 2026-09-07

`skill_discovery::profile_extra_dirs` resolves create_dir first, then external
roots in config order. Relative paths use the supplied profile home; resolved
local aliases and duplicates are excluded. Missing directories are omitted.
Non-string external entries preserve Python str() behavior. Creation-directory
ordinary resolution errors retain Python's raw-path fallback, while symlink
loops and external resolution errors propagate. Expansion is a captured caller
input, not an ambient environment lookup.

The real-files resolver test passes, including aliases, ordering, null entries,
missing directories and symlink-loop failure. A direct Python config/import
probe confirms ordering and deduplication. Clippy passes with warnings denied.
Config-file mtime caching, concrete captured expansion and production prompt
wiring remain pending. Gemini's source-resolution helper session 48705 finished
successfully; its advisory map is analysis/skill-source-resolution-next.md.
No helper remains active. The map predates profile_extra_dirs and contains a
suggested ambient-environment fallback that must not be adopted without evidence.

## Rendered skills prompt cache checkpoint: 2026-09-07

`skill_loader::PromptLoader` now owns a 32-entry LRU of rendered strings and
retains project admission decisions. Keys preserve project/external directory
order, skills path, sorted tools/toolsets, platform, disabled names and compact
categories. Like Python, absent and empty tool sets share a key; host/environment
are not key fields. Hits refresh recency without reading skill metadata. The
public no-source check precedes cache lookup. Explicit clearing optionally
removes the owning home's disk snapshot, leaving admission decisions intact.

All seven loader tests pass, including file edits preserving cached bytes,
recency eviction and snapshot clearing. Actual Python imports confirm the same
cache behavior. Clippy passes with warnings denied. The previous full workspace
checkpoint remains 1,477 passed, two ignored; this change has targeted validation.

Production must retain and synchronize one PromptLoader owner across builds;
this is not yet wired at startup. Next is captured config/source resolution:
create_dir precedes external_dirs, relative paths use the owning Hermes home,
resolved duplicates/local aliases are excluded, and config mtime caching must
preserve reference semantics. Gemini's bounded source-resolution review was
started through agy.sh with the single-flight auth lock. Session 48705 is still
running at this checkpoint; poll that handle before starting another helper.
Expected artifact: analysis/skill-source-resolution-next.md; log:
/tmp/hermes-next-skill-source.log.

## Combined skills loader checkpoint: 2026-09-07

`skill_loader::load_index` now assembles profile discovery, project admission,
profile merging, profile snapshot completion, and external loading in Python's
order. Sources are explicit paths from the owning profile/session. The caller
retains `ProjectAdmission`. External category descriptions now fill missing
categories while profile descriptions win; external metadata never enters the
profile snapshot. Category parsing is shared with the profile path.

The real-files combined test covers project-over-profile-over-external name
precedence, category-description precedence, snapshot isolation, and identical
cold/warm rendering. A matching scenario through actual Python
`_build_skills_system_prompt_inner` confirms those contracts. Full workspace:
**1,477 passed, two ignored**. Clippy passes with warnings denied.

Next: wrap the assembled loader with the reference's 32-entry rendered-prompt
LRU (including directory order, tools/toolsets, platform, disabled and compact
categories in its key), then connect captured config/source resolution and
production prompt assembly. These are still missing; load_index is a tested
assembly API, not yet a production startup path. Earlier checkpoints below
retain their historical validation totals and next-step notes.

## Resumed after user PC restart: 2026-09-07

The user requested continuation. The previous restart handoff is historical.
`skills_guard::check_structure` now covers file counts, size limits, suspicious
extensions, executable bits and symlink containment, with shared ignore rules.
All 59 captured Python structural cases and the Python 3.12 symlink-loop error
case pass using real temporary files. Tests remain inline with the implementation.
`scan_skill` now combines structure and text scanning with trust metadata and
UTC timestamps. Six complete Python scan fixtures cover clean, ignored,
protected SKILL.md, mixed findings, single-file and missing-path behavior.
All six bundle cases pass. These helpers are not yet wired into project
admission or production prompt assembly.

Canonical content hashing and `scan_skill_cached` are now implemented. Seven
actual Python hash cases cover byte preservation, case-sensitive ordering,
swapped contents, ignored files, Unicode names and symlinks. Inline cache tests
cover reuse, content/source/URL/version invalidation, malformed JSON, UTF-8
errors and failed writes. Direct Python filesystem probes confirm cache reuse,
ignored-content invalidation and UTF-8 error propagation. The targeted scanner
suite passes all 10 tests; current Clippy passes with warnings denied. Hash
fixture regeneration passes --check. Cache serialization is semantically JSON
compatible, not byte-identical to Python; malformed records with non-string
metadata remain stricter in Rust because ScanResult has typed string fields.

Project admission is now implemented as `skill_discovery::ProjectAdmission`:
resolved-directory decisions, explicit clearing, dangerous-only verdict gate,
fail-closed scanner errors, and the supplied Hermes home's
cache/project_skill_scans directory. `files` filters the shared index traversal.
`skill_loader::merge_project` consumes that gate, applies compatibility and
visibility, labels accepted entries, and returns names for profile shadowing.
Its caller must retain one admission instance across loads; production startup
and the full loader orchestrator are still pending.

All 14 targeted project tests pass. Direct Python imports with a temporary
HERMES_HOME confirm safe/caution admission, dangerous blocking, scanner-error
blocking, and reuse until explicit clearing. A looping symlink does not itself
contribute to the content hash, so an otherwise identical bundle may reuse an
existing attestation; this matches the reference and is not claimed fixed.
Next: assemble profile/project/external loading in reference order, retaining
the admission cache and captured environment through the owning loader.
Latest full workspace validation: **1,472 passed, two ignored**, with Clippy
passing warnings denied. Both structural and bundle generators pass --check.
Changes remain uncommitted.

Gemini reviewed the structural scanner through the existing single-flight auth
wrapper. Its recommendation to reuse the first stat was rejected: the Python
permission check performs a second uncaught stat, so Rust preserves that error
behavior. Root resolution likewise stays inside the symlink loop to preserve
reference timing. Known gaps remain Windows permission synthesis/path matching,
non-Unicode paths, and traversal behavior during concurrent filesystem changes.

## Current full-port estimate: 2026-09-07

**33% complete**, using the same scope weights as the previous 28% audit.
Gateway 50% x 35% + tool/RPC 8% x 30% + state/search 52% x 15% + native
core 28% x 20% = **33.3%**. These are engineering scope estimates, not test
or line coverage. See the [current audit](analysis/progress-audit-2026-09-07.md)
for evidence, limits and comparison. Older percentages below are historical.

Session routing/persistence and inbound audio now have substantial live wiring.
Most native tools/RPC remain missing, startup registers only CurrentTimeTool,
and the tested prompt/memory/skills pieces are not yet assembled into production
turns. Python/external CLI capabilities do not count toward native completion.
The unfinished skills disk snapshot load/write edits are excluded from this
estimate and still need validation. Last completed workspace run: 1,449 passed,
two ignored. Gemini's independent audit completed; no helper remains active.

## Skills metadata manifest checkpoint: 2026-09-07

File-scanner checkpoint: skills_guard::scan_file now compiles all **129 captured
Python threat patterns** and matches **221 actual Python file cases**. The
generator --check passes. Shared regex whitespace conversion is reused; finding
ordering, per-pattern/line deduplication, docstring suppression, Unicode display
truncation, extension filtering and malformed UTF-8 behavior pass the corpus.
Invisible findings use stable generated character order when several occur on
one line; Python's set order is process-randomized. Regex Unicode edge behavior,
Windows paths and resource-limit behavior need broader parity checks.
Gemini task 39870 completed; no helper remains active. Structural checks and
bundle hashing/attestation still need implementation before project admission.
Workspace: **1,469 passed, two ignored**, /tmp/hermes-skill-file-scanner-workspace.log.
Clippy with warnings denied passes, /tmp/hermes-skill-file-scanner-clippy.log.
Next helper: structural-oracle session 51095 is active through agy.sh, log
/tmp/hermes-skill-structure-helper.log. The no-helper statement above describes
completion of the file-oracle task, before starting this structural task.

Scanner ignore checkpoint: IgnoreRules reads both supported ignore files and
preserves SKILL.md, root anchoring, directory prefixes and per-segment fnmatch
behavior. All **676 actual Python wildcard cases and 14 real ignore-file
configurations** pass in the inline Rust oracle test; generator --check passes.
The matcher follows [Python fnmatch](https://docs.python.org/3.12/library/fnmatch.html),
where slash and leading dots are ordinary characters. It is currently POSIX;
Windows normcase and unusual absolute-path inputs need separate coverage.
Structural/text scan consumers remain pending. Gemini session 39870 is still
live; pattern and file fixtures have appeared but the helper is not yet terminal.

Scanner foundation checkpoint: skills_guard.rs now represents findings and
ports verdict aggregation, summary formatting, source trust boundaries and
docstring-line suppression. All three inline tests pass, with direct checks
against the actual Python helpers. Project-local source remains community;
this is not yet a bundle scanner or project admission gate. Regex file scanning,
ignore rules, structure checks and attestation hashing/cache remain pending.
Gemini scanner-oracle session 39870 is live (re-polled); log
/tmp/hermes-skill-guard-helper.log. Expected artifacts are skill-guard-patterns.json,
skill-guard-file-goldens.json and their generator under rust/tools.

Trusted candidate checkpoint: project_dirs applies the exact boolean-false
discovery gate, normalizes configured trust entries through caller-supplied
path expansion, compares resolved roots exactly, orders .hermes/skills before
.agents/skills, and excludes the owning profile's skills path. Real-file Rust
and actual Python comparisons pass for missing/parent trust, disabled vs zero,
symlink trust aliases and local-directory exclusion. Captured environment path
expansion still needs its production caller. Trust is not a scanner substitute.
Gemini project map session 8254 completed; see
analysis/project-skills-integration-map.md. No helper remains active.
Targeted check: `cargo test -p hermes-gateway project_candidates_require -- --nocapture`.

Merge oracle/project-root checkpoint: all **37 extracted-Python merge/render
cases** pass in Rust, and the generator --check passes. The fixture harness
provides preaccepted project/external entries; it does not verify trust,
quarantine or edge visibility. Gemini merge task 69637 completed.
project_root now resolves captured working paths, checks up to 64 ancestors,
accepts .git files/directories and excludes the OS user home checkout. Its
real-file test and direct Python checks pass, including nonexistent descendants.
Trust configuration, project candidate dirs and quarantine are still pending.
Gemini project mapping session 8254 is active; log
/tmp/hermes-project-skill-helper.log. Latest checks are targeted; the last full
suite remains the 1,461-test loader checkpoint below.

Profile completion/external checkpoint: finish_profile reads uncapped category
descriptions and writes only scanned profile metadata after merge; warm hits
restore category descriptions. merge_external scans read-only directories in
order, preserves already-claimed names and skips per-entry errors. The combined
real-file Rust test passes, and the actual Python inner index builder confirms
local-over-external precedence and profile-only snapshot content. The stages
are exercised together but still need project trust/discovery, owning config
resolution, LRU acquisition and production prompt lifecycle wiring.
Gemini merge-oracle session 69637 remains live after polling.
Workspace validation: **1,461 passed, two ignored**, at
/tmp/hermes-skill-loader-workspace.log. Clippy with warnings denied passes at
/tmp/hermes-skill-loader-clippy.log.

Merge checkpoint: skill_loader::merge_profile applies accepted project-name
overrides before personal/org collision analysis, then groups visible entries
with Python-exact author and collision labels. The focused inline test passes;
executing the actual Python AST merge block independently verifies both owners'
labels and the project override. Gemini merge-oracle session 69637 remains live.
Raw non-string names/categories and unlabelled non-string descriptions still
need renderer representation parity; the typed Index cannot preserve them yet.
Project discovery/acceptance, final snapshot writing and external merge consumers
remain to be connected. This checkpoint does not claim those stages are complete.

Profile ingestion checkpoint: skill_loader::load_profile connects owning-home
snapshot reads with filesystem scans, raw entry retention and visibility filters.
The real-file test passes for disabled/required-tool filtering and home isolation.
A direct run of Python's _build_skills_system_prompt_inner confirms that a
cold environment rejection can reappear on a warm snapshot with an empty
description: snapshot hits do not rerun environment checks. Preserve this
existing behavior until a deliberate upstream behavior change is requested.
Scanned entries are returned for the later category-description/snapshot write
stage, after project/org merging. That merge orchestrator remains unfinished.
Targeted test: `cargo test -p hermes-gateway profile_scan_and_snapshot -- --nocapture`.
Gemini merge-oracle session 69637 started through agy.sh; log
/tmp/hermes-skill-merge-helper.log. Poll it before starting replacement work.

File-scan checkpoint: parse_skill_file connects real file reads, universal newline
normalization, frontmatter parsing, platform filtering, lazy environment filtering
and description extraction. The inline real-file test and direct Python checks
pass for missing/invalid-UTF8 files, CR/CRLF, platform-before-environment order and
detector failure. Exceptions retain a visible empty-metadata entry as Python does.
The directory/snapshot/merge orchestrator is still pending; this is its per-file
consumer, not production prompt wiring. Targeted test:
`cargo test -p hermes-gateway skill_file_scan_orders -- --nocapture`.

YAML scalar checkpoint: all **108 actual Python frontmatter cases** pass and
gen_skill_frontmatter_goldens.py --check passes in the checkout venv. The
frontmatter path now uses skill_yaml.rs with yaml-rust2 0.11.1 parsing events,
preserving quote style before applying PyYAML boolean, integer and float rules.
This replaces the temporary serde visitor; ordinary config parsing is unchanged.
It also handles duplicate keys, aliases, merge precedence, unknown-tag fallback
and pre-content column-zero BOMs. Parser API reference:
[yaml-rust2 events](https://docs.rs/yaml-rust2/0.11.1/yaml_rust2/parser/enum.Event.html).

The oracle explicitly excludes timestamps and non-string mapping keys. Remaining
representation/parity work includes those cases, arbitrary-size integers,
non-finite JSON values, recursive aliases and other safe-loader constructors.
Do not count the skill loader as integrated yet. Workspace validation:
**1,457 passed, two ignored**, /tmp/hermes-frontmatter-workspace.log.
Clippy with warnings denied passes at /tmp/hermes-frontmatter-clippy.log.
Gemini session 63336 completed; no helper remains active.

Duplicate-key follow-up: frontmatter now uses a recursive mapping visitor to
retain the last typed value, including nested keys. Unknown custom YAML tags
trigger Python's literal key/value fallback. Direct Python comparisons and both
inline frontmatter tests pass. This closes those tested cases, not all YAML tag
or scalar parity. Frontmatter oracle session 63336 remains live after polling;
do not restart it merely because the redirected output is quiet.

Frontmatter checkpoint: parse_frontmatter has seven directly checked Python
cases and a passing inline Rust test for BOM, closing-fence whitespace, body
preservation, malformed YAML fallback and merge keys. It reuses serde_yaml_ng
0.10 and its [apply_merge API](https://docs.rs/serde_yaml_ng/0.10.0/serde_yaml_ng/enum.Value.html#method.apply_merge).
Scalar resolution, duplicate keys, tags, timestamps and non-string mapping keys
still require parity work before connecting the parser to production. Do not
claim full PyYAML compatibility from the passing fence tests.
Gemini loader-map session 22513 completed; see analysis/skills-loader-integration-map.md.
Its collision message paraphrase is not a prompt constant; Python's exact text
remains authoritative. Frontmatter oracle session 63336 is live, log
/tmp/hermes-skill-frontmatter-helper.log, running through the unchanged auth lock.

Oracle follow-up: Gemini session 18980 completed. All **100 actual Python entry
cases** now pass in the inline snapshot-entry test; the generator --check passes
in the checkout venv. The oracle caught a narrow description parameter: Python
preserves supplied raw description values, so snapshot_entry now accepts a JSON
value as well as strings. Fixtures: rust/tools/skill-entry-goldens.json and
gen_skill_entry_goldens.py. This was a targeted rerun after the prior full suite.
Loader-map session 22513 remains live and was polled again. Next: frontmatter
parsing and scan/merge consumer, preserving project precedence and org collisions.

Entry validation follow-up: inline root/category/nested/org cases match direct
Python output; provenance precedence/malformed-file and invalid-platform tests
also pass. Full workspace: **1,453 passed, two ignored** at
/tmp/hermes-skill-entry-workspace.log. Clippy with warnings denied passes at
/tmp/hermes-skill-entry-clippy.log. Broader Gemini fixtures remain pending;
sessions 18980 and 22513 were re-polled and remain live. The scan/merge consumer
is still unfinished. Project overrides must precede personal/org collision
labeling; external entries lose to names already visible, matching Python.

Loader follow-up: skill_discovery::snapshot_entry now constructs category/name,
raw frontmatter metadata, conditions and organization provenance. It compiles
but still awaits the actual-Python entry oracle before being considered verified.
skills_index::extract_description has seven direct Python comparisons and a
passing inline Rust test covering truthiness, quote stripping and Unicode length.
Gemini entry-oracle session 18980 is still running; loader-map session 22513 was
queued through the same agy.sh lock. Poll these handles before starting more
helpers. Logs: /tmp/hermes-snapshot-entry-helper.log and
/tmp/hermes-skills-loader-map.log. No new workspace-wide validation yet.

Follow-up: profile-scoped snapshot load/write methods now have passing real-file
tests for read-back, cross-profile isolation, metadata invalidation, malformed
JSON, invalid UTF-8 propagation, symlink preservation and best-effort failure.
`cargo test -p hermes-gateway skill_discovery::tests -- --nocapture` passes all
five discovery tests; formatting passes. This is targeted validation, not a new
workspace run. These methods still need the loader consumer. Full shared atomic
I/O parity remains open, particularly owner preservation, Windows retries and
EXDEV/EBUSY fallback; symlink targets with missing parents also need comparison.
Do not interpret this checkpoint as completion of the skills loading pipeline.

skill_discovery::manifest now shares a single walk with discovery and captures
SKILL.md/DESCRIPTION.md size and nanosecond mtime plus the active-org marker's
size and integer-second mtime. The marker preserves Python float-to-int
truncation, including pre-epoch values. Broken links can be discovered but are
omitted from the manifest when stat fails. Directory enumeration errors discard
that directory's partial listing, matching os.walk.

Gemini task 20712 completed. All **32 actual Python filesystem cases** pass in
Rust with real files and symlinks; the generator --check passes in the checkout
venv. The other discovery and manifest tests pass, including direct Python
timestamp/org/broken-link comparisons. Log:
`/tmp/hermes-skill-walk-oracle-tests.log`. Workspace tests: **1,449 passed,
two ignored** (`/tmp/hermes-skill-manifest-workspace.log`). No helper remains active.
Next: load/write profile-scoped disk snapshots and parse skill entries, then
merge project/profile/org/external sources and add the in-memory LRU. Snapshot
validity must use the manifest, not a new content hash or timestamp convention.
Non-Unicode paths and filesystem races still need broader parity coverage.

## Skills filesystem discovery checkpoint: 2026-09-07

skill_discovery::index_files follows the Python index walker: fixed metadata/
dependency exclusions, support-directory pruning only beneath a SKILL.md root,
sorted results, directory symlink following and active-org mirror selection.
The active marker uses Python whitespace trimming; filesystem read failures
mean no active org, while invalid UTF-8 propagates as in Python. Real filesystem
tests and direct actual-Python comparisons pass for pruning and org switching.
Test log: `/tmp/hermes-skill-discovery-tests.log`.

Gemini task 20712 completed broader filesystem fixtures at
rust/tools/gen_skill_walk_goldens.py and skill-walk-goldens.json; log
`/tmp/hermes-skill-walk.log`. They are integrated in the checkpoint above.
This walker is not yet the complete loader:
frontmatter/snapshot entries, metadata manifests, merging and cache acquisition
remain pending. Non-Unicode filename ordering and mid-enumeration filesystem
errors still need parity review. Symlink traversal deliberately follows Python;
no independent recursion or canonical-path deduplication rule was introduced.

## Python literal numeric parity checkpoint: 2026-09-07

Literal equality now follows Python across bool, arbitrary-size integer, float,
complex and tuple keys. Dictionary replacement preserves the first key object
and overwrites its value. Integral floats are compared as exact integers rather
than rounding large integer keys through f64. Set deduplication shares the same
equality. Complex representations preserve negative zero, scientific notation
and infinity spelling.

All **784 actual Python literal-key cases** pass, including large adjacent
integers, mixed numeric keys, tuple keys and complex formatting. Generator:
gen_literal_key_goldens.py --check (Python 3.12.13). Test log:
`/tmp/hermes-literal-key-tests.log`. Workspace tests: **1,446 passed, two
ignored** (`/tmp/hermes-literal-parity-workspace.log`). Remaining gaps include set display order,
surrogate escapes, overflowing mixed complex arithmetic and interpreter-specific
syntax/resource limits. The parser remains literal-only; no code is executed.

## Disabled skill configuration checkpoint: 2026-09-07

skills_index now normalizes raw disabled-name configuration, merges global and
exact-platform exclusions, applies explicit/env/session platform precedence,
and protects the exact essential name hermes-agent. Python-literal array strings
use a native literal-only evaluator over rustpython-parser/ast 0.4.0; JSON-only
parsing would incorrectly accept lowercase true/null and reject single quotes,
adjacent strings, comments and trailing commas. The evaluator never runs calls
other than recognizing the empty-set literal form. Parser reference:
[RustPython parser API](https://docs.rs/rustpython-parser/0.4.0/rustpython_parser/).

Workspace tests passed before the broader oracle test: **1,444 passed, two
ignored** (`/tmp/hermes-disabled-skills-workspace.log`). Gemini task 91614
completed 110 actual-Python config cases; all pass the inline Rust oracle test,
and its generator --check passes in the checkout venv. Oracle test log:
`/tmp/hermes-disabled-skills-oracle-tests.log`.

Full CPython literal parity remains unfinished: set representation order,
surrogate escapes and interpreter-specific syntax/resource limits need more
work. The Rust evaluator currently keeps set insertion order. Numeric key
equality and complex representations were improved in the checkpoint above.
Do not claim full ast.literal_eval parity from
the disabled-list examples. Raw config loading and skills filesystem discovery
remain pending. No helper task is running.

## Skills OS and environment filters checkpoint: 2026-09-07

skills_index now implements OS-tag matching, macOS/Windows aliases, Termux
compatibility and environment offer filtering. Unknown environment tags remain
visible; blank tags and lazy detector calls follow Python exactly. Explicit
skill loading must bypass the environment relevance filter. Host identity and
environment detection remain captured caller inputs, not process-global state.

All **736 actual Python offer-filter cases** pass, including detector call
order. All four skills_index tests pass (`/tmp/hermes-skill-offer-tests.log`),
and gen_skill_offer_goldens.py --check reproduces the fixtures on Python 3.12.13.
The filesystem loader and environment detection/cache adapter remain pending.

Disabled-list parsing needs Python literal-list support in addition to JSON
strings; do not replace ast.literal_eval with JSON-only parsing. Gemini task
91614 is generating actual disabled-name cases in
rust/tools/gen_disabled_skills_goldens.py and disabled-skills-goldens.json;
log `/tmp/hermes-disabled-skills-oracle.log`. Last poll confirmed it is running.
Review those cases next, then complete disabled configuration and file loading.

## Skills activation conditions checkpoint: 2026-09-07

skills_index now extracts nested metadata.hermes conditions and evaluates
session-platform, fallback-tool/toolset and required-tool/toolset gates in
Python order. Unknown tool-filter information is distinct from an explicitly
empty tool surface. Raw malformed condition values preserve Python iteration,
unhashable-member failures and short-circuit outcomes rather than silently
becoming empty lists. Parent metadata that is not an object remains tolerated.

All **2,160 actual Python condition cases** pass in the inline Rust test, along
with the two existing skills-index tests. gen_skill_conditions_goldens.py
--check reproduces the fixtures using Python 3.12.13. Test log:
`/tmp/hermes-skill-conditions-tests.log`. These gates still need the filesystem
loader consumer. Next: disabled-name normalization (including essential-skill
immunity), platform/environment gates and snapshot entries, then source merging
and cache behavior described in rust/analysis/skills-prompt-map.md.

## Skills index rendering checkpoint: 2026-09-07

Workspace tests: **1,440 passed, two ignored**
(`/tmp/hermes-skills-index-workspace.log`). skills_index renders resolved
category entries with exact Python guidance, sorted categories, stable
first-description deduplication, names-only category demotion and tool-aware
basic-tool wording. Twenty-one oracle cases execute the actual Python rendering
block; gen_skills_index_goldens.py --check reproduces both fixtures and template.
ResolvedPromptSections::set_skills_index gates the block on skills_list,
skill_view or skill_manage and uses that same tool surface for wording.

The renderer does not yet load files or apply conditions/disabled filters,
project precedence, org labels/collisions, external-directory precedence or
the two cache layers. The help-guidance variant also needs the completed index
before stable guidance is finalized. Gemini task 45878 completed the skills
loader map at rust/analysis/skills-prompt-map.md. Review the remaining sections
against Python before implementing the loader. Final workspace Clippy passed
(`/tmp/hermes-skills-index-clippy.log`). No helper task remains active.

## Plugin session metadata checkpoint: 2026-09-07

Workspace tests: **1,438 passed, two ignored**
(`/tmp/hermes-plugin-metadata-tests.log`). plugin_prompt::session_info captures
the six callback fields with Python scalar coercion, owning-home profile
identity and context-cwd resolution. Invalid session cwd does not fall back to
the terminal or launch cwd. Explicit agent homes use _profile_name_for_home
semantics: the first component beneath root/profiles wins, including nested or
mixed-case paths; unrelated homes become default. Prompt profile guidance now
uses this same rule instead of the distinct ambient CLI profile-name inference.
Actual Python metadata/identity comparisons passed.

The metadata helper accepts an already-resolved agent home; bound override,
session-DB fallback and unavailable ambient-profile capture still belong to the
production agent construction work. Non-Unicode path parity remains incomplete.
Gemini task 45878 is mapping the skills-index loader to
rust/analysis/skills-prompt-map.md; log `/tmp/hermes-skills-prompt-map.log`.
Poll that task before replacing it. Next: review the skills map and port the
owning-home skills index, then finish external-memory prompt wiring and the
production prompt lifecycle.

## Plugin section ownership checkpoint: 2026-09-07

plugin_prompt::Registry owns validated IDs, position and per-section limits,
text/callback content and plugin ownership. Duplicate IDs are rejected before
mutation. Identity-bearing handles make repeated disposal harmless and prevent
stale handles from removing same-ID replacements. Owner unload leaves other
plugins intact. Registry rendering uses the existing ordering and budget rules;
ResolvedPromptSections::load_plugin_sections connects it to the agent snapshot
and after-memory assembly. Frozen sessions retain their bytes after unload.

Actual Python registration checks passed for invalid/valid ID boundaries,
positions/limits, duplicates, callbacks and stale disposal. Workspace tests
passed before the assembly convenience method: **1,437 passed, two ignored**
(`/tmp/hermes-plugin-registration-workspace.log`). Focused registration tests
are recorded in `/tmp/hermes-plugin-registration-tests.log`.
Gemini review task 24450 completed at
rust/analysis/plugin-registration-review.md. Its Rust-status description was
captured before this turn's registry edits; its Python source mapping remains
useful for the broader lifecycle work. Six focused tests and final workspace
Clippy pass (`/tmp/hermes-plugin-registration-clippy.log`).
This is a typed Rust registry, not the complete plugin manager: raw RPC input
validation, discovery, cross-capability reverse-order unload, callback transport,
and production per-profile registry/per-agent snapshot ownership remain pending.

## Frozen plugin prompt checkpoint: 2026-09-07

plugin_prompt now formats and restores canonical after-memory sections using
Python character lengths, last-start framing, the required conversation footer
and exact whole-container reconstruction. ResolvedPromptSections can restore
the block in its original volatile-tail position. Snapshot freezes a render,
restores persisted prompts without invoking callbacks, and retains the previous
snapshot when rendering fails after explicit invalidation. An empty successful
render replaces the previous snapshot. Actual Python lifecycle checks passed.

Workspace tests passed with the initial parser: **1,432 passed, two ignored**
(`/tmp/hermes-plugin-workspace.log`). The subsequently added lifecycle test and
parser/assembly test both pass (`/tmp/hermes-plugin-lifecycle-tests.log`).
Gemini task 99321 completed. All 39 actual-Python framing fixtures in
rust/tools/gen_plugin_prompt_goldens.py and plugin-prompt-goldens.json now pass
the inline Rust oracle test; the generator's --check passes in the checkout
venv. All four plugin_prompt tests pass (`/tmp/hermes-plugin-oracle-tests.log`).
Rendering from validated registrations now preserves sorted IDs, lazy callback
evaluation, the 32-section cap, individual limits, reserved-marker rejection,
Python whitespace trimming and the 8,000-character budget including framing.
Direct Python checks cover failures, non-string output, Unicode, callback count,
and exact 8,000/8,001-character boundaries. Registration validation/ownership,
live plugin execution and production agent snapshot ownership remain unfinished.

## Composed toolset resolver checkpoint: 2026-09-07

Workspace tests: **1,429 passed, two ignored**. toolset_resolution resolves
captured static definitions, registry overlays/aliases, cycle/diamond includes,
all-tool aliases and plugin-platform fallbacks. Static-only mode excludes
registry-derived tools and aliases. The external-memory enabled policy consumes
this resolution, preserving explicit disabled-memory precedence and the
None-versus-empty enabled-list distinction. Direct actual Python checks passed
the representative composition/alias/platform and memory-policy cases.
Log: `/tmp/hermes-toolset-resolution-tests.log`.

Gemini task 39648 completed. Its 25 registry-aware fixtures at
rust/tools/gen_toolset_resolution_goldens.py and toolset-resolution-goldens.json
reproduce actual Python output and pass the inline Rust oracle test. All three
toolset_resolution tests pass. The schema exposure helper additionally preserves
Python's early memory match and malformed-function error ordering; four direct
Python checks confirm those cases. The resolver
uses typed captured definitions, not the live registry; static catalog loading,
snapshot acquisition, registry failure behavior, malformed definition parity
remain pending. Provider execution and prompt lifecycle wiring remain pending.
Use the shared schema exposure check when wiring
the external-memory prompt block or changing native tool selection.

## Memory snapshot parity checkpoint: 2026-09-07

Workspace tests: **1,428 passed, two ignored**. All 23 Gemini-generated cases
match actual Python MemoryStore loading and formatting with real files:
absent/empty inputs, BOM/CRLF, exact delimiters, duplicates, threat placeholders,
preblocked markers, Unicode counts, zero/negative/over-limit budgets and comma
formatting. The Rust test also checks all four memory/user enablement
combinations for every snapshot. Generator:
`rust/tools/gen_memory_snapshot_goldens.py --check` (checkout venv).
Test log: `/tmp/hermes-memory-oracle-tests.log`.
Gemini task 86462 completed successfully; no helper is running.

Next: external-memory tool exposure and plugin/skills volatile sections.
agent/memory_manager.py::memory_provider_tools_exposed shares its decision
with tool injection: disabled memory wins, an exposed built-in memory schema
enables providers, None enabled-toolsets preserves defaults, and composed
toolsets require actual registry-aware resolution. The Rust toolset resolver
is not yet present; do not replace this gate with a loaded-provider check.
Production prompt lifecycle and live memory writes remain unfinished.

## Frozen memory snapshot checkpoint: 2026-09-07

Workspace tests: **1,427 passed, two ignored**. memory_snapshot loads the owning
home's MEMORY.md/USER.md, strips UTF-8 BOM, normalizes newlines, parses exact
entry delimiters, deduplicates in order and sanitizes strict threat matches
only in the frozen prompt copy. Original files remain unchanged. Character
usage formatting preserves over-limit content and configured integer limits.
ResolvedPromptSections consumes the snapshot under separate memory/user gates.
Real-file checks and actual Python comparisons confirm decoding, deduplication
and immutable snapshots after external writes. Test log:
`/tmp/hermes-memory-snapshot-tests.log`.

Gemini task 86462 is producing broader actual-Python fixtures in
rust/tools/gen_memory_snapshot_goldens.py and memory-snapshot-goldens.json;
log `/tmp/hermes-memory-oracle.log`. Poll before replacing it. Review and wire
those fixtures next. Live memory write-tool behavior is not implemented by
this read-only snapshot module. Skills/external-memory/plugin volatile loaders
and native prompt lifecycle integration remain unfinished.

## Scoped runtime clock checkpoint: 2026-09-07

Workspace tests: **1,426 passed, two ignored**. runtime_clock adds a source-keyed
timezone cache and clock snapshot consumed by Footer. Environment names win
over config, including invalid names; config-path identities keep profiles
isolated until reset. Capture supplies one instant for footer labels, dates and
lineage conversion. IANA zones preserve historical DST for the birth date;
Unix local fallback uses the OS timezone abbreviation.
Tests verify cache isolation/reset and winter-to-summer date conversion;
actual Python hermes_time/ZoneInfo checks passed the same scenarios.
Log: `/tmp/hermes-runtime-clock-tests.log`.

Added chrono-tz 0.10.4 for IANA conversion (see its official
[timezone API](https://docs.rs/chrono-tz/0.10.4/chrono_tz/enum.Tz.html)). Its bundled
TZDB can differ from Python's host TZDB; broad historical/future zone parity
still requires validation. Non-Unix local abbreviations remain a gap.
Canonical raw config/managed-overlay capture and live native prompt lifecycle
consumers remain unfinished. No helpers running.

## Database-to-footer verification checkpoint: 2026-09-07

Workspace tests: **1,424 passed, two ignored**. A real SQLite integration
test now creates an original/compacted session chain, resolves the original
date through Footer::resolve_start_date, persists the prompt, reopens the
database and rebuilds a later candidate. The original conversation date stays
anchored across a UTC-to-UTC+08 date crossing. Candidate construction does not
rewrite the stored prompt bytes; replacement remains an explicit lifecycle
action. Log: `/tmp/hermes-lineage-footer-tests.log`.

Next: timezone capture from hermes_time.py. Its cache identity is either the
trimmed environment override or the profile config path; cache entries include
invalid-zone results until reset. Raw config receives the managed overlay,
and invalid names fall back to server-local time. Do not use a process-global
unkeyed timezone cache. Then continue volatile loaders and live restoration.
No helpers running.

## Session-start date resolution checkpoint: 2026-09-07

Workspace tests: **1,423 passed, two ignored**. Footer::resolve_start_date
consumes SessionDb conversation-root lookup, then falls through segment ID,
creation stamp and build date. Naive stamps receive the captured machine
offset before display-zone conversion; aware creation stamps keep their own
offset. Embedded ID parsing follows Python's field-specific Unicode digit
rules. Empty current IDs suppress lineage-root selection, as Python does.
The actual Python resolver supplies 180 cases for precedence, malformed IDs,
Unicode, naive/aware creation and timezone date crossings. Its clock lookup
is replaced with an explicit machine-offset input in the oracle.
Generator: `rust/tools/gen_prompt_footer_goldens.py --check`.
Test log: `/tmp/hermes-session-start-tests.log`.

Remaining: timezone configuration/capture and runtime lifecycle consumers,
end-to-end database-to-footer testing, out-of-range datetime error parity,
volatile loaders and production restoration. No helpers running.

## Conversation lineage lookup checkpoint: 2026-09-07

Workspace tests: **1,422 passed, two ignored**. SessionDb now exposes
session_lineage_root_to_tip and get_conversation_root using Python's bounded
parent walk. It includes delegation parents, preserves dangling-parent IDs,
stops cycles before revisiting an ID and limits traversal to 100 entries.
Real SQLite tests cover missing/empty IDs, ordinary chains, cycles, dangling
parents and over-limit chains. The actual Python methods executed against
SQLite pass the same cases. Log: `/tmp/hermes-lineage-tests.log`.

Next: consume the root ID in session-start timestamp resolution, including
machine-local to display-zone conversion, then connect footer inputs and live
prompt restoration. This lookup is not the compression-only peer lineage
query and must not inherit that query's source/branch filters. No helpers running.

## Prompt footer renderer checkpoint: 2026-09-07

Workspace tests: **1,421 passed, two ignored**. prompt_footer renders captured
display-zone dates, zone labels/offsets and optional session/model/provider/
platform metadata. Cross-day builds retain the original start date and add
the rebuild date; timeless Bot Chat keeps timezone and identity without dates.
ResolvedPromptSections::set_footer connects it to volatile-tier assembly.
The actual Python footer block supplies 120 cases covering the date, timezone,
timeless and identity gates, including raw leading-newline behavior before
the final tier trim. Generator: `rust/tools/gen_prompt_footer_goldens.py --check`.
Test log: `/tmp/hermes-prompt-footer-tests.log`.

Next: resolve conversation start from lineage/session timestamps and capture
configured timezone input, then finish volatile loaders and production prompt
restoration. Footer dates are explicit resolved inputs today. No helpers running.

## Bot Chat epoch assembly checkpoint: 2026-09-07

Workspace tests: **1,420 passed, two ignored**. The fingerprint matches all
22 independently generated Python cases, covering real profile files, skills,
roles, peers, remote roster, malformed config, Unicode and float MCP values.
Generator: `rust/tools/gen_bot_fingerprint_goldens.py --check` (checkout venv).
Gemini task 61247 completed; no helper is running.

ResolvedPromptSections::append_bot_chat now gates cached protocol and epoch
insertion on the enabled flag and exact canonical title. A nonempty hint wins
over stored title; whitespace-only hints fall back. The method returns whether
footer construction should omit dates. Legacy SOUL suppression leaves that
flag false. Real-file integration tests and five executions of the actual
Python title-gate block passed. Test log:
`/tmp/hermes-bot-epoch-integration-tests.log`.

Remaining: live session title/config acquisition, protocol refresh during
restoration, volatile/footer construction and native lifecycle wiring. YAML,
filesystem and non-UTF-8 parity limitations recorded below still apply.

## Capability fingerprint implementation checkpoint: 2026-09-07

Workspace tests: **1,418 passed, two ignored**. capability_fingerprint now
captures config capabilities, SOUL hash, installed skill names, managed roster,
roles, peers and normalized remote roster titles. Config is an explicit
canonical-loader snapshot; None represents loader failure. Partial config
fields survive later conversion failures, matching Python's assignment order.
Hashing uses sorted JSON with Python spacing, ASCII escaping and float text.
Initial tests check the actual Python empty-home digest and invariants for
SOUL/skill changes and unchanged skill-body content. Log:
`/tmp/hermes-fingerprint-tests.log`.

Broader parity validation is pending. Gemini task 61247 is generating actual
Python fingerprint fixtures in rust/tools/gen_bot_fingerprint_goldens.py and
bot-fingerprint-goldens.json; log `/tmp/hermes-fingerprint-oracle.log`. Poll
that session before replacing it. Review its files and add the Rust comparison
test next. Canonical config loading, filesystem error/non-UTF-8 edge cases,
YAML parity and production session integration remain unfinished.

## Bot Mode protocol cache checkpoint: 2026-09-07

Workspace tests: **1,417 passed, two ignored**. ProtocolCache serializes
discovery and caches by the supplied home's exact OS-string spelling,
including empty results, until forced refresh. Its legacy-upgrade check
suppresses rebuilds when any epoch prefix or protocol heading already exists.
Epoch comparison uses the first twelve-character lowercase-hex stamp;
unavailable or failed fingerprint inputs cannot invalidate a stored prompt.
Forty-five actual Python epoch cases and real-file cache/upgrade comparisons
passed. Test log: `/tmp/hermes-bot-cache-tests.log`.

The fingerprint itself still needs its disk/config implementation; epoch
comparison currently accepts that resolved result. Production must share one
cache and apply the upgrade check only to canonical Bot Chat sessions.
Live restore/build wiring, capability hashing and YAML parity remain pending.
No helpers running.

## Bot Mode remote discovery checkpoint: 2026-09-07

Workspace tests: **1,415 passed, two ignored**. Bot protocol construction now
reads bot_relay/roster.json, normalizes remote rows, qualifies duplicate handles
by connection ID and renders remote teammate descriptions. Peer names are read
from root config.yaml and sorted. build_live_section connects these file reads
to the local roster/protocol renderer. Invalid UTF-8 remote JSON returns no
roster. Reading preserves row order and duplicates, matching the Python reader.
Five actual Python fixtures cover invalid rows, missing handles, boolean
liveness, duplicate handles, typed fields and Unicode truncation through full
protocol rendering. Existing sixteen protocol fixtures still pass.
Generator: `rust/tools/gen_bot_protocol_template.py --check` (checkout venv).
Test log: `/tmp/hermes-bot-remote-tests.log`.

Remaining: protocol cache, capability fingerprint and live prompt/session
gates. YAML non-string peer keys and PyYAML scalar differences remain part of
the metadata parity gap recorded below. No helpers running.

## Bot Mode protocol renderer checkpoint: 2026-09-07

Workspace tests: **1,414 passed, two ignored**. bot_mode::build_section now
joins the Python protocol text with the real profile roster, suppresses it
for unmanaged installs or legacy SOUL protocol, and appends explicit remote
text and peer names in reference order. Text is extracted from the actual
Python renderer, not rewritten. Sixteen generated cases execute Python with
real temporary profile metadata and controlled remote/peer inputs; Rust
recreates those layouts and compares full output.
Generator: `rust/tools/gen_bot_protocol_template.py --check` (checkout venv).
Test log: `/tmp/hermes-bot-protocol-tests.log`.

Remote roster discovery/formatting, peer config reads, protocol caching,
capability fingerprints and live prompt gates remain pending. The renderer
accepts resolved remote/peer data; this is not complete Bot Mode integration.
No helpers running.

## Bot Mode roster checkpoint: 2026-09-07

Workspace tests: **1,413 passed, two ignored**. bot_mode ports profile/root
mapping, ordered roster discovery, managed-install detection, legacy SOUL
protocol detection, handles and bounded role lines. Unmanaged and hidden
profile directories remain teammates; ordinary files do not. A legacy SOUL
protocol does not disable managed-install detection. Inline tests exercise
real profile files, metadata corruption and default/worker roster output;
the same scenarios passed against actual Python with the checkout's PyYAML.
Test log: `/tmp/hermes-bot-roster-tests.log`.

Gemini mapping task 87976 completed successfully. Its full source map is at
`/tmp/hermes-bot-mode-map.md`; no helper is running. Next: protocol rendering,
peer/remote roster inputs, capability fingerprints and prompt restore gates.
Metadata currently uses serde_yaml_ng decoded into JSON values; PyYAML 1.1
scalar quirks and non-string YAML mapping keys still need parity work. The
roster helpers are not yet connected to live native prompt construction.

## Profile/platform assembly checkpoint: 2026-09-07

Workspace tests: **1,412 passed, two ignored**. Post-workspace assembly now
connects explicit home/root profile inference and platform hint rendering,
after any preceding probe/protocol sections. Profile inference failures use
Python's default fallback; the active home remains distinct from the default
root. Inline tests cover default/red/blue profiles with and without a workspace
snapshot, checking placement and paths. The actual Python profile assembly
block also passed the three profile cases. Test log:
`/tmp/hermes-profile-platform-tests.log`.

Gemini task 87976 is mapping Bot Chat protocol and capability epoch behavior;
output `/tmp/hermes-bot-mode-map.md`, process log `/tmp/hermes-bot-mode-map.log`.
Poll the existing session before starting a replacement. Remaining work:
actual environment probe and Bot Chat loaders, volatile sections, captured
runtime inputs and production restore/build lifecycle. This checkpoint does
not make the native gateway's complete prompt construction live.

## Runtime guidance assembly checkpoint: 2026-09-07

Workspace tests: **1,411 passed, two ignored**. ResolvedPromptSections now
appends provider identity and rendered environment hints, then resolves coding
posture from scoped cwd and captures its three blocks. Coding is gated on a
nonempty toolset. Resolution/probe errors discard the coding blocks without
preventing prompt assembly, matching Python's exception boundary. Environment
probe results remain explicit session-owned inputs.
Tests run real workspace discovery, malformed config and tool-gate cases,
including post-workspace tier placement. Four cases executed the actual
Python provider/environment/coding selection block with controlled coding
results and errors. Test log: `/tmp/hermes-runtime-guidance-tests.log`.

No helpers running. Next: remaining post-workspace/volatile sections, input
capture, and the production restore/build lifecycle. These assembly methods
run at prompt construction only; they must never be called per model request.
Full native prompt parity remains incomplete.

## Stable identity initialization checkpoint: 2026-09-07

Workspace tests: **1,410 passed, two ignored**. ResolvedPromptSections now
initializes stable guidance using SOUL from the explicit owning profile home.
It implements load_soul_identity OR not skip_context_files and returns the
loaded flag for context-file deduplication, plus per-build warnings. Missing
or empty SOUL uses the default identity. This preserves cron's ability to
retain persona while skipping project instructions.
Real profile-file tests cover red/blue/empty identities and all load/skip flag
combinations through final context assembly. Twelve matching cases executed
the actual Python identity-selection block. Log:
`/tmp/hermes-stable-identity-tests.log`.

Gemini mapping task 51020 completed successfully; output is
`/tmp/hermes-prompt-next-map.txt`. It confirms these identity gates and identifies
provider identity/environment ordering as the next integration. No helper is
running. Clippy with warnings denied, formatting and diff checks passed.
Next: join remaining
prompt sections and wire the production restore/build lifecycle. Runtime
context-limit resolution and automatic profile-home initialization must be
connected at the caller; no live native prompt parity claim yet.

## Prompt context policy checkpoint: 2026-09-07

Workspace tests: **1,409 passed, two ignored**. ResolvedPromptSections now
loads its context-file slot through scoped cwd selection and Python's surface
policy. Desktop launch artifacts are treated as fallback paths, explicit
workspaces remain valid, and only exact cli/tui platforms allow install-tree
fallback. Skip-context bypasses scope resolution and clears the slot.
The existing context loader still owns file precedence, read limits and SOUL.
Inline tests exercise real files and final tier assembly across 32 combinations;
the actual Python selection block executed through AST matches those cases.
Log: `/tmp/hermes-context-policy-tests.log`.

Gemini task 51020 is mapping the remaining identity/stable-guidance loading
rules read-only; output `/tmp/hermes-prompt-next-map.txt`. Poll that session
before replacing it. Next: identity and stable-guidance initialization, then
remaining prompt sections and live native lifecycle integration. This is
prompt assembly integration, not yet a production gateway call path.

## Scoped cwd integration checkpoint: 2026-09-07

runtime_cwd::CwdInputs ports agent/runtime_cwd.py's distinct agent/context
fallback chains from explicit session, terminal, launch and home inputs.
An invalid session override falls through to terminal cwd for agent execution
but suppresses terminal fallback for context discovery. Explicit coding cwd
preserves whitespace and bypasses directory validation; missing coding cwd
uses the runtime chain, with launch fallback on resolver failure. Home
expansion reuses the existing file-read policy helper, including named-user
lookup. RuntimeMode::from_scope now consumes these inputs before discovery.

Actual Python checks confirmed invalid-session fallback, context suppression,
tilde expansion and explicit whitespace behavior. Inline Rust tests additionally
exercise relative paths and scoped cwd through coding profile detection.
Workspace tests: **1,408 passed, two ignored**.
Test log: `/tmp/hermes-runtime-cwd-tests.log`.
Live terminal scope acquisition/refusal, capturing temp roots and full prompt
lifecycle wiring remain pending. Existing non-UTF-8 and Windows expansion
limitations in the shared path helper remain to be resolved.

## Runtime posture integration checkpoint: 2026-09-07

Workspace tests: **1,407 passed, two ignored**. coding_context::RuntimeMode
now resolves settings, workspace detection, profile and model once, with
private fields and read-only accessors. It connects focus toolset proposals
and the coding prompt renderer to the real workspace snapshot. Inline tests
verify that config changes affect a fresh mode but leave the existing mode's
instructions and posture intact. Equivalent checks passed against Python's
actual resolve_runtime_mode and system_prompt_parts functions.
Test log: `/tmp/hermes-runtime-posture-tests.log`.

Gemini review session 88322 completed successfully. Its report is at
`/tmp/hermes-workspace-review.txt`; no helper is currently running. Caller
input resolution remains incomplete: expanduser, absent cwd fallback, and
capturing home/temp roots must be connected before production use. The
report's general claim that Python snapshot errors are suppressed was not
accepted: malformed porcelain and truthy non-iterable package scripts raise
in the reference. Rust's shared path resolver already handles missing tails.
Non-UTF-8 path parity and exceptional resolution paths remain unfinished.

Next: finish caller input resolution and connect the full prompt builder and
session lifecycle. RuntimeMode is a building block, not yet a live gateway
consumer; system_prompt_parts must only run when constructing a prompt.

## Workspace snapshot integration checkpoint: 2026-09-07

Workspace tests: **1,406 passed, two ignored**. coding_context now joins
workspace discovery, bounded Git probes, status parsing and project facts
into the prompt snapshot. Marker discovery is shared with coding detection.
Real filesystem tests cover marker-only projects, unborn branches, linked
detached worktrees, recent commits and dirty files. The same scenarios were
executed against Python's actual build_coding_workspace_block, with matching
expected text. Linked worktrees expose the shared-state fact without adding
the primary checkout path to the prompt.

Validation log: `/tmp/hermes-workspace-snapshot-workspace.log`.
Clippy with warnings denied, formatting and diff checks also passed;
Clippy log: `/tmp/hermes-workspace-snapshot-clippy.log`.
Gemini read-only review is running under exec session 88322, with output at
`/tmp/hermes-workspace-review.txt`. Poll that session before replacing it.
Full native prompt construction and immutable runtime posture wiring remain
next, along with the Git cleanup limitations recorded below. This snapshot
must be captured at prompt construction, never refreshed on every turn.

## Project facts integration checkpoint: 2026-09-07

Workspace tests: **1,405 passed, two ignored**. Gemini's coding_project_facts
module is reviewed and registered. It detects ordered manifests, package
managers, verification commands and context filenames, with file-size checks
and replacement UTF-8/newline decoding. Review preserved Python errors for
truthy non-iterable package scripts and added Python's control whitespace to
Makefile target matching. The root-only gateway formatter is named facts_for_root
so it does not imply that it performs workspace discovery.
The oracle checks 28 real temporary-project layouts and nine read cases;
extra inline cases for scripts errors/control whitespace were confirmed with
the actual Python detector. Generator:
`rust/tools/gen_coding_project_facts_goldens.py --check`.
Logs: `/tmp/hermes-coding-facts-workspace.log`,
`/tmp/hermes-coding-facts-clippy.log`.

Helper 31333 completed; none running. Next: join Git probes, parsed status and
project facts into the actual coding workspace snapshot, then wire runtime
posture and prompt construction. Read-small currently also rejects files that
grow past the cap after stat; Python checks only the initial stat. Full native
prompt lifecycle and remaining Git cleanup parity are still unfinished.

## Bounded Git probe checkpoint: 2026-09-07

Workspace tests: **1,400 passed, two ignored**. git_probe runs argv-based Git
commands with an explicit environment snapshot, Python's inert config overrides,
null stdin and bounded execution/post-kill drain. POSIX probes have their own
process group; Windows uses bounded taskkill /T /F. Output follows replacement
UTF-8 decoding, universal newline conversion and Python trimming; failures
return empty text. Real Git tests verify status, nonzero failure and suppression
of a repository fsmonitor marker script. A shell/descendant test proves an
inherited stdout pipe cannot hold the probe open after its deadline.
Logs: `/tmp/hermes-git-probe-workspace.log`,
`/tmp/hermes-git-probe-clippy.log`.

Remaining cleanup parity: Python's deadline helper additionally sweeps escaped
descendants (setsid); this probe currently kills its owned process group.
Windows behavior needs platform validation. Caller cancellation also needs
tree-wide cleanup, beyond the direct-child kill-on-drop. Next: workspace
snapshot rendering and wiring the probes to it. Gemini project-facts task
31333 remains live as last polled; full native prompt lifecycle is unfinished.

## Workspace status parser checkpoint: 2026-09-07

Workspace tests: **1,398 passed, two ignored**. coding_context::parse_status
ports Python's porcelain-v2 branch/count parsing, including preserved branch
suffix bytes, Unicode line splitting, duplicate branch replacement and errors
for incomplete tracked/ahead-behind records. Inline tests check 171 cases
executed from the actual Python parser.
Generator: `rust/tools/gen_coding_status_goldens.py --check`.
Logs: `/tmp/hermes-coding-status-workspace.log`,
`/tmp/hermes-coding-status-clippy.log`.

Next: bounded Git execution and ordered workspace rendering. The Python Git
wrapper is hermes_cli/_subprocess_compat.py::bounded_git_probe; preserve its
bounded post-kill cleanup so inherited descendant pipes cannot hang startup.
Gemini project-facts task 31333 remains live as last polled. Full native prompt
construction and session lifecycle integration remain unfinished.

## Focus toolset checkpoint: 2026-09-07

Workspace tests: **1,397 passed, two ignored**. focus_toolsets proposes the
profile toolset plus enabled MCP names only in focus mode, preserving raw
config order and Python's enabled-flag coercion. It accepts the raw profile
config explicitly: Python ignores the passed merged config in this path.
Inline tests check 204 cases from the actual Python method/helper/parser,
including malformed entries, float-vs-integer flags and duplicate toolset names.
Generator: `rust/tools/gen_focus_toolsets_goldens.py --check`.
Logs: `/tmp/hermes-focus-toolsets-workspace.log`,
`/tmp/hermes-focus-toolsets-clippy.log`.

The caller must still preserve explicit user toolset pins and freeze selection
per session. Gemini project-facts task 31333 is running, log
`/tmp/hermes-coding-facts-task.log`; it owns coding_project_facts.rs and its
generator/goldens only. Review before registering. Git workspace snapshot
probes and full native session construction remain pending.

## Coding renderer checkpoint: 2026-09-07

Workspace tests: **1,396 passed, two ignored**. Gemini's coding_prompt module
is reviewed and registered. It renders the coding brief, model edit-format
guidance, optional todo sentence removal, resolved workspace text and operator
instructions in separate ordered parts. Review corrected profile lookup to
exact Python names, without trimming/case normalization, and fixed ambiguous
test assertions before enabling the module. The real Python RuntimeMode oracle
checks 22 prompt assemblies and 34 model-family cases; source constants are
checked too. Generator: `rust/tools/gen_coding_prompt_goldens.py --check`.
Logs: `/tmp/hermes-coding-render-workspace.log`,
`/tmp/hermes-coding-render-clippy.log`.

Helper task 84850 completed; no helpers running. Next: focus-mode toolset
selection (Python reads raw MCP config, not the passed merged config), workspace
Git/project-fact snapshots, and runtime assembly using the resolved posture.
The registered renderer is not yet connected to native session construction.

## Coding workspace detection checkpoint: 2026-09-07

Workspace tests: **1,390 passed, two ignored**. coding_context detects coding
posture from explicit mode/surface/cwd/home/temp-root inputs. Auto/focus uses
interactive surfaces, markers within six ancestor levels and a shared-budget
500-entry source scan of the Git root plus immediate children. Home/temp
markers and home-rooted dotfiles repos are excluded. On/off short-circuit
without filesystem probing. Source suffix handling follows Python splitext.
Constants are extracted with
`rust/tools/gen_coding_detection_constants.py --check`.
Filesystem cases cover notes repos, source depth, home markers and surface
gates; actual Python detector execution confirmed the same outcomes.
Logs: `/tmp/hermes-coding-detect-workspace.log`,
`/tmp/hermes-coding-detect-clippy.log`.

Gemini renderer task 84850 remains live as last polled. Workspace facts/Git
snapshot rendering, focus-mode toolsets/skill categories and native construction
still need integration. Resolve posture once per session to preserve caching.

## Coding configuration checkpoint: 2026-09-07

Workspace tests: **1,389 passed, two ignored**. coding_context resolves mode
aliases and operator instructions from a loaded config snapshot. It preserves
Python scalar/container stringification, per-list-entry stripping and errors
for malformed truthy config maps. Inline tests check 97 actual Python cases.
Generator: `rust/tools/gen_coding_settings_goldens.py --check`.
Logs: `/tmp/hermes-coding-settings-workspace.log`,
`/tmp/hermes-coding-settings-clippy.log`.

Gemini coding renderer task 84850 is running, log
`/tmp/hermes-coding-render.log`. It owns coding_prompt.rs and its generator/
goldens only; do not register the module until reviewed. Runtime coding
detection, Git/project facts, focus-mode selections and full prompt integration
remain pending. Settings must be resolved once per session, not each turn.

## Outer context wrapper checkpoint: 2026-09-07

Workspace tests: **1,388 passed, two ignored**. ContextRequest/build_context
connects project loaders and independent profile SOUL under Python's exact
Project Context heading and single-newline join. A fallback cwd inside the
resolved install root suppresses project discovery unless allowed; an explicit
cwd, an ancestor or a similarly named sibling remains eligible. skip_soul
prevents identity duplication. Warnings remain owned by this build.
Filesystem tests cover 32 combinations; the actual Python wrapper and
install-tree guard were executed separately against the same routing cases.
Logs: `/tmp/hermes-context-wrapper-workspace.log`,
`/tmp/hermes-context-wrapper-clippy.log`.

Runtime construction must supply the correct installed package root, launch
cwd, explicit workspace and profile home. Home initialization, non-UTF-8
paths, metadata deadlines and full system-prompt lifecycle remain unfinished.
Next major section: coding-context loaders/gates from the completed Gemini map,
then skills/memory/plugin/footer assembly and native stored-prompt integration.

## Project context priority checkpoint: 2026-09-07

Workspace tests: **1,387 passed, two ignored**. context_files adds cwd-only
CLAUDE/claude loading and combined .cursorrules plus sorted immediate .mdc
loading. Cursor text preserves trailing newlines and applies one combined
budget. load_project selects exactly one nonempty source type in Python's
order: Hermes, AGENTS, CLAUDE, Cursor. Filesystem tests cover name/type
precedence, empty-file fallback, sorted Cursor entries and combined truncation.
Actual Python loader branches separately confirmed ordering and output bytes.
Logs: `/tmp/hermes-project-context-workspace.log`,
`/tmp/hermes-project-context-clippy.log`.

Next: build_context_files_prompt's outer wrapper, including independent SOUL,
the Project Context heading and fallback install-tree rejection. Python's
install-tree test is resolved-path equality/descendance of _PACKAGE_ROOT;
ancestors of the package root remain legitimate workspaces. No helpers running.
Full live native prompt orchestration remains unfinished.

## Project instruction loading checkpoint: 2026-09-07

Workspace tests: **1,386 passed, two ignored**. context_files now loads the
AGENTS directory chain and nearest Hermes instruction file through the shared
bounded reader, scan and truncation path. SOUL uses that reader too, retaining
its existing filesystem/deadline tests. AGENTS override/name precedence,
raw-content deduplication, ancestor provenance labels and the merged-chain
budget match Python. Empty overrides fall through; duplicate overrides stop
that directory's name search. Real filesystem tests cover these transitions
and Hermes frontmatter; the Python AGENTS loader was executed separately to
confirm precedence, duplicate handling and merged truncation behavior.
Logs: `/tmp/hermes-context-load-workspace.log`,
`/tmp/hermes-context-load-clippy.log`.

Next: CLAUDE/cursor context loaders and the full context assembly gates,
including install-tree rejection and skip-SOUL. Discovery metadata calls are
not yet deadline-bounded. Gemini map retry 10545 completed with exit 0;
`rust/analysis/coding-context-map.md` is ready for source-level review.
No helper tasks remain running. Full native prompt integration is pending.

## Context discovery checkpoint: 2026-09-07

Workspace tests: **1,385 passed, two ignored**. context_files discovers the
nearest .hermes.md/HERMES.md within the Git root, recognizes .git files as
well as directories, and produces the root-to-cwd AGENTS directory chain.
Without a Git root it checks only cwd. It reuses the existing realpath helper;
non-UTF-8 paths currently return an explicit error and remain a parity gap.
Frontmatter stripping follows Python delimiter slicing, including leading
BOMs and preserving a document whose stripped body is empty. The inline test
checks 120 Python AST cases; filesystem tests cover discovery precedence and
repository boundaries. Generator:
`rust/tools/gen_context_frontmatter_goldens.py --check`.
Logs: `/tmp/hermes-context-discovery-workspace.log`,
`/tmp/hermes-context-discovery-clippy.log`.

Next: load/scan/truncate discovered context files, preserve provenance and
AGENTS override precedence, and apply install-tree/skip-SOUL gates during
assembly. Gemini coding-context map retry 10545 remains live as last polled.

## Typed environment hint checkpoint: 2026-09-07

Workspace remains **1,383 passed, two ignored**. resolve_extra_hint now accepts
the actual config snapshot rather than a prefiltered string. It preserves
Python stringification of null, booleans, numbers and containers, skips
malformed agent sections, and prioritizes a nonblank environment hint.
The existing inline test now checks 72 cases from the real Python resolution
block. Generator: `rust/tools/gen_environment_extra_hint_goldens.py --check`.
Logs: `/tmp/hermes-extra-hint-workspace.log`, `/tmp/hermes-extra-hint-clippy.log`.

Gemini coding-context mapping task 99915 exited 1 with a network error and no
report. One retry is running as session 10545, log
`/tmp/hermes-coding-context-map-retry.log`, expected report
`rust/analysis/coding-context-map.md`. Poll that handle before any restart.
Full live prompt orchestration and environment probes remain pending.

## Environment renderer checkpoint: 2026-09-07

Workspace tests: **1,383 passed, two ignored**. Gemini's environment_prompt
module is reviewed and registered in main.rs. It renders explicit local host,
Windows/WSL, remote probe/fallback and extra-hint inputs. Review fixed built-in
remote classification and description precedence, Python whitespace/line
splitting, and preservation of nonempty formatted probe bytes. Removed the
partial ambient WSL detector and Cargo's cwd-dependent Python subprocess test.
The external oracle generator now checks 36 actual Python rendering cases,
including conflicting plugin metadata and whitespace edge cases.
Generator: `rust/tools/gen_environment_prompt_goldens.py --check`.
Logs: `/tmp/hermes-environment-render-workspace.log`,
`/tmp/hermes-environment-render-clippy.log`.

Rendering is not live environment discovery. Native construction still needs
the owning terminal backend's probes, cleanup/cache lifecycle, local host
facts, typed config-to-extra-hint conversion and full prompt orchestration.
EnvironmentPromptInputs::is_remote represents plugin classification; it cannot
override built-in remote backends to local. No helper tasks remain running.

## Embedded TUI prompt checkpoint: 2026-09-07

Workspace remains **1,377 passed, two ignored**. platform_hint now applies
the embedded-terminal clarification after user overrides, gated on normalized
tui identity and a captured desktop-terminal flag. Empty hints are preserved
and existing clarification text is not duplicated. The selection oracle now
executes Python's real qualifier and truthy helper: 351 selection cases plus
153 override cases. Generator: `rust/tools/gen_platform_hint_goldens.py --check`.
Logs: `/tmp/hermes-tui-hint-workspace.log`, `/tmp/hermes-tui-hint-clippy.log`.

Gemini environment renderer session 49230 completed with exit 0. Files exist
at environment_prompt.rs and its generator/goldens, but the module is NOT
registered, reviewed or included in the above checks yet. Review all branches
and oracle coverage before integration. Its runtime discovery/probes remain
pending; do not equate explicit-input rendering with live environment support.

## Profile/provider prompt text checkpoint: 2026-09-07

Workspace tests: **1,377 passed, two ignored**. profile_hint renders the
default/named profile warning using resolved home/root strings, without
appending profiles/name twice. provider_identity ports the exact Alibaba
model workaround and its last-slash model label. Inline tests check 18 profile
and 20 provider cases executed from the actual Python branches.
Generator: `rust/tools/gen_prompt_identity_goldens.py --check`.
Logs: `/tmp/hermes-prompt-identity-workspace.log`,
`/tmp/hermes-prompt-identity-clippy.log`.

These renderers still need the owning assembler's resolved profile inputs and
correct stable/post-workspace placement. Gemini environment renderer task
49230 remains live as last polled. Continue the full loader/assembler wiring;
do not mistake the standalone text helpers for native runtime completion.

## Resolved section assembly checkpoint: 2026-09-07

Workspace tests: **1,376 passed, two ignored**. ResolvedPromptSections now
assembles loaded sections into the three prompt tiers. Coding workspace
presence moves the coding tail and post-workspace guidance into context;
absence retains them in stable. The gate uses raw list presence before trim,
including a list containing an empty string. Caller/context files follow;
skills, memory, user profile, external memory, plugins and footer retain their
volatile ordering. Inline tests cover the boundary, with all three cases also
checked by executing the actual Python branches and finalizer.
Logs: `/tmp/hermes-section-routing-workspace.log`,
`/tmp/hermes-section-routing-clippy.log`.

This assembles resolved snapshots, not the missing loader/runtime gates.
Gemini is implementing the environment renderer in session 49230, log
`/tmp/hermes-environment-render-task.log`. Owned files: environment_prompt.rs,
gen_environment_prompt_goldens.py and environment-prompt-goldens.json. It must
not run Cargo or wire modules. Review its renderer and oracle before registering
the module; full remote probes and environment discovery remain separate work.

## Context settings and stalled-read checkpoint: 2026-09-07

Workspace tests: **1,375 passed, two ignored**. context_file_limits resolves
explicit config limits and deadlines, Python numeric/boolean acceptance,
fractional truncation and the model-window cap's 20K floor/500K ceiling.
The inline oracle test checks 1,331 cases executed from Python's real config
resolvers. Generator: `rust/tools/gen_context_limits_goldens.py --check`.
A Unix FIFO test now exercises the real SOUL reader with no writer: its
deadline returns no content, then a late writer releases the detached reader.
Logs: `/tmp/hermes-context-limits-workspace.log`,
`/tmp/hermes-context-limits-clippy.log`.

Gemini environment map task 81000 completed with exit 0; report
`rust/analysis/environment-prompt-map.md` is ready for source-level review.
Next: connect resolved loader settings during full prompt construction, handle
profile home initialization, and assemble environment/coding/context tiers.
Production native prompt integration remains unfinished.

## Scoped SOUL read checkpoint: 2026-09-07

Workspace tests: **1,373 passed, two ignored**. load_soul reads SOUL.md from
an explicit home on a detached reader thread with a caller-supplied deadline,
normalizes Python text-mode newlines/whitespace, removes one leading BOM,
scans with the shared context threat patterns and applies the validated
truncator. It returns content and warning to the owning prompt build. Read
errors and timeouts return no identity so the assembler can use its fallback.
Real filesystem tests cover two concurrent profile homes, newline/BOM handling,
blank files, invalid UTF-8, absent files and the exact blocked-content marker.
The marker was checked by executing Python's scan helper with its real scanner.
The timeout branch still needs a controlled slow-read test.
Logs: `/tmp/hermes-soul-loader-workspace.log`,
`/tmp/hermes-soul-loader-clippy.log`.

Full loader orchestration remains pending: config/dynamic cap resolution,
read timeout config and ensure_hermes_home initialization must be supplied by
the owning agent build. Production prompt construction is not connected yet.
Gemini environment map task 81000 remains live as last polled.

## Context truncation checkpoint: 2026-09-07

Workspace tests: **1,372 passed, two ignored**. system_prompt ports the
context-file head/tail truncator with Unicode character counts, Python's
floating-point slice sizes, full-content zero-tail behavior, concrete read-path
markers and exact warning text. Warnings return to the owning prompt build,
not a shared global collection. The inline test checks 132 cases executed from
the Python function AST. Generator:
`rust/tools/gen_context_truncation_goldens.py --check`.
Logs: `/tmp/hermes-context-truncation-workspace.log`,
`/tmp/hermes-context-truncation-clippy.log`.

The SOUL loader is still pending. Next dependencies: resolved config/dynamic
character limits, bounded UTF-8 reads, Python whitespace normalization and the
context threat scan (reuse threat_patterns::scan_for_threats with context scope).
Preserve the single leading BOM removal and blocked-content marker semantics.
Gemini is mapping full environment hint dependencies in session 81000, log
`/tmp/hermes-environment-prompt-map.log`, expected report
`rust/analysis/environment-prompt-map.md`. Poll that task before restarting.

## Platform selection checkpoint: 2026-09-07

Workspace tests: **1,371 passed, two ignored**. platform_hint selects built-in
guidance before plugin fallback, normalizes platform keys with Python whitespace
rules, adds Telegram rich-message guidance using merged leaf settings, then
applies the override resolver. A malformed truthy parent config suppresses the
extension, while malformed extra leaves act as empty maps, matching Python.
The platform-hint generator now also executes the actual selection block for
270 cases; the existing 153 override cases remain. Embedded TUI qualification
and full assembler wiring are still pending.
Logs: `/tmp/hermes-platform-selection-workspace.log`,
`/tmp/hermes-platform-selection-clippy.log`.

Gemini review session 70218 completed with exit 0. Findings are in
`rust/analysis/stable-guidance-review.md`. Reviewer caveats: stable_guidance
takes resolved SOUL text, so whitespace normalization belongs at the pending
loader boundary. Python raises TypeError for truthy non-string kanban guidance;
silently dropping it, as suggested by the review, is not parity. Replace the
Rust panic with explicit validation/error propagation when wiring resolved
agent inputs. Do not treat the review's proposed minimal host block as full
environment parity: remote backends and platform-specific hints remain required.

## Platform hint override checkpoint: 2026-09-07

Workspace tests: **1,370 passed, two ignored**. system_prompt now resolves
per-platform hint overrides with Python's string shorthand, replacement,
append, malformed-value fallback and Unicode whitespace behavior. When both
replace and append are present, Python applies both, despite the resolver's
docstring suggesting otherwise. Follow the executable reference.
The inline test checks 153 cases from the actual Python resolver AST;
generator `rust/tools/gen_platform_hint_goldens.py --check`.
Logs: `/tmp/hermes-platform-hint-workspace.log`,
`/tmp/hermes-platform-hint-clippy.log`.

Default/plugin hint selection, Telegram rich-message extension and embedded
TUI qualification still need assembly wiring. The resolver accepts already
normalized platform keys and resolved override settings, not ambient config.
Gemini review session 70218 remains live as last polled; use that handle.

## Initial stable guidance checkpoint: 2026-09-07

Workspace tests: **1,369 passed, two ignored**. The system_prompt module now
selects identity, Hermes help, task/parallel guidance, grouped tool guidance,
steering, model enforcement and execution guidance in Python's order. The help
pointer requires both skill_view and the rendered hermes-agent skill entry.
Execution guidance removes web_search advice when that tool is unavailable.
The inline test verifies exact ordered section bytes against 192 assemblies
executed from the Python function's AST.

The guidance catalog contains 18 values. Its generator extracts literals and
evaluates the two memory aliases through the reviewed pure string builder,
without importing runtime modules. Both generators support --check:
`rust/tools/gen_system_prompt_guidance.py` and
`rust/tools/gen_stable_guidance_goldens.py`.
Logs: `/tmp/hermes-stable-guidance-workspace.log`,
`/tmp/hermes-stable-guidance-clippy.log`.

This is not full prompt construction or production integration. Next: provider
identity, environment/coding sections, profile/platform hints, context files,
volatile memory/skills/metadata, then stored-prompt restore/build orchestration.
Keep the coding workspace boundary and historical section order intact.
Gemini is reviewing the initial selection and next integration steps, task
session 70218, log `/tmp/hermes-stable-guidance-review.log`, expected report
`rust/analysis/stable-guidance-review.md`. Poll the known task before restarting.
The earlier static catalog extraction completed. A fresh AGY auth probe returned
AUTH_OK with exit 0 through rust/tools/agy.sh without login interaction.

## Prompt tier assembly checkpoint: 2026-09-07

Workspace tests: **1,368 passed, two ignored**. PromptParts now finalizes stable,
context and volatile sections using Python's exact trim/filter/double-newline
rules. The shared model-guidance predicate ports explicit booleans, recognized
string settings, custom substring lists and default-family fallback. Callers
must still gate these sections on tool availability.
An inline test checks 64 tier joins and 240 guidance decisions generated from
the actual Python finalizer, full join function and enforcement branch.
Generator: `rust/tools/gen_system_prompt_assembly_goldens.py --check`.
Logs: `/tmp/hermes-prompt-assembly-workspace.log`,
`/tmp/hermes-prompt-assembly-clippy.log`.

Full section selection/loaders and restore/build orchestration remain pending.
Gemini's static-guidance extraction completed and its catalog is now validated
in the initial stable guidance checkpoint above.

## Stored prompt identity checkpoint: 2026-09-07

Workspace tests: **1,367 passed, two ignored**. The new system_prompt module
ports Python's stored-prompt runtime guard: last matching model/provider/platform
footer lines win, while cwd must be found within the host-info block's next three
lines. Project prose cannot shadow that block and trigger perpetual rebuilding.
The guard uses Python-compatible line splitting and whitespace handling.
An inline test checks 924 cases generated by executing the actual Python guard,
covering missing/blank fields, conflicting footers, cwd windows and Unicode line
separators. Generator: `rust/tools/gen_stored_prompt_runtime_goldens.py --check`.
Logs: `/tmp/hermes-prompt-runtime-workspace.log`,
`/tmp/hermes-prompt-runtime-clippy.log`.

The restore/build lifecycle has not invoked this guard yet. Gemini is generating
the static prompt guidance catalog and its --check generator under rust/tools;
task log `/tmp/hermes-guidance-task.log`. Next: assemble the full ordered tiers,
then connect stored-prompt reuse, runtime checks and persistence to native turns.

## Prompt snapshot boundary checkpoint: 2026-09-07

Workspace tests: **1,366 passed, two ignored**. NativeAgentClient accepts an
assembled immutable system prompt, shared by client clones and prepended to both
streaming and tool-loop histories. Real HTTP checks verify identical prompt bytes
across turns, one injected system message per request with ordinary history, and
stable cache keys across turns/tool rounds. The full prompt assembler has not
yet supplied this input in production.

SessionDb::update_system_prompt now ports Python's content-addressed update:
insert the body, replace the session hash/clear legacy inline text, and garbage
collect unused bodies in one transaction. An inline SQLite test covers shared
deduplication, exact whitespace, null/empty distinctions, missing-row cleanup,
rollback after an injected update failure, and reopen persistence.
Logs: `/tmp/hermes-prompt-snapshot-workspace.log`,
`/tmp/hermes-prompt-snapshot-clippy.log`.

Gemini's system-prompt map is complete at
`rust/analysis/native-system-prompt-map.md`. Next: full tiered assembly and the
restore/build lifecycle, including runtime identity checks and conversation
cache boundaries. SOUL.md alone is not full prompt parity.

## Scoped native factory checkpoint: 2026-09-07

Workspace remains **1,365 passed, two ignored**. The explicit-home factory now
returns initialization errors instead of silently choosing the Python bridge
when native construction fails. The default startup wrapper retains its prior
fallback. Native construction uses prepared secret scopes when present, requires
a scope in multiplex mode, and resolves provider keys, endpoint variables and
runtime limits through get_secret. Generic key lookup now accepts an injected
environment resolver too. Prepared scopes take precedence over a fresh .env read.

Extended real HTTP factory checks prove an empty authoritative scope cannot
borrow populated file/process keys for registered or generic providers, while
a hydrated scoped key reaches the model request. Logs:
`/tmp/hermes-scoped-factory-workspace.log`, `/tmp/hermes-scoped-factory-clippy.log`.

Next: connect profile selection to the strict factory with per-profile config
and explicit overrides, plus prompt/tool/terminal policy and subprocess scoping.
This has not enabled live multiplexing. Gemini is mapping complete native system
prompt construction to `rust/analysis/native-system-prompt-map.md` (pending while
helper runs), log `/tmp/hermes-prompt-map-task.log`.

## Profile factory input checkpoint: 2026-09-07

Workspace remains **1,365 passed, two ignored**. The native agent factory now
accepts an explicit profile home and reads its .env once per client build;
default startup uses the existing ambient-home wrapper. The factory HTTP test
now builds two profile clients concurrently and checks distinct model/auth
headers without changing HERMES_HOME. This establishes the selected-home input,
not full runtime multiplexing or missing-secret isolation.
Logs: `/tmp/hermes-profile-home-workspace.log`,
`/tmp/hermes-profile-home-clippy.log`.

Gemini's profile-runtime map completed and has source-review notes at
`rust/analysis/profile-agent-runtime-map.md`. Next: enforce scoped credential
resolution and fallback behavior, then compose profile agent/prompt/tool/terminal
policy with route and history selection. Native persona loading, subprocess
scope propagation and conversation-stable caching are still required.

## Turn cancellation ownership checkpoint: 2026-09-07

Workspace tests: **1,365 passed, two ignored**. Admitted HTTP and push turns now
own their transcript lease, agent completion, persistence and delivery in a task
independent of the ingress waiter. Cancelling that waiter no longer drops the
lease while the detached inner agent is still running or discards its reply.
The HTTP regression failed before the fix. Inline HTTP and push tests pause an
agent after its terminal event, cancel the waiter, prove the lease remains held,
then verify the completed reply is persisted (and delivered for push) before
another owner can acquire the transcript.
Logs: `/tmp/hermes-http-cancel-red.log`, `/tmp/hermes-turn-owner-workspace.log`,
`/tmp/hermes-turn-owner-clippy.log`.

This is ingress-waiter cancellation handling, not shutdown drain or explicit
agent interruption. Those remain separate integration work. Gemini is mapping
profile-specific agent construction to `rust/analysis/profile-agent-runtime-map.md`
(pending while helper runs), log `/tmp/hermes-profile-agent-task.log`.

## Live coordinator startup checkpoint: 2026-09-07

Workspace tests: **1,363 passed, two ignored**. Main now initializes SessionStore
for backends whose history the gateway owns, using the full gateway config loader,
resolved running-profile identity and configured freshness. HTTP and push ingress
pass their exact earlier Rust ID/source pair into the per-key session flight.
After ordinary recovery misses, the owner atomically claims an eligible legacy
row and retries normal recovery. Idle expiry, predecessor lineage and force-new
therefore retain the established transition behavior.

Inline tests cover legacy adoption/restart, idle predecessor preservation,
force-new bypass, and actual native-model HTTP history replay. A separate compiled
binary smoke test in a temporary Hermes home exercised startup, HTTP turns,
restart with the same durable ID, and legacy history reaching a fixture CLI agent.
Clippy passes with warnings denied. Logs:
`/tmp/hermes-legacy-adoption-workspace.log`,
`/tmp/hermes-legacy-adoption-clippy.log`, `/tmp/hermes-legacy-startup-smoke.log`.

Remaining integration: profile route attribution and profile-specific agent
configuration, background process registry pinning, bridge-owned history,
cancellation ownership, and legacy IDs unsafe for the routing entry codec.
Existing unsafe-ID transcripts now return an explicit migration error before
mutation rather than silently starting over; those IDs still need a migration.
The process probe currently models Python's absent-registry case.

## Legacy history ownership checkpoint: 2026-09-07

Workspace tests: **1,361 passed, two ignored**. SessionDb now has an atomic
claim for rows written by the earlier Rust history path. The guarded UPDATE
binds routing identity without copying messages or changing activity timestamps.
It rejects previously owned rows, source/channel mismatches, incompatible
thread/type, nonrecoverable closures and any existing destination lineage.
Two inline SQLite tests cover preservation, boundary guards, retries and one
winner between competing independent connections.
Logs: `/tmp/hermes-legacy-claim-workspace.log`,
`/tmp/hermes-legacy-claim-clippy.log`.

Gemini's migration review completed; verified design notes are at the top of
`rust/analysis/legacy-rust-history-migration.md`. Next: pass the exact legacy
message ID into the session flight, claim only after ordinary recovery misses,
then retry recovery through the normal policy/lineage path. The claim is not
yet invoked by the coordinator, and startup remains unwired.

## Startup identity and freshness checkpoint: 2026-09-07

Workspace tests: **1,359 passed, two ignored**. Added path-based running profile
inference matching get_active_profile_name, including missing path tails,
symlink-before-parent resolution, custom homes and the source ID regex behavior.
The inference does not read the sticky active_profile file or HERMES_PROFILE.
Added freshness resolution matching the config-to-env startup bridge followed
by auto_continue_freshness_window, without mutating environment variables.
A present config setting overrides the env fallback even if invalid. Numeric
strings, Unicode digits, underscores, nonpositive values and nonfinite values
retain Python's behavior; malformed input falls back to one hour.
Executed both actual Python functions via AST extraction against the key Rust
test cases. Logs: `/tmp/hermes-startup-inputs-workspace.log` and
`/tmp/hermes-startup-inputs-clippy.log`.

These inputs are ready for startup composition; main still does not create the
coordinator. Gemini's migration task is reviewing adoption of old Rust transcript
IDs into the routing store. Output: `rust/analysis/legacy-rust-history-migration.md`
(pending while the helper runs), log `/tmp/hermes-legacy-history-task.log`.

## HTTP routing checkpoint: 2026-09-07

Workspace tests: **1,356 passed, two ignored**. HTTP `/message` now consumes the
optional AppState SessionStore, resolves durable identity before history reads,
and refreshes session activity after the agent finishes. HTTP and all push
dispatchers receive one shared transcript lease registry and generation counter.
The HTTP lease spans history load, model completion and transcript flush.
Inline tests exercise native model HTTP/tool rounds with image history, reload
the routing store to recover the durable transcript, and verify an HTTP turn
cannot reach the model while another ingress holds its transcript lease.
Logs: `/tmp/hermes-http-routing-workspace.log`,
`/tmp/hermes-http-routing-clippy.log`.

Startup still needs to instantiate the coordinator. Gemini's input map is at
`rust/analysis/session-store-startup-inputs.md`, with review corrections at its
top. Next work is startup/profile/freshness inputs and old Rust history migration.
Active process tracking, bridge-owned history, and cancellation ownership still
need end-to-end integration; these tests do not establish those contracts.

## Session activity parity checkpoint: 2026-09-07

Workspace tests: **1,354 passed, two ignored**. Gemini's transition review
identified a confirmed snapshot gap: activity touched during an unlocked process
probe could be lost when Rust evaluated idle policy. Reset checks now refresh
fields from the same entry instance after I/O, preserving replacement identity
guards. The inline regression failed before the fix and passes afterward;
executing Python's actual reset method confirmed the expected behavior.
Review dispositions are recorded in `rust/analysis/store-transition-review.md`.
Log: `/tmp/hermes-probe-activity-workspace.log`.
Next: complete startup inputs and history migration, then enable coordinated
sessions across push and HTTP ingress. The full port remains in progress.

## Dispatcher and helper checkpoint: 2026-09-07

Current validation: **1,353 workspace tests passed, two ignored** (one core test
and 1,352 gateway tests). Clippy passes with warnings denied. Dispatcher now
accepts an optional SessionStore, resolves durable session identity before leases
and history reads, and performs blocking store work off Tokio executor threads.
An inline test verifies two turns across a store restart share durable history.
Production startup has not enabled this builder yet. Existing Rust history-ID
migration, profile inputs, active-process probes, HTTP ingress and bridge-owned
history still need integration. Gemini's store review is available but its
findings still need checking against Python before applying changes.
Logs: `/tmp/hermes-dispatch-store-workspace.log` and
`/tmp/hermes-dispatch-store-clippy.log`.

Gemini authentication was rechecked with a fresh wrapper call returning `AUTH_OK`.
Keep desktop D-Bus selection, `BROWSER=true`, closed stdin, the invocation lock,
and `--dangerously-skip-permissions` in `rust/tools/agy.sh`.

## Resumed after rest checkpoint: 2026-09-06

Previous resumed validation: **1,352 workspace tests passed, two ignored**.
SessionStore::get_or_create_session now composes per-key flights, waiter activity
touching, scoped Slack memory migration, compression healing, stale/reset checks,
durable recovery, ordinary/forced publication, routing persistence, predecessor
promotion and row creation. Entry locks are released for DB work. Reopen and final
creation resolve the owning handle again so an earlier failed lookup does not pin
the transition to a missing handle. Three real SQLite tests cover creation/reuse,
accidental closures, compression, restart, force-new, idle lineage and Slack move.
Logs: `/tmp/hermes-store-transition-workspace.log` and
`/tmp/hermes-store-transition-clippy.log`.
Next: consume Gemini's `review-store-transition.txt` review and wire startup/
dispatch to this coordinator, including profile config and process/freshness
inputs. The coordinator is callable and tested, but main/dispatch still use their
prior live path. No end-to-end platform claim is made by these tests.

Previous activity update checkpoint: **1,349 workspace tests passed, two ignored**.
SessionStore::update_session captures metadata and peer identity under the index
lock, then performs fast routing writes, fallback persistence and peer refresh
outside it. Recovery reads also happen outside the lock before merging current
outage edits. New initialized-state snapshot helpers do no I/O. A real SQLite
test verifies internal activity-clock preservation, token/null handling, persisted
routing metadata, missing-row peer repair and missing-key no-op. Logs:
`/tmp/hermes-store-update-workspace.log` and `/tmp/hermes-store-update-clippy.log`.
Next: compose get-or-create using flights, existing-route decisions and recovery,
then instantiate SessionStore and assign durable IDs in dispatch. The update path
is available for waiter activity handling but not yet invoked by live dispatch.

Previous store startup checkpoint: **1,348 workspace tests passed, two ignored**.
SessionStore now owns the index mutex, shared recovery/flight state, configuration
and profile database resolver. Its constructor loads and prunes before exposing
the store to workers, persisting structural repairs when needed. A real SQLite
startup test verifies accidental-close reopening with queued metadata preserved,
and no ambient fallback or directory creation for an unprovisioned profile.
Logs: `/tmp/hermes-store-startup-workspace.log` and
`/tmp/hermes-store-startup-clippy.log`.
Next: implement the store's live get-or-create phases and waiter activity updates,
then instantiate it in gateway startup and dispatch. The constructor is tested but
not yet called by main. Gemini's coordination review is complete; follow-up notes
reject its two suggestions that would change explicit Python flight semantics.

Previous profile recovery checkpoint: **1,347 workspace tests passed, two ignored**.
Recovery and pruning accept both borrowed SessionDb handles and shared Arc handles,
allowing SessionDatabases to resolve each lookup directly. A combined real SQLite
test follows a named profile's compression child despite a conflicting root copy,
then persists the repaired route solely in the fixed routing-index database.
Logs: `/tmp/hermes-profile-recovery-workspace.log` and
`/tmp/hermes-profile-recovery-clippy.log`.
Next: owning coordinator initialization and transition composition, then live
dispatch assignment. The combined path is verified in tests; startup has not yet
been switched to it. Gemini's coordination review remains running.

Previous session flights checkpoint: **1,346 workspace tests passed, two ignored**.
SessionFlights elects one transition owner per key and shares its result/error
with overlapping callers. Different keys stay independent. Owner Drop wakes
waiters and removes abandoned flights; completed/failed flights also leave the
registry so later calls can retry. Two concurrency tests cover shared identity,
shared errors, independent keys and owner abandonment. Logs:
`/tmp/hermes-session-flights-workspace.log` and
`/tmp/hermes-session-flights-clippy.log`.
Next: compose the owning coordinator, including waiter touch_activity behavior,
per-key Arc database resolution, initialization, transition/persistence phases
and live dispatch assignment. Blocking waits must run outside Tokio executor
threads. Gemini is reviewing coordination in `tasks/review-session-coordination.txt`.

Previous creation oracle checkpoint: **1,344 workspace tests passed, two ignored**.
All 52 Gemini creation fixtures now run against Rust SQLite. The inline test seeds
sessions/prompts, executes creation or enrichment, and compares failures, selected
session fields and prompt rows after every operation. Clock and owner lookup are
injected identically; real filesystem ownership tests remain separate. Regeneration
passes under Python 3.12.13. Logs: `/tmp/hermes-create-oracle-workspace.log` and
`/tmp/hermes-create-oracle-clippy.log`.
Next: per-key coordinator flight sharing, then compose the existing recovery,
reset, creation/publication and persistence phases and wire live dispatch.
Python overlapping calls share the owner's result even for force-new; simply
serializing calls would create multiple replacement sessions and is insufficient.

Previous existing-reset checkpoint: **1,343 workspace tests passed, two ignored**.
existing_reset_reason now implements existing-route suspension and resume-pending
freshness checks around ordinary reset policy. Mode none disables marker expiry;
exact freshness boundaries do not expire; an explicit resume timestamp takes
precedence over updated_at. Active processes only suppress the ordinary reset
gate, matching Python. Tests also cover disabled/nonfinite windows and mixed
aware/naive timestamp errors. Logs: `/tmp/hermes-existing-reset-workspace.log` and
`/tmp/hermes-existing-reset-clippy.log`.
Gemini completed 52 creation fixtures in `tools/session-create-goldens.json`;
`gen_session_create_goldens.py --check` passes using Python 3.12.13. Next review
and consume these in Rust, then complete the owning coordinator and dispatch
integration. The existing-route helper is not yet called by live dispatch.

Previous compression checkpoint: **1,342 workspace tests passed, two ignored**.
SessionDb now walks compression chains and returns their tips using Python's
continuation/live/closed ordering, message-plus-heartbeat recency, branch/delegate/
tool exclusions, cycle detection and 100-hop limit. SessionEntry can apply a tip
only if its current ID still matches the originally queried ID, preserving all
other state and object identity. Three tests cover these behaviors, including
real SQLite branching, malformed JSON and long chains. Logs:
`/tmp/hermes-compression-walk-workspace.log` and
`/tmp/hermes-compression-walk-clippy.log`.
Next: existing-route suspended/resume-pending/reset decisions and coordinator
wiring. Compression lookup is available but not called by live dispatch yet.
Gemini's session creation fixture generation remains running.

Previous force-new checkpoint: **1,339 workspace tests passed, two ignored**.
Force-new publication now checks the identity of the entry observed before
unlocked work. SessionEntry snapshots retain a private Arc token; fresh/reloaded
entries receive a distinct token, even for identical serialized data. Ordinary
publication remains vacancy-only. A regression covers metadata mutation, winning
replacement, stale observers, equal-data reloads and an empty slot. Logs:
`/tmp/hermes-force-publication-workspace.log` and
`/tmp/hermes-force-publication-clippy.log`.
Next: existing-route compression/reset checks and owning coordinator integration.
Live dispatch assignment remains pending. Gemini's creation fixture task is still
running; no generated fixtures have been consumed yet.

Previous candidate checkpoint: **1,338 workspace tests passed, two ignored**.
SessionEntry::new_candidate creates Python-shaped timestamp/random IDs, origin,
creation timestamps and reset lineage with the normal entry defaults.
RoutingIndex::publish_candidate handles ordinary publication under the owning
lock and identifies the winner for subsequent DB creation. An eight-thread test
verifies exactly one winner and consistent published identity/context. Logs:
`/tmp/hermes-session-candidate-workspace.log` and
`/tmp/hermes-session-candidate-clippy.log`.
Next: coordinator force-new observed-entry replacement, phased existing-route
checks and live dispatch wiring. These helpers do not yet implement the full
coordinator or assign live Message IDs. Gemini is generating broader creation
fixtures through `tasks/gen-session-create-oracle.txt`.

Previous session creation checkpoint: **1,337 workspace tests passed, two ignored**.
SessionDb::create_session now writes full creation metadata atomically, enriches
NULL fields on conflict, preserves reset markers during model enrichment, stores
hashed system prompts and inherits parent context/compression routing. SQL is
copied from Python _insert_session_row, including namespace checks. Migration adds
model/cwd/git metadata columns. Two real SQLite tests cover enrichment, prompt
rollback, compression versus live-parent routing and profile boundaries. Logs:
`/tmp/hermes-session-create-workspace.log` and `/tmp/hermes-session-create-clippy.log`.
Next: create/publish candidate entries through the owning session coordinator,
then wire dispatch. Python's extended write-retry patience is not yet ported.
Gemini's live integration map is complete but has factual corrections at its top;
use it as a call-site map, not an implementation specification.

Previous identity propagation checkpoint: **1,335 workspace tests passed, two ignored**.
Message now carries an internal resolved_session_id, skipped by serde in both
directions. message_session_id prefers it, so dispatch leases, delivery identity,
begin/end history and native cache scope can use the same durable identity.
Platform adapters leave it unset; replies preserve it. Existing temporary IDs
remain the fallback until the session coordinator assigns durable IDs.
A real SQLite test verifies transcript continuity across channel/thread/workspace
changes, untrusted JSON rejection and bridge-managed-history behavior. Logs:
`/tmp/hermes-resolved-id-workspace.log` and `/tmp/hermes-resolved-id-clippy.log`.
Next: implement coordinator creation/resume and assign the ID before dispatch's
lease/history access. This checkpoint adds identity propagation, not live durable
session resolution. Gemini's live integration map remains running.

Previous recovery locking checkpoint: **1,334 workspace tests passed, two ignored**.
SessionRecovery now owns legacy claims independently of RoutingIndex. The owner
can clone its Arc under the index lock and perform recovery after releasing that
lock. Claim reservation holds its own mutex only for insertion, never during DB
lookup. Existing index methods delegate to the shared recovery component.
A concurrent regression blocks the legacy DB resolver and verifies that routing
and claim locks remain available while a second workspace cannot claim the same
legacy key. Logs: `/tmp/hermes-recovery-lock-workspace.log` and
`/tmp/hermes-recovery-lock-clippy.log`.
Next: owning store initialization, startup pruning and phased inbound routing.
Dispatch still derives temporary IDs via message_session_id for leases/history;
these must change together when durable routing is wired. Gemini is mapping those
call sites in `tasks/map-live-session-integration.txt`. No live resolver change
was made at this checkpoint.

Previous shared registry checkpoint: **1,333 workspace tests passed, two ignored**.
SessionDb::open_shared now acquires one Arc-owned writer per resolved path, with
per-path opening locks and Unix device/inode generation checks. A failed
replacement open forgets the retired generation and retries fresh; existing Arc
owners retain their generation until dropped. Gateway startup and SessionDatabases
now use this registry. Two real SQLite tests cover eight simultaneous callers,
path/symlink aliases, last-owner release, replacement failure and fresh retry.
Logs: `/tmp/hermes-shared-db-workspace.log` and `/tmp/hermes-shared-db-clippy.log`.
Gemini's `analysis/profile-db-resolution-map.md` is complete. Next: independent
legacy-claim coordination and the owning session store's phased startup/inbound
wiring. Full Python replacement guards on writes, shutdown close-all semantics,
and non-Unix file identity parity remain unported; tests do not establish them.

Previous profile resolver checkpoint: **1,331 workspace tests passed, two ignored**.
SessionDatabases now resolves named session owners separately from the fixed
routing-index database and reuses RecoverableHandleCache for bounded retries.
Ambient home is explicit for default/legacy keys. Named profiles require an
existing, non-tombstoned directory; misses are retried after provisioning, while
successful home resolution is memoized as in Python. Two real SQLite tests cover
ownership isolation, ambient/default behavior, routing stability, case aliases,
enrollment, deletion markers and failed-open retry. Logs:
`/tmp/hermes-profile-databases-workspace.log` and
`/tmp/hermes-profile-databases-clippy.log`.
Next: process-wide shared SessionDb handles (current cache is per resolver),
independent legacy-claim coordination, and the owning session store's phased
startup/inbound wiring. SessionDatabases is not yet connected to the live gateway.
Gemini's profile resolution mapping remains running.

Previous query recovery checkpoint: **1,329 workspace tests passed, two ignored**.
RoutingIndex::query_recoverable now returns durable predecessors without reset
promotion or reopen. It shares candidate selection and isolation with synchronous
recovery but records legacy identity before returning, including expired rows.
Each lookup failure becomes a miss independently, preserving legacy fallback.
Two real SQLite tests compare query/startup behavior and simulate a failed primary
store with a working legacy store. Logs: `/tmp/hermes-query-recovery-workspace.log`
and `/tmp/hermes-query-recovery-clippy.log`. The query currently requires mutable
RoutingIndex access for legacy claims; move shared claim coordination outside the
index lock when wiring the live store's unlocked I/O phase. It is not inbound-wired.
Gemini is mapping per-key DB resolution in `tasks/map-profile-db-resolution.txt`.
Next: consume that mapping, implement the owning session store and profile handle
resolution, then connect startup pruning and phased inbound recovery.

Previous pruning checkpoint: **1,327 workspace tests passed, two ignored**.
RoutingIndex::prune_stale now scans each route's owning database, retains original
entry state after same-ID recovery, repoints recovered children, preserves routes
on recovery errors, and defers deletions until the scan completes. A database scan
error keeps pending deletions and does not request persistence. Two real SQLite
tests exercise these decisions, including separate profile stores. The owning
session store must invoke pruning after load and persist when it returns true;
startup wiring remains pending. Logs: `/tmp/hermes-stale-prune-workspace.log` and
`/tmp/hermes-stale-prune-clippy.log`.
Gemini's orchestration review is complete, with follow-up corrections recorded in
`analysis/recovery-orchestration-review.md`. Next: query-only recovery, then the
owning store's per-key DB resolution and phased startup/inbound integration.

Previous recovery checkpoint: **1,325 workspace tests passed, two ignored**.
All 65 Gemini peer-recorder cases now compare Rust and actual Python SQLite
results, errors and durable rows. The recorder resolves ownership lazily for
missing-row repair; the oracle can inject the same owner values as Python.
RoutingIndex::recover_from_db now composes per-key database lookup, legacy Slack
claims, workspace/profile guards, recovered entries, reset promotion and reopen.
Two real SQLite integration tests cover migration, one-time claims, active-process
protection, durable reset boundaries and lookup errors. Clippy passes with warnings
denied. Logs: `/tmp/hermes-recovery-orchestration-workspace.log` and
`/tmp/hermes-recovery-orchestration-clippy.log`.
Next: consume Gemini's `review-recovery-orchestration.txt` review, implement the
query-only recovery path and per-key stale-route pruning, then wire the owning
session store into inbound runtime. This module is not yet the gateway's live
session lifecycle. No full-port percentage was re-audited at this checkpoint.

All 70 Gemini-generated lifecycle cases now run against Rust SQLite, comparing
results, failures, session rows and generation counters after 90 operations.
Clippy passes with warnings denied. Logs:
`/tmp/hermes-lifecycle-oracle-workspace.log` and
`/tmp/hermes-lifecycle-oracle-clippy.log`. Gemini is generating peer-recorder
fixtures via `tools/tasks/gen-session-peer-record-oracle.txt`. Next implement
record_gateway_session_peer (including self-heal and compression ancestry), then
use it in legacy Slack recovery and stale-route orchestration.

Recovered entries now derive naive local created/updated timestamps from durable
recency, with epoch fallback for invalid starts and source-compatible activity
flags. 216 actual Python comparisons pass across UTC and UTC+8. Clippy passes
with warnings denied. Logs: `/tmp/hermes-recovered-entry-workspace.log` and
`/tmp/hermes-recovered-entry-clippy.log`. Gemini completed 70 lifecycle fixtures
in `tools/session-lifecycle-goldens.json`; regeneration and the Rust comparisons
pass. Recovery and per-key stale-route pruning remain pending.

SessionDb implements end_session, reopen_session and reset promotion. Boundaries
and conversation-generation increments commit together; first closure wins,
accidental closures remain promotable, and reopen stamps legacy reset children
before clearing the parent reason. Real database regressions verify rollback,
lineage and generation retention after history deletion. Clippy passes with
warnings denied. Logs: `/tmp/hermes-lifecycle-workspace.log` and
`/tmp/hermes-lifecycle-clippy.log`. Gemini is generating broader lifecycle source
fixtures via `tools/tasks/gen-session-lifecycle-oracle.txt`; those comparisons
are not yet validated. Next: review/consume fixtures, build recovered entries,
then wire per-key stale-route orchestration.

SessionDb now implements exact-key and peer-tuple recovery selection, including
message-bearing ranking, durable recency, recoverable closures and strict reset
boundaries. Peer fallback derives profile ownership from the database path.
Gemini produced 112 actual Python/SQLite cases; all pass against Rust, along with
two direct database regressions. Logs: `/tmp/hermes-peer-workspace.log` and
`/tmp/hermes-peer-clippy.log`. Next: lifecycle writes (reopen/reset promotion),
recovered-entry construction and per-key stale-route orchestration.

SessionDb now migrates the lifecycle columns needed by recovery and creates the
system_prompts table. get_session reads durable rows with Python's resolved
system-prompt projection. Real database tests verify existing-row/history/extra-
column preservation across reopen, missing rows, lifecycle metadata and prompt
fallback. Clippy passes with warnings denied. Logs:
`/tmp/hermes-recovery-schema-workspace.log` and
`/tmp/hermes-recovery-schema-clippy.log`. Next: find_latest_gateway_session_for_peer
selection, reset boundaries and per-profile ownership, then lifecycle writes.

Recovery guards now match Python profile namespaces and Slack origin scope,
including explicit-null versus absent scope aliases. 128 profile and 198
workspace comparisons pass, along with Clippy with warnings denied. Logs:
`/tmp/hermes-recovery-isolation-workspace.log` and
`/tmp/hermes-recovery-isolation-clippy.log`. Guards await recovery-query wiring.
Gemini finished `analysis/session-recovery-map.md`; treat it as a map to verify
against source, especially per-key profile DB ownership and recovered display
names (Python uses source.chat_name). Gemini is generating actual SQLite peer
selection fixtures via `tools/tasks/gen-session-peer-oracle.txt`. The required
initial lifecycle schema and peer queries are present; lifecycle writes and
store orchestration remain unfinished.

Gemini's routing audit reproduced two missing-load boundary problems in
save_entry: cold candidate updates disappeared, and failed upserts after outage
recovery could delete database-only routes during full replacement. save_entry
now loads/reconciles first. Two real SQLite regressions failed before the fix and
pass afterward. Clippy passes with warnings denied. Logs:
`/tmp/hermes-routing-audit-workspace.log` and `/tmp/hermes-routing-audit-clippy.log`.
The reported double revision allocation is source behavior and was retained.
Gemini's recovery dependency map is available in `analysis/session-recovery-map.md`.

Recovery baseline comparison now uses Python numeric/container equality,
including bool/int/float equivalence without rounding large integers to f64.
The 37 recovery oracle cases reproduce and cover this fix. Workspace tests
pass; logs: `/tmp/hermes-routing-equality-workspace.log` and
`/tmp/hermes-routing-equality-clippy.log`.

Gemini audit task: `rust/tools/tasks/review-session-routing.txt`. Initial auth
timeout was caused by this agent host's private D-Bus Secret Service. Dgnrt uses
the desktop bus at `$XDG_RUNTIME_DIR/bus`, served by GNOME keyring. The wrapper
now selects that bus when its socket exists and applies dgnrt's `BROWSER=true`
and closed stdin. The retry authenticated and completed the audit in
`rust/analysis/session-routing-review.md`. Keep the single-flight
lock, Gemini 3.8 Flash high model and permission-skipping flag intact.

Database open now heals legacy routing primary keys within one transaction,
including a missing scope column. It preserves timestamps, normalizes null scope
on rebuild and keeps the newest duplicate mapping. Tests cover reopening,
cross-profile keys, failed-copy rollback and three actual Python schema repairs.
Clippy passes with warnings denied. Logs:
`/tmp/hermes-routing-migration-workspace.log` and
`/tmp/hermes-routing-migration-clippy.log`.

Routing persistence now shares one revision sequence across full snapshots and
entry upserts. The separate writer lock skips superseded writes and folds newer
entry records into delayed snapshots. Full writes maintain the legacy mirror
when enabled or when SQLite fails; a failed mirror does not undo a successful
database commit. Candidate metadata is preserved through fallback before live
publication. Four new inline tests include 64 Python writer decisions exercised
against real SQLite/filesystem failures. Clippy passes with warnings denied.
Logs: `/tmp/hermes-routing-writer-workspace.log` and
`/tmp/hermes-routing-writer-clippy.log`. Stale-route pruning, reset lifecycle,
full state schema migration and inbound integration remain pending.

RoutingIndex now loads real SQLite rows first, imports missing legacy entries,
and reconciles recovered primary rows against an outage baseline. Four inline
tests cover source decisions (25 Python cases), invalid-entry isolation, local
edits/deletions/creations and actual SQLite failure/recovery. Clippy passes with
warnings denied. Logs: `/tmp/hermes-routing-recovery-workspace.log` and
`/tmp/hermes-routing-recovery-clippy.log`. Stale-route pruning and inbound
lifecycle integration are next; the loader alone does not
make the gateway session store complete.

The database now exposes scoped routing load/upsert/replace/delete operations
using Python's current `gateway_routing` schema. Full replacement and batch
deletion use SQLite transactions. Real database tests cover reopen, scope
isolation, rollback after a partial replacement and ten Python SQLite
transitions. Clippy passes with warnings denied. Logs:
`/tmp/hermes-routing-db-workspace.log` and `/tmp/hermes-routing-db-clippy.log`.
Full state schema migration remains unfinished; these methods are not yet called by inbound
routing.

Latest slice adds the persisted SessionEntry codec with inline tests and 124
comparisons against the Python implementation. It preserves timestamp
microseconds and UTC offsets, reset/resume state, metadata and sanitized model
overrides. The shared override sanitizer now uses Python spelling for non-string
values. This codec is used by the routing-index loader. Plugin platform
resolution and malformed origin field coercions remain outside this slice.
Validation: workspace tests, Clippy with warnings denied, formatting and all
four resumed-work fixture generators. Logs: `/tmp/hermes-entry-workspace.log`
and `/tmp/hermes-entry-clippy.log`. See
[session entry evidence](analysis/session-entry-verification.md).

The Slack wake resolver now combines sent/mentioned markers, an explicit active
session probe, fetched root authorship and raw parent mentions in source order.
96 Python comparisons plus local HTTP tests verify decisions, probe short
circuits, fetched roots and memory reuse. Inbound routing still awaits the real
SessionStore and authorization hooks; these tests do not establish full gateway
parity. Logs: `/tmp/hermes-slack-wake-workspace.log` and
`/tmp/hermes-slack-wake-clippy.log`.

The user explicitly asked to continue after the rest break. Preserve all tracked and
untracked work; this continuation has not been committed or pushed.

Latest slice adds the process-liveness reset guard: registry errors preserve a
session, absent probes do not pin it, and stale processes stop pinning at the
exact configured age threshold. The threshold comes from the default reset
policy. The OS/process-registry refresh and SessionStore integration remain
unfinished. 100 source process-age cases and 13 source configuration cases
supplement the 283 reset-decision cases; tests stay inline in session_reset.rs.

Checkpoint validation: **1,287 workspace tests passed, two ignored**. Fixture
regeneration, formatting and whitespace checks pass. Logs are
`/tmp/hermes-pause-checkpoint-tests.log` and
`/tmp/hermes-pause-checkpoint-clippy.log`.

Resume with SessionStore entry/key/process integration and the real authorization
predicate, then wire Slack's existing marker/cache/fetch chain into inbound wake
decisions. The broader gateway, tool runtime/RPC, state/search and native core
scope remains intact and unfinished. The continuation history below is retained.

## Active continuation: 2026-09-06, Codex resumed after Claude

The user explicitly resumed Codex. The paused handoff below is historical.
Reviewed Claude's latest session (`f4982272-287e-4cc9-afaf-1d23c409bb1f` under
the project's Claude transcripts) and commits through `f1859c87cd`. Claude
committed the previous work and added credential selection/store assembly plus
STT construction, startup installation and Dispatcher consumption. The working
tree was clean when this continuation began.

Current continuation adds Telegram voice/audio downloads into `Message.audio_paths`:
`getFile` -> bounded HTTP body -> profile audio cache -> Dispatcher STT -> agent.
`audio_process.rs` owns shared audio extension sniffing and cache writes, using
the existing current/legacy profile directory resolver. `main.rs` configures the
adapter's cache home and `gateway.max_inbound_media_bytes` limit. A download
failure preserves the caption and adds a non-secret failure note.

Verification: 981 actual Python sniffer comparisons plus a local HTTP test that
passes downloaded voice/audio bytes through real transcription HTTP and the
Dispatcher queue into a recording agent. It also covers a populated legacy
cache, invalid download paths and oversized bodies. No real Telegram or STT
credentials were used. Detailed evidence: [Telegram audio](analysis/telegram-audio-verification.md).
Current validation: **1,292 workspace tests passed, two ignored**; Clippy with
warnings denied, formatting, fixture regeneration and diff whitespace checks pass.

Discord now also accepts audio-only Gateway messages and downloads matching
attachments into `audio_paths`, preserving attachment order and bot filtering.
Telegram and Discord share bounded response reading and container-aware cache
writes. Discord's media client sends no bot authorization and rejects redirects;
the current native path accepts Discord's CDN hosts only. Inline tests exercise
the Gateway read loop, multiple attachments, chunked size enforcement and caption
preservation after download failures. Sixteen classifications execute the actual
Python attachment branch. See [Discord audio evidence](analysis/discord-audio-verification.md).

Slack's complete-file audio path now also feeds `audio_paths`: file_share and
audio-only events, video-labeled voice markers, authenticated private downloads,
profile caching and startup configuration. HTTPS host checks and validated DNS
pinning precede bot-token downloads. HTML and redirected responses are rejected.
216 Python classification cases plus local HTTP authentication/cache checks
verify the implemented boundary. See [Slack audio evidence](analysis/slack-audio-verification.md).

Slack Connect `check_file_info` stubs now resolve through authenticated files.info
before media classification. Complete entries bypass lookup, missing IDs are
skipped, and bot events are filtered before network work. Failed metadata lookups
preserve the caption with a generic notice. The source's standalone file_shared
fallback is video-only and uses the same share-timestamp deduplication cache;
it does not provide a general audio lifecycle handler.

Slack now reuses the shared MessageDeduplicator on the adapter instance, before
metadata lookup/download. Keys preserve Python's event/body/authorization
workspace precedence and timestamp scoping; missing timestamps are not claimed.
The default TTL is one hour, with the existing SLACK_DEDUP_TTL_SECONDS override.
Thirty Python key-resolution cases plus concurrent claim and repeated metadata
lookup tests cover this wiring. The normal-message dedup cache survives reconnects.

Slack normal file-share events now preserve genuine videos in `Message.video_paths`.
The private-download safeguards are shared with audio, while video bytes retain
supported video suffixes and use the current/legacy video cache. Dispatcher adds
existing Python-compatible video path notes after slash-command handling. Voice
clips labeled as video remain on the audio/STT path. Validation includes 144
Python video classification cases, local HTTP cache checks and Dispatcher tests.
This exposes the cached path, not video analysis capability; native media tools
and remote-agent path translation remain unfinished.

Slack's video-only `file_shared` fallback now resolves metadata, chooses the
channel share timestamp, waits 750 ms, then enters the shared message path.
Socket Mode polls handlers alongside incoming frames, allowing normal shares to
win deduplication during the grace period. Acknowledged handlers finish before
socket reconnects. A local WebSocket/HTTP test covers prompt acknowledgement,
normal-share precedence, lifecycle-only delivery, repeat suppression and socket
closure with pending work. Native thread routing remains incomplete.

Slack startup now combines comma-separated bot tokens with the active profile's
`slack_tokens.json`, authenticates them through `auth.test`, and publishes a
complete workspace token map. Metadata uses event workspace identity; downloads
prefer that identity, then the private-file URL workspace, then the primary token.
Replies carry the original workspace. History, concurrency leases and model cache
keys now share workspace-scoped message identity, avoiding cross-workspace channel
collisions. Old messages without workspace metadata retain their original IDs.
This scoped native key is not yet Python's full session-key format, and existing
unscoped Slack history is not migrated automatically. Full bot identity filtering remains a gap.
A local HTTP/SQLite test covers credential selection, saved tokens, replies,
history isolation and failed reauthentication without partial map replacement.

Slack now remembers channel ownership and infers missing workspace identity only
while a channel has one known owner. Conflicting observations remove that route;
explicit workspace metadata still wins. Both observation and route caches follow
Python's independent oldest-first eviction down to half capacity (10,000 cap).
Normal messages retain Python's dedup-before-inference order; lifecycle shares
claim their original workspace identity separately before normal preparation.
Fifteen transitions executed from Python verify ambiguity and eviction, and an
inline runtime test verifies inference, explicit routing and redelivery behavior.

Slack ignored-channel policy now gates normal messages, lifecycle metadata and
outbound sends. It matches parent channels for thread-shaped IDs, supports `*`,
and preserves precedence: platform extras, nonempty legacy environment override,
then the top-level `slack.ignored_channels` YAML setting. An empty explicit extras
list disables the blacklist. 192 cases execute the Python policy, with additional
YAML precedence assertions and local HTTP request-count checks proving ignored
channels trigger neither lookup/download nor send requests. Configuration is
resolved when the adapter is constructed/configured, not reread on every event.

Slack now retains the bot user ID returned by each workspace's `auth.test`.
Self-authored messages without `bot_id` are rejected before file metadata lookup,
using explicit or inferred workspace identity. Token and identity maps publish
together; the first token's primary bot identity survives duplicate-workspace
entries, and failed refreshes preserve the previous complete registry. Existing
HTTP tests now cover self-message suppression without requests, inferred identity,
failed refresh and duplicate-workspace primary identity. The test count remains
1,262 because these checks extend the existing workspace integration test.

Declared Slack bot/app messages now use `allow_bots` (`none`, `mentions`, `all`)
with extras/environment/YAML precedence and the `api_human_users` exception for
user-token API posts. Mention gates inspect flat text and authored Block Kit user
elements, excluding rich-text quotes. Self messages remain rejected in every mode.
48 Python cases verify declaration and mention detection; an inline runtime test
covers all three modes and self rejection. Events without a user identity and
full Block Kit body rendering remain incomplete, as does unmarked-bot detection
through `users.info`. These are not yet full Slack bot-policy parity.

Slack now resolves unmarked senders through authenticated `users.info`, scoped
by workspace and cached for both bot/human results. It recognizes `is_bot`,
`is_workflow_bot` and profile bot IDs. Lookup failures retain Python's false
classification; successful insertions trigger oldest-first trimming at 5,000.
Resolved bots use the existing permission/mention gates, including the extra
primary-bot gate for events without `client_msg_id`. A local HTTP test verifies
token routing, cache reuse, workspace separation, failures and turn admission.
User display-name cache population from the same response remains unported.

Non-command Slack messages now include Python's filtered Block Kit UI payload.
The cohesive `slack_blocks.rs` module retains the source field allowlists and
Unicode character limit, excludes rich-text blocks from this serialized view,
and removes unrelated URL/value fields. Section/button-only messages now reach
the model even when flat text is empty. Slash-command text remains unchanged.
35 Python serializer cases and an inline message-path test cover this slice.
Authored rich-text rendering, semantic deduplication, bang-command rewriting and
legacy attachment text are still separate gaps; this is not full Block Kit parity.

Slack authored rich-text rendering now joins the live non-command message path.
Quotes, lists, code, links and inline Slack entities render through slack_blocks.rs.
Python-style normalization removes mirrored flat text without dropping forwarded
content; fenced code uses whole-fence equality rather than substring matching.
396 Python merge cases cover rendering and normalization, and the runtime test
covers flat-text deduplication plus forwarded and block-only content. This does
not finish legacy attachment text, bang commands or full channel/mention policy.

Slack live attachment previews now append titles/links, preview or fallback text,
and footers. Message unfurls are skipped, preview bodies use Python's 500-character
limit, and only a complete rendered section suppresses duplication. A URL already
in the user's text does not hide its preview. Forty-eight cases execute the actual
Python live branch, and the inline adapter test covers preview-only messages and
URLs accompanied by useful previews. The distinct thread-history attachment
renderer (structured fields and nested blocks) is still unported.

Slack now rewrites known leading bang commands to slash form before Block Kit
merging. Unknown exclamations remain unchanged, and rewritten arguments do not
pick up mirrored blocks. The generated command catalog contains all 101 Python
registry definitions, preserving names, aliases and gateway/config-gate metadata.
261 cases execute the Python rewrite against that actual registry; the runtime
test checks !help reaches native help recognition. This adds command recognition,
not the missing native handlers. Plugin command registration remains unported.
Regenerate/check with `mise exec python@3.12.13 -- python
rust/tools/gen_command_catalog.py --check` alongside the media fixture generator.

Slack media downloads now follow redirects with per-hop destination validation
and DNS pinning. The initial URL remains restricted to HTTPS Slack CDN hosts;
public redirected origins receive no bot authorization after an origin change,
even if the chain later returns to the original host. The chain stops after 20
redirects. Final responses still pass HTML rejection, size limits and cache writes.
A local HTTP test verifies relative redirects, credential retention/removal,
return-to-origin behavior, private/userinfo rejection, loop bounds and cached bytes.
It overrides DNS only for fixture hosts; production DNS/TLS is not live-tested.
HTTPX cookie-jar behavior and source retry policy remain unported.

Slack media retries now wrap redirect/download/cache processing. Request/read
timeouts and HTTP status errors from 429 upward get at most three attempts, with
1.5/3-second backoff. Other access failures, HTML responses and local cache errors
do not retry. Status is checked before HTML so HTML-formatted 503/429 responses
follow the source retry path. A local HTTP test covers eventual success, backoff,
timeout exhaustion, and immediate 403/HTML failures. Cookies remain unported.

Slack media downloads now keep a cookie jar for one attachment, shared across
redirects and retries and discarded afterward. Each request selects cookies for
its destination URL; Set-Cookie is processed on redirects and error responses too.
The per-hop DNS-pinned clients and authorization-origin rules remain in force.
Reqwest's cookies feature adds locked cookie dependencies without upgrading
existing packages. A local HTTP test covers redirect/path/host/Secure rules,
fresh-download isolation and cookies retained through a retryable 503 response.
This verifies the native cookie path, not every Python CookieJar edge case.

Slack channel classification now infers one-to-one DMs from D-prefixed channel
IDs when channel_type is absent/falsy, and treats mpim as DM scope. The DM-disable
switch blocks im and mpim; only im bypasses the allowed-channel list. Shared
surfaces use that list once bot identity is known, following Python. Config
precedence matches extras, legacy environment and the Slack YAML bridge.
Forty Python policy cases plus runtime/HTTP checks cover classification, group-DM
controls and normal-message rejection before user/file lookups. Full mention gates,
early sender authorization and thread/session reply routing remain unported.

Slack messages now retain original message_id and selected thread_id through
Dispatcher replies. Thread-aware history/lease/model-cache keys isolate roots
within a workspace/channel. Top-level messages use source defaults; legacy shared
DM sessions and flat channel replies follow the extras settings, while existing
threads keep their root. chat.postMessage receives the selected reply thread.
144 Python routing cases, temporary SQLite history tests, HTTP reply-body checks
and Dispatcher propagation assertions verify this slice. Full Python session-key
format/migration, assistant-thread metadata and backfill remain incomplete.
Also corrected command enrichment: final slash/bang command text stays original,
as Python's MessageEvent construction does, even when attachments are present.

Addressed Slack commands now re-run command recognition after stripping the
workspace bot's mention. `@bot !help` and `@bot /help` keep canonical arguments
without appending quoted blocks or attachment previews. Unknown bang phrases
remain conversation text. The generated command catalog has a narrow `.gitignore`
exception so fresh checkouts include this required compile-time input.

Slack now honors `ignore_other_user_mentions` before file enrichment. A leading
mention of another user suppresses shared-channel/MPIM turns unless this bot is
also mentioned or a configured regex wake word matches. One-to-one DMs bypass
this gate. Routing text includes authored block mentions but excludes quoted
mentions. Labeled bot mentions count as an exception. Extras/environment parsing
and the top-level YAML boolean bridge match the source's whitespace/coercion
rules. Regex patterns reuse the existing shared compiler; arbitrary Python regex
syntax and Unicode semantics are not yet exhaustively verified. 48 message
shapes and 44 settings execute actual Python helpers, with adapter-path checks
for rejection, exceptions and declared-versus-resolved bot behavior.

Slack's unconditional strict/thread mention rejection rules now run before file
enrichment. Free-response channels bypass strict top-level gating, but thread
replies still obey `thread_require_mention`; require-mention channel overrides
cancel the free-response exception. Explicit mentions and internal forced turns
bypass this rejection, and one-to-one DMs remain exempt. 256 combinations execute
the actual Python branch up to its asynchronous wake check, with inline adapter
tests. Passing this rejection predicate does not establish permission to wake:
the default require-mention path still needs its session/history wake checks.

Slack now records workspace-scoped mentioned-thread and sent-message markers.
Incoming mentions record the selected root only after current gates pass and
only outside strict/thread-required mode. Successful sends record the returned
timestamp plus the outgoing root. Both sets use Python's separate cap/eviction
rules and decimal timestamp ordering. 21 actual-source transitions and adapter
assertions cover recording and eviction. These are in-memory inputs for the
forthcoming wake resolver, not restart persistence or completed wake gating.

The thread-history renderer now handles structured legacy attachments, nested
rich text, section/header/context text, ordered block URLs and compact file
markers, verified against 480 actual Python renderings. A cold parent-fetch
helper uses workspace-routed conversations.replies and validates the root ID;
local HTTP checks cover successful rendering and failure results. These helpers
are ahead of the wake resolver, not yet called by inbound runtime routing.
The source cold-fetch renderer strips bot mentions even when its caller requests
preservation; the raw-message cache path differs and still needs porting.
The current Rust session registry also lacks the source reset-aware active-session
predicate, so simply finding history is insufficient evidence to wake a thread.

The thread-block formatter now matches 108 actual Python cases, including
watermark deltas, current-message exclusion, parent-text capture, workspace-owned
assistant reply labels and unverified-sender headers. Names and non-assistant
bodies use the existing inline neutralizer. It accepts resolved names and
authorization results; the adapter still needs to supply those through the
pending thread-context fetch/cache path. This helper is not inbound integration.

The adapter's thread fetch/cache now calls the formatter with resolved display
names and caller-supplied authorization results. It stores full context plus raw
messages, serves watermark deltas without another replies request, and supports
expiry, forced refresh and rate-limit retry. Warm parent lookup now returns raw
or rendered text as requested. Name and bot caches share users.info responses.
Local HTTP tests cover this chain. Inbound routing still does not call it; the
runner's real authorization predicate and reset-aware session check are pending.

Reset-policy selection and the pure SessionStore reset predicate now exist.
They reuse the existing GatewayConfig/SessionResetPolicy types: platform policy
overrides type policy, active processes suppress reset, idle checks precede daily
checks, and comparisons preserve the source's exact deadline boundaries. 283
actual Python cases include invalid values and datetime overflow. This is a
dependency for active-session lookup; session entries and process-liveness wiring
remain pending. Details: [reset policy evidence](analysis/session-reset-verification.md).

Slack's store-lookup key helper now reuses the shared session-key builder and
store isolation settings, verified against 288 Python keys. Profile selection
uses stamped source, adapter owner, then store resolution, with invalid candidates
ignored. The key comparisons exposed and fixed shared empty-identifier handling:
empty threads no longer append colons or suppress per-user isolation, and empty
alternate user IDs fall back to the real ID. These helpers remain ahead of the
SessionStore integration; they do not migrate existing native history keys.

Next: Slack default require-mention wake resolution and active-session checks
and thread context/backfill, then remaining
rich-media routing and vision. Telegram's broader adapter parity still includes
group/mention policy, metadata-specific admission limits and exact failure UX,
thread routing, pending-event handling, documents/video and other rich features.
The credential seeding/OAuth/lease gaps Claude recorded still apply. These new
changes are uncommitted at this checkpoint; preserve them when continuing.

## Takeover handoff: 2026-09-06, paused at the user's request

The user requested a wrap-up so another agent can continue before Codex reaches
its weekly quota. The full port is unfinished. Resume from the current working
tree, including untracked sources, generators and fixtures. Do not reset it.

**Overall estimate: about 28%, roughly 25-35%.** This is a subjective estimate of
full native replacement, not a percentage of files, tests or lines translated.

| Phase | Scope weight | Rough completion |
| --- | ---: | ---: |
| Gateway | 35% | 45% |
| Tool runtime / RPC | 30% | 5% |
| State / search | 15% | 35% |
| Native agent core | 20% | 25% |

Weighted result: 27.5%, rounded to 28%. Recent credential, transcription and
request-policy work advances core foundations; much still needs runtime wiring.
The earlier audit estimated core at 20% and overall at 27%. This adjustment is
judgment, not a measured productivity gain. Do not inflate progress from the
large differential fixture corpus. See the [scope audit](analysis/progress-audit-2026-09-06.md).

### Exact checkpoint

- Branch: `rust-rewrite`. Latest commit: `75aad17d8e` (after `55633f2f76` and
  `2a48a28cfb`). Those were the previous commit/push checkpoint. Later work,
  including this handoff, is uncommitted. No new commit or push was made during
  wrap-up. Inspect `git status --short`, including `??` files, before staging.
- Final validation: **1,238 workspace tests passed, two ignored by default**
  (one passing core test and 1,237 passing gateway tests). Clippy with warnings
  denied passed. The ignored tests are the Python bridge and optional FFmpeg
  integration; the FFmpeg integration passed earlier, not rerun for this header
  change. Logs: `/tmp/hermes-handoff-workspace.log` and
  `/tmp/hermes-handoff-clippy.log`. Temporary logs are not durable evidence.
- Latest completed slice: `custom_provider_config.rs` normalizes saved providers,
  resolves named entries, and selects custom headers by the effective URL.
  `main.rs` consumes request-body overrides, provider output limits and headers;
  `native_agent.rs` applies headers to streaming and tool requests.
- Latest source comparisons: 297 compatibility cases, 221 named-provider cases,
  301 route identities and 32 header selections. Real HTTP tests cover matching
  headers (four cases), endpoint-override isolation (two), and output limits
  (twelve). Fixtures come from `gen_custom_provider_config_goldens.py`.
- Other substantial uncommitted work includes streaming/final-text cleanup,
  native transcription HTTP/audio processing, scoped secret selection, auth-store
  reads, credential entry/persistence rules, source seeding and cooldowns. Read
  [auth verification](analysis/auth-store-verification.md),
  [STT plan](analysis/stt-credential-resolution-plan.md), and
  [transcription verification](analysis/transcription-http-verification.md).

### Continuation 2026-09-06 (Claude Opus 4.8, taking over from Codex)

- Validated the entire uncommitted handoff tree (1238 workspace tests, clippy
  -D warnings, fmt, and gen_custom_provider_config_goldens.py --check all pass)
  and COMMITTED it as `c8f6358eb5` so it is no longer at risk. Nothing was reset.
- Ported the CredentialPool API-key SELECTION CORE onto the existing
  PooledCredential model (`829c2cfcf5`): new/has_credentials/has_available/
  next_available_at/current/entries/entry_id_for_api_key/peek/select plus the
  internal _available_entries/_select_unlocked, with fill_first/least_used/
  round_robin/random strategies, sole-credential cooldown, clear-expired reset,
  DEAD manual prune, injected persist+clock. Differentially verified against the
  real AST-extracted CredentialPool via gen_credential_pool_select_goldens.py
  (12 cases). Deferred, documented, and unreachable on the API-key path: store
  hydration, OAuth refresh, the anthropic/nous/codex/xai auth-store sync
  branches, the codex early-reopen probe, and lease ownership.
- Re-enabled agy (Gemini 3.8 Flash) and produced analysis/stt-vision-runner-seam.md.
  As before, agy's structural facts are reliable but its specifics are not:
  spot-check found it fabricated a `runner_vision.rs`, mislocated
  build_native_content_parts, and hedged/invented prepare_inbound_message_text.
  The doc carries a verification banner; treat it as leads, not truth.

UPDATE (later 2026-09-06): the credential vertical below is now built and
verified end to end. Committed:
- `load_pool_from_store` (`da9b10d2b1`): the read/heal/construct flow for
  non-anthropic, non-custom, non-single-use providers (guarded; those need the
  deferred seeding), differentially verified against the real AST-extracted
  load_pool with seeders stubbed identically to the Rust deferral.
- `store_pool_callback` + an end-to-end test (`f56e46ec4c`): AudioCredentials::
  resolve pulls a stored key through provider_secret's pool branch ->
  store_pool_callback -> load_pool_from_store -> peek. The STT credential can now
  be constructed from the profile store.
The one remaining step for a LIVE STT path is calling store_pool_callback at the
actual inbound-audio construction site (build ProfileAudioCredentials with the
runner-resolved profile/root auth.json paths, then hand it to the transcription
backend). That is runner-integration work; verify it with a runner-level test,
and note the deferred pieces still open: env/singleton/custom credential SEEDING
(load_pool only reflects the persisted store), OAuth refresh, and lease ownership.

UPDATE 2 (2026-09-06): the STT pipeline is now wired end to end in the runtime,
verified at each hop:
- `build_openai_transcription` / `build_openai_transcription_at` (`d1034ea7a0`):
  the construction site that calls store_pool_callback with runner-resolved
  auth.json paths; temp-profile tests cover store-hit, config-precedence, and
  the honest unconfigured error.
- Dispatcher enrichment hook + `Message.audio_paths` (`7c7f5ad4b0`): handle_turn
  transcribes attached audio (via the ported enrich_message_with_transcription)
  before the turn; dispatcher tests prove the turn runs on the transcript, and
  that a missing backend falls back to the caption.
- `build_gateway_transcription` + startup install (`6d3bf9acf5`): composes the
  real HttpTranscriptionBackend (credential pool + audio_process duration +
  file-read policy + gateway context) and installs it on the Dispatcher when STT
  is configured. resolve_openai_stt_model ports DEFAULT_STT_MODEL, unit-tested.

THE ONLY REMAINING PIECE for a live STT turn is per-adapter voice-note DOWNLOAD
populating Message.audio_paths (telegram/discord/slack). That needs each
platform's file-fetch plus an audio cache-write primitive (the base.py
cache_audio_from_bytes seam, still deferred). The Dispatcher hook already
consumes audio_paths, so once an adapter fills it, transcription runs. Still
deferred across the credential layer: env/singleton/custom credential SEEDING
(load_pool_from_store reflects only the persisted store), OAuth refresh, lease
ownership, and the Nous managed-audio gateway.

**Earlier next seam (now done): `load_pool(provider)` for the non-OAuth, non-custom path.**
The building blocks exist (auth_store::read_pool, credential_sources::seed_from_env,
credential_pool::{read_stored_entries, prune_stale_sources, normalize_priorities},
credential_persistence). Assemble them in load_pool's exact control flow
(read -> from_dict -> seed_from_env -> prune_stale_seeded -> normalize_priorities
-> persist-if-changed -> CredentialPool::new), then wire the result into
tool_credentials::provider_secret's `pool` callback and construct real STT
credentials. CAVEAT for the verifier: load_pool is I/O-driven and its Python
import graph needs yaml/httpx (won't import for a subprocess oracle), so verify
with a temp HERMES_HOME + written auth.json integration test and/or an
AST-extraction that stubs ONLY read_credential_pool/persist/env, never the
selection or seeding logic. _seed_from_singletons is a no-op for pure API-key
providers (anthropic/codex/xai/nous own the singletons); a general load_pool
must still port it.

### What the next agent should do

1. Read this handoff, the analysis index and relevant verification documents,
   then inspect current code and commits. Older sections below contain historic
   qualifications; current code and this checkpoint take precedence.
2. Continue full named-provider runtime resolution in
   `hermes_cli/runtime_provider.py::_resolve_named_custom_runtime`: endpoint and
   explicit model/key precedence, complete auth-provider canonicalization,
   credential pool selection, command-issued tokens and API transport handling.
   The named getter is ported; it is not the complete runtime resolver. Native
   HTTP tests currently supply an explicit endpoint and key. Only native
   Chat Completions is implemented; other transports remain work.
3. Finish credential pool construction around the existing entry/source helpers:
   singleton OAuth sources, provider-specific endpoint hooks, availability and
   current-key selection, refresh, locking and persistence. Do not replace this
   with "take the first stored key". Then construct real STT credentials from
   the profile and connect transcription/vision to the gateway runner.
4. Integrate rich inbound media, pending messages, session/image routing and
   live capability resolution into Dispatcher and platform adapters. Tested
   helpers with injected effects are not a completed gateway pipeline.
5. Follow the full phase plan: remaining gateways/platforms and commands, native
   terminal/file/browser tools, discovery/MCP/plugins/RPC, state parity, then
   remaining prompt/memory/skills/compression/provider/delegation core work.
   Startup still registers only `CurrentTimeTool`; the three live messaging
   adapters are Telegram, Discord and Slack. Frontends remain TypeScript.

### Working rules and method

- Preserve the Python reference. Port its actual behavior, including coercion,
  precedence, side effects and failure paths. Read source and relevant commit
  intent before changing a contract. When context is missing, search transcripts
  under `~/.claude/projects/-home-eins0fx-development-hermes-agent-port` selectively.
- Keep cohesive Rust modules and useful comments explaining the contract or
  non-obvious behavior. Tests stay inline in `#[cfg(test)] mod tests`; do not
  create sibling `*_parity.rs` or `*_test.rs` files. Fixtures and Python generators
  belong in `rust/tools/`. No em dashes or stock AI prose in comments/docs.
- Generate differential cases by executing actual Python source functions or
  extracted AST bodies. Stub only explicit I/O dependencies and record call
  order when it matters. Do not rewrite the Python algorithm as the oracle or
  weaken a failing case to make Rust pass. State malformed-input limitations.
- Follow pure comparisons with actual consumers: temporary profile files,
  SQLite or local HTTP servers as applicable. Check requests, execution effects
  and user-visible results. A helper existing or compiling is not integration.
- Protect byte-stable conversation prefixes, tool schemas and replay history.
  Apply wire-only cleanup to fresh copies. Do not introduce mid-turn system
  prompt changes or use synthetic messages outside the reference's rules.
- Use synthetic credentials and temporary `HERMES_HOME`. Never read or print
  real secrets for fixtures. Preserve scoped-secret boundaries and borrowed-key
  sanitization. Use the transaction skill for new locking/transaction work.
- Current Rust: stable 1.95.0, edition 2021. Oracle: mise Python 3.12.13. Query
  installed managers again when resuming; do not assume the shell Python version.
- Helpers: user requested Claude Opus 4.8 medium and Gemini 3.8 Flash high via
  `rust/tools/claude.sh` and `rust/tools/agy.sh`. Both wrappers retain explicitly
  authorized permission bypass. Preserve AGY's flock, assign bounded file
  ownership, inspect helper output independently, and do not silently substitute
  models. The coordinating agent owns integration and Cargo validation. See
  [helper instructions](tools/README.md). Recheck quota availability if needed.
- Keep build artifacts, raw helper transcripts and local logs ignored. Keep
  Rust sources, synthetic fixtures, generators, task prompts and Markdown
  evidence. Do not use broad ignore patterns that hide port work.
- Run focused tests while iterating, then workspace tests, Clippy and formatting
  at an integration checkpoint. Avoid concurrent Cargo builds. Tests that mutate
  process environment must share `secret_scope::GLOBAL_TEST_LOCK` and restore it.
  If that lock is poisoned, fix the first failing test rather than its cascade.

```bash
mise exec python@3.12.13 -- python rust/tools/gen_custom_provider_config_goldens.py --check
cargo test --manifest-path rust/Cargo.toml --workspace
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
git diff --check
```

## Detailed implementation inventory

Read [the analysis index](analysis/INDEX.md) first when resuming. Codex owns
integration and validation, with Claude Opus 4.8 (medium) and Gemini 3.8 Flash
(high) as CLI helpers. Both wrappers use the user's requested permission bypass.
Usage and reference-app findings are in [tools/README.md](tools/README.md).

Current full-port scope estimate: **about 28%**, based on the handoff above and
[runtime and scope audit](analysis/progress-audit-2026-09-06.md). This is a weighted
engineering estimate, not test or module coverage.

Commit history confirms two separate levels of completion:

| Capability | State | Evidence |
| --- | --- | --- |
| Runnable gateway, Telegram/Discord/Slack, CLI/native/Python agent backends | Existing runtime paths | `379f8b74e8`, `ea428d0723`, `5db3460dc8`, `8a761a89f4` |
| History and delivery ledger | Wired into Dispatcher | `3afb9d2145`, `232626198d` |
| Full gateway config pipeline | Ported and differential-tested; runner integration remains | `811fcaca55`, config_loader golden corpus |
| Session registry and run generations | Ported and tested; future runner owns consumption | `1a6cfb3546` |
| QQBot onboarding, WhatsApp helpers, shared text helpers | Support modules, not complete platform adapters | `6fc55e3f38`, `b5271716b2` |
| Inbound attachment classification | New Tier 2 slice, compiled and tested; not yet called by Dispatcher | `inbound_media.rs`, 217 Python differential cases plus focused unit tests |
| Media placeholders and document/audio/video notes | Ported; display names and sandbox paths are caller-resolved | `media_context.rs`, 28 placeholder cases, 40 document cases, 4 audio/video input pairs |
| Pending STT and combined transcribe/echo flow | Ported with injectable async operations; real providers remain unwired | `pending_stt.rs`, 27 Python transition steps |
| Pending event merge and caption deduplication | Ported, with STT state bundled alongside the event | `pending_messages.rs`, 166 Python merge cases and executable cache/echo integration tests |
| Native-image buffer consumption | Atomic take, scoped to one session | `session_registry.rs`, simultaneous-consumer and cross-session tests |
| Sandbox cache-path mapping | Ported, including staging creation and legacy layout selection | `cache_paths.rs`, 224 mappings generated with real Python imports |
| Sender and reply context | Ported, preserving prompt placement and shared-session policy | `inbound_text_context.rs`, 144 context cases and 30 metadata-normalization cases |
| Native audio duration | WAV/Opus probing and formatted duration notes wired into HTTP-backed enrichment, with bounded ffprobe fallback | 16 Python formats, 36 WAV headers, 43 Mutagen files and enrichment/probe integration; [verification](analysis/transcription-http-verification.md) |
| Audio upload validation | File-kind, supported-format and size validation wired before native STT upload | 46 Python filesystem cases and HTTP no-upload checks; [verification](analysis/transcription-http-verification.md) |
| Raw tool-provider selection | Saved provider intent parsed without schema defaults and consumed by STT credential resolution | 216 Python cases and real config-file checks; [resolution plan](analysis/stt-credential-resolution-plan.md) |
| STT credential selection | Lazy direct/managed selection and strict endpoint locality connected to HTTP client construction; live credential sources remain | 100 selection cases, 257 locality cases and four HTTP uploads; [resolution plan](analysis/stt-credential-resolution-plan.md) |
| Voice-provider secret lookup | Config/scope/file precedence and plain/custom pool lookup policy connected to STT; pool storage and managed account effects remain | 576 Python cases and real scope/file/consumer checks; [resolution plan](analysis/stt-credential-resolution-plan.md) |
| Auth-store and pool reads | File loading, legacy normalization and profile/root shadowing ported; pool hydration, selection and runtime consumption remain | 324 pool cases, 38 file cases and corruption/I/O checks; [verification](analysis/auth-store-verification.md) |
| Credential cooldown policies | Billing/sole-key TTLs, reset deadlines and retry-message normalization ported; pool availability integration remains | 609 Python cases; [verification](analysis/auth-store-verification.md) |
| Shared ISO timestamp parsing | Compact/calendar/week dates, arbitrary separators, fractional times/offsets consumed by gateway and credential deadline parsing | 1,572 CPython cases; local DST fold/gap handling remains; [verification](analysis/auth-store-verification.md) |
| Credential entry model and disk boundary | Stored entries decode into runtime values and serialize with borrowed secrets removed; live-source rehydration and pool selection remain | 698 dataclass cases, 159 sanitizer cases and real store/cooldown checks; [verification](analysis/auth-store-verification.md) |
| Credential source updates | Fingerprint-aware rehydration, token rotation, duplicate-source removal and disk-change detection implemented; source discovery and pool loader remain | 220 Python update cases and serialized-reference reload checks; [verification](analysis/auth-store-verification.md) |
| Credential source maintenance | Stale-source pruning and Anthropic priority normalization implemented; discovery and pool availability integration remain | 380 Python cases and store/reload pruning checks; [verification](analysis/auth-store-verification.md) |
| Credential environment discovery | Profile-file/scope reads, suppression gates, provider source order and upsert connected; full pool loader remains | 112 seeding cases, 47 helper cases and real profile/reload checks; [verification](analysis/auth-store-verification.md) |
| Custom credential sources | Name/slug/legacy alias matching and custom/model-config seeding implemented; merged config normalization and pool loader remain | 78 Python cases and YAML-to-runtime-entry checks; [verification](analysis/auth-store-verification.md) |
| Merged custom-provider configuration | Keyed/legacy normalization consumed by credential seeding and native request settings; full runtime resolution remains | 297 Python cases, real YAML and streaming/tool HTTP checks; [verification](analysis/auth-store-verification.md) |
| Named custom-provider lookup | Keyed-first lookup, aliases, request-body settings and provider output limits consumed by native startup, including bare saved names; full endpoint/key/transport resolution remains | 221 Python cases, four request-body HTTP checks and twelve output-limit HTTP checks; [verification](analysis/auth-store-verification.md) |
| Custom provider HTTP headers | Effective-route selection consumed by native streaming and tool requests; different endpoint overrides do not inherit saved headers | 301 Python route cases, 32 selections and six HTTP checks; [verification](analysis/auth-store-verification.md) |
| STT language configuration | Provider/global/legacy language precedence connected to transport configuration | 108 Python cases, override checks and four HTTP uploads; [verification](analysis/transcription-http-verification.md) |
| Transcription HTTP transport | Real multipart provider calls connected to the enrichment interface; full runner construction remains | Four model uploads, denied reads, HTTP errors and 15 Python text cases; [verification](analysis/transcription-http-verification.md) |
| Transcription enrichment orchestration | Ported with an explicit provider boundary; live provider implementation remains | `transcription_enrichment.rs`, 38 Python scenarios plus recording-backend tests |
| Vision enrichment and memory-context sanitizer | Ported with an explicit provider boundary; live vision provider remains | `vision_enrichment.rs`, 51 Python scenarios checking output and call order |
| Attachment display names | Ported inside the existing media context module | `media_context.rs`, 14 source-executed cases |
| Image mode and capability overrides | Ported with live capability lookup; runner construction remains | `image_routing.rs`, 392 Python cases including recorded lookup effects |
| Session-aware image routing wrapper | Ported with runtime resolution supplied by the runner | `session_image_routing.rs`, 28 Python cases checking fallback and call order |
| Image references in text | Ported with real filesystem checks | `image_references.rs`, 46 Python cases on temporary files |
| Native image content | Real file reads, guarded loading, base64, and PNG conversion; dispatcher integration and HEIC/AVIF decoding remain | `native_image_content.rs`, 23 byte signatures and 20 real-file/Pillow comparisons |
| File read policy | Ported for the POSIX reference, used by native image loading | `file_read_safety.rs`, 69 Python cases including missing paths and symlinks |
| MIME inference and document fallback | CPython default mappings plus system overlays, wired to native image and document helpers | `mime_types.rs` and `media_context.rs`, 78 MIME and 56 document cases |
| Structured message transport and history | Prepared text/image parts reach the native streaming and tool paths through `/message`, survive SQLite replay, and are rejected by unsupported backends | Inline core, storage, native-agent and HTTP tests; [verification](analysis/structured-content-verification.md) |
| Inference endpoint resolution | Ported inside image_routing.rs with explicit turn context; consumed by live capability lookup | 1,164 Python cases, inline tests, [lookup plan](analysis/live-capability-plan.md) |
| Local server detection and Ollama vision probes | Real HTTP and memory/disk cache implemented; prefix recognition implemented; discovery and runner integration remain | 42 source-derived cases plus inline HTTP/cache tests; [verification](analysis/local-probe-verification.md) |
| Endpoint locality and Ollama fallback | Locality gate connected to URL/key resolution and real probes; managed/catalog stages connected through live lookup | 253 Python cases and inline HTTP integration; [verification](analysis/endpoint-locality-verification.md) |
| Managed local vision capability | Staged files, live state/props, and projector fallback implemented; packaged catalog available, caller supplies shared root | 50 source-derived cases and real HTTP tests; [verification](analysis/managed-capability-verification.md) |
| Managed curated catalog | Embedded shared catalog, constructor coercions, explicit background/forced refresh and failure retention | 43 Python loader cases and real HTTP/filesystem tests; [verification](analysis/managed-catalog-verification.md) |
| Cloud registry cache | Memory/disk/network cache, stale background refresh, ETag and failure backoff implemented; capability/context lookup available | 60 Python cases and inline HTTP/concurrency tests; [verification](analysis/cloud-catalog-verification.md) |
| Cloud capability and context metadata | Provider map, override/default selection, model matching and vision catalog stage implemented | 683 Python cases, HTTP tests and insertion-order collision regression; [verification](analysis/cloud-metadata-verification.md) |
| Provider registration and live vision lookup | Registration/aliases, prefix recognition and combined lookup implemented; discovery and runner construction remain | Six registration transitions, 265 Python prefix cases, full HTTP-stage tests; [verification](analysis/provider-registry-verification.md) |
| Base provider model-list hook | Native endpoint selection, model-list HTTP and credential-safe redirects implemented; per-provider TLS contexts and overrides remain | 49 Python fetch and 12 hostname cases plus real redirect/header tests; [verification](analysis/provider-fetch-verification.md) |
| Provider model-list CA bundles | Environment precedence, full PEM bundles, default fallback and custom trust store implemented | Real local HTTPS and public environment-to-fetch tests; [verification](analysis/provider-tls-verification.md) |
| Bundled base profiles and native startup | 17 profiles across 13 modules loaded natively; selected endpoints, headers and declared credentials wired into startup | Source regeneration, real streaming/tool HTTP requests and saved-key rotation; [verification](analysis/bundled-base-profiles-verification.md) |
| Upstage provider hook and reasoning config | Native Solar profile, reasoning hook, shared clamping and per-model config resolution wired into both request paths | 561 Python comparisons and 12 real HTTP requests; [verification](analysis/upstage-verification.md) |
| Nebius Token Factory | Native profile, model allowlist and request reasoning hook wired into startup | 812 Python cases and 12 real HTTP requests; [verification](analysis/nebius-verification.md) |
| Output caps and profile defaults | Gateway/init cap resolution, wire parameter selection and profile temperature/token defaults reach both native request paths | 496 Python comparisons, eight startup HTTP requests; [verification](analysis/output-cap-verification.md) |
| Request-body projection and Vercel | Split profile maps, caller overrides, shallow SDK projection and legacy custom-provider body selection wired into native requests | 76 source comparisons and eight HTTP requests; [verification](analysis/request-merge-verification.md) |
| Gemini thinking caps and wire reasoning | Shared pre-hook normalization and thinking output headroom wired into both native request paths | 115 Python comparisons and 14 HTTP requests; [verification](analysis/gemini-thinking-verification.md) |
| Native prompt-cache routing | Per-turn persisted scope, static-prefix/tool hashing and explicit key bounding wired before SDK body flattening; dispatcher locks share history identity | 65 Python comparisons, ten HTTP requests and endpoint/profile gate checks; [verification](analysis/prompt-cache-verification.md) |
| Native tool replay and message projection | Valid argument text, thought signatures and reasoning sidecars preserved in flight; outgoing copies enforce model filtering and provider echo policy | 92 Python comparisons, eight tool HTTP requests and twelve startup HTTP requests; [verification](analysis/tool-call-replay-verification.md) |
| Native refusal payloads | Non-streaming refusal-only responses reach the user; usable text and tool calls retain precedence | 48 Python comparisons and a real HTTP-to-event test; [verification](analysis/refusal-verification.md) |
| Tool-name recovery | Name repair, deterministic identity ordering and three-strike invalid-batch termination integrated | 143 Python cases, native HTTP execution and retry sequences; [verification](analysis/tool-name-repair-verification.md) |
| Malformed tool batches | Unchanged-history retries, third-attempt paired recovery, and immediate truncation stops integrated | Source-classified arguments and zero-execution batch regression; [verification](analysis/tool-argument-verification.md) |
| Tool argument normalization | Blank and structured argument values normalized before the native execution guard | 21 source-loop cases plus execution comparisons; [verification](analysis/tool-argument-verification.md) |
| Tool call validation | Invalid names and non-object arguments return paired errors without executing tools | 61 Python cases and native execution checks; [verification](analysis/tool-argument-verification.md) |
| Empty post-tool retry | Bounded continuation nudge with inline-thinking exclusion and reset after new tool work | Four loop sequences and real HTTP recovery; [verification](analysis/tool-text-verification.md) |
| Native streaming reasoning filter | Stateful upstream reasoning suppression connected to SSE delivery | 526 Python sequences and six byte-split SSE cases; [verification](analysis/tool-text-verification.md) |
| Native final-answer cleanup | Reasoning and tool XML removed before non-streaming tool-loop delivery | 156 Python cases through the loop and HTTP response proof; [verification](analysis/tool-text-verification.md) |
| Post-tool answer recovery | Housekeeping answers recovered after empty follow-ups; substantive work invalidates stale answers | 156 Python cleanup cases and eight native-loop sequences; [verification](analysis/tool-text-verification.md) |
| Tool-template marker cleanup | Bare bracketed protocol markers removed from validated tool-batch replay | 17 Python cases through the native loop; [verification](analysis/tool-text-verification.md) |
| Delegation batch cap | Config/env limit propagated to native filtering before duplicate suppression; delegation engine remains pending | 39 Python cases and execution/replay order regression; [verification](analysis/tool-pairing-verification.md) |
| Duplicate execution suppression | Equivalent name/argument pairs run once per batch while distinct requests retain unique IDs | 24 Python cases and native execution/replay regressions; [verification](analysis/tool-pairing-verification.md) |
| Tool-call/result repair | Full JSON sanitizer, deterministic missing IDs and fresh-batch ID renaming wired into native requests | 447 Python cases, duplicate execution regression and main/summary HTTP; [verification](analysis/tool-pairing-verification.md) |
| Thinking-only message repair | Empty-message healing, prefill/reasoning detection and adjacent-user text/image merges wired before native schema stripping | 207 Python cases, precedence checks and main/summary HTTP proof; [verification](analysis/message-repair-verification.md) |
| API content replay | Existing sidecars restored on outgoing copies before native schema projection; persistence and note generation remain | 100 Python cases and main/summary HTTP replay; [verification](analysis/api-content-verification.md) |
| Native iteration summary | Normal cap exit requests a tool-free summary with bounded empty retry; full provider/finalizer parity remains | 17 Python cleanup and 76 temperature cases, inline loop contracts and real HTTP; [verification](analysis/iteration-summary-verification.md) |
| Native turn-limit configuration | Config/env authority and unlimited default reach the native loop; budget accounting and runtime refresh remain | 88 Python comparisons and real HTTP limit regression; [verification](analysis/turn-limit-verification.md) |
| Native tool events | Decoded arguments, per-turn correlation indexes and measured execution duration emitted through the loop | Inline multi-iteration, failed-call and counter-reset regression; [verification](analysis/tool-events-verification.md) |
| Native tool-result construction | Names, canonical IDs, timestamps, elision notices, untrusted framing and advisory findings wired into result creation; internal fields stripped at wire projection | 191 Python comparisons and three real HTTP tool rounds; [verification](analysis/tool-result-verification.md) |

The classification oracle executes AST-extracted Python predicates and the
actual inbound bucketing loop. This proves the pure transformation contract,
not end-to-end media delivery, model resolution, STT, or vision integration.
The broader runner tier includes network calls and session mutation, despite
the earlier map calling it pure. See [the source audit](analysis/tier2-source-audit.md).

Current validation: 1238 workspace tests passed, two tests ignored by default
(Python bridge and optional FFmpeg integration). The FFmpeg integration test was
also explicitly run and passed. Clippy with warnings denied and formatting pass. Rust tests live
inside their implementation files, following the user's layout preference.
See [inbound verification](analysis/inbound-state-verification.md) and
[routing verification](analysis/image-routing-verification.md) and
[native image verification](analysis/native-image-verification.md) and
[structured transport verification](analysis/structured-content-verification.md) for source
comparison results and limitations. The full-port goal remains active;
tested orchestration is not yet the complete live runner.

Next steps, in order:

1. Port the live image capability lookup, runtime model resolution, remaining
   native decoder support (HEIC/AVIF), and @ context expansion.
   Local-server probes, locality, and the Ollama fallback now exist. Next are
   dynamic provider discovery and custom hooks (the 13 base-only bundled modules
   now load natively, along with Upstage and Nebius request hooks), then runner construction around the live
   capability lookup. Managed local capability and cloud registry caching are implemented;
   follow the corrected [dependency plan](analysis/live-capability-plan.md).
2. Connect transcription and vision orchestration to real provider adapters
   behind their explicit I/O boundaries. Native STT HTTP now exists; complete
   its [strict credential-resolution dependencies](analysis/stt-credential-resolution-plan.md)
   before constructing it from live profile settings.
3. Integrate the pipeline and pending-message state into the richer runner event path with real
   adapter and model-runtime resolution. The Dispatcher accepts a
   core Message type carrying prepared content parts, but platform adapters
   still need attachment download, enrichment, and session routing integration.

The sections below retain the earlier port inventory; this handoff qualifies
what "ported" means where that inventory does not distinguish runtime wiring.

Full rewrite of hermes-agent from Python to Rust. Goal: lower memory/startup
footprint and a single deployable binary. Strategy is strangler-fig, not
big-bang: we stand up Rust components one at a time behind the boundaries that
already exist in the Python codebase, keep everything running, and stay in
sync with upstream by pinning a known commit as the porting spec.

## Layout

```
rust/
  Cargo.toml            workspace
  crates/
    hermes-core/        shared types + error (no async, no IO)
    hermes-gateway/     the long-lived network process (first target)
```

## Phase order

1. **Gateway** (`crates/hermes-gateway`) — the long-lived, latency-sensitive,
   most self-contained process. Ports `gateway/`. *In progress.*
2. **Tool runtime + RPC host** — tool-calling layer and subprocess/RPC plumbing.
3. **State + search** — SessionDb (conversation history) + FTS5 message search
   done (session_db.rs); full schema/migrations + CJK C ext remain. *In progress.*
4. **Agent core loop** — `run_agent.py`. Last, once contracts are frozen.
5. **TUI / web frontend** — left in TS/React unless there's a reason to move it.

## Subsystems ported (cohesive units, not leaves)

- **Media delivery** (`platforms/base.py` core -> media.rs) + `media_policy.py`
  -> media_policy.rs + `media_repair.py` -> media_repair.rs. The security gate
  (validate_media_delivery_path: denylist, allowlist, strict/recency, symlink
  resolution), MEDIA extraction (extract_media/extract_local_files with fenced/
  inline/blockquote/JSON masking, char-accurate offsets), the config->env policy
  bridge, and computer_use path repair. Docker container-path translation is
  behind a `SandboxLayout` seam (configured volumes + cwd bind work now; the
  session-scoped sandbox roots plug in with the terminal subsystem).
- **Hooks** (`hooks.py` -> hooks.rs). Design decision made: a compiled binary
  can't import user Python in-process, so hooks execute as SUBPROCESSES
  (executable / interpreter model) with event on argv + JSON context on stdin +
  JSON stdout as the return. HOOK.yaml discovery, `command:*` wildcard routing,
  and emit / emit_collect are preserved.
- **Status / lifecycle core** (`status.py` -> status.rs). gateway_state.json
  writer/reader (StatusUpdate), the pure derivations (normalize_updated_at,
  parse_active_agents, derive_gateway_busy/_drainable, staleness + no-kill PID
  liveness with a /proc start-time reuse guard), PID file, the exclusive runtime
  flock, and the respawn-storm breaker. session_db_recovery's health aggregate
  is wired into it via a startup sink.
- `message_timestamps.py` -> message_timestamps.rs (chrono; parse/strip/render).
- **Lifecycle / shutdown telemetry**: `lifecycle_ledger.py` -> lifecycle_ledger.rs
  (unclean-death detection via the sentinel, sample_memory, the loop-heartbeat
  writer, state.db quick_check — the write side of what memory_status reads;
  wired live into the singleton path: record_startup on boot, 30s heartbeat,
  mark_exited on shutdown). `restart.py` -> restart.rs (supervisor detection,
  container-restart routing, drain/cron/signal timeout budgets, systemd
  TimeoutStopSec sizing). `shutdown_forensics.py` -> shutdown_forensics.rs
  (SIGTERM/SIGINT /proc snapshot, detached ps/pstree/dmesg diagnostic, systemd
  timing-alignment check).
- **Data-loss flush/recover**: `shutdown_flush.py` -> shutdown_flush.rs
  (flush_pending_to_file / recover_pending_to_db / flush_agent_history_to_file;
  wired live: recover_pending_to_db runs at singleton startup and is verified to
  reinsert a prior life's flushed messages). SessionDb gained
  `append_message_with` (tool fields + display_kind/display_metadata + explicit
  timestamp) + `get_message`, the shape delivery/TUI rows and cron deliveries
  need. `wake.py` delegation-persistence half -> wake.rs
  (persist_delegation_delivery + delegation_display_metadata over SessionDb).
- `memory_monitor.py` -> memory_monitor.rs (periodic [MEMORY] RSS logging on a
  tokio task, getrusage-based; wired into startup, always on).
- `mirror.py` -> mirror.rs (delivery-mirror into a session transcript; +
  SessionDb::find_session_by_origin unambiguous-match + set_thread_id).
- `scale_to_zero.py` DECISION layer -> scale_to_zero.rs (idle predicate, arming
  precondition, idle-timeout parse, relay-only check, dashboard-client liveness
  marker, self_suspend_available). The Fly Machines suspend POST is deploy I/O.
- `channel_directory.py` READ half -> channel_directory.rs (load the cached
  directory + friendly-name alias overlay, resolve_channel_name, lookup type,
  format_directory_for_display). The adapter-driven BUILD half lands with the
  adapter subsystem.

## Hub cores (bounded slices of the big files)

- `session.py` IDENTITY core -> session.rs: SessionSource (+ to_dict/from_dict
  with the scope_id/guild_id migration + description), build_session_key (the
  single source of truth: DM/group/thread isolation, Slack scope prefix,
  WhatsApp canonicalization, Discord prospective-thread continuity, profile
  namespacing), is_shared_multi_user_session, id hashing, path/key traversal
  guards, sanitize_model_override. The 3k-line SessionStore transcript layer
  (persistence, prompt building, auto-continue) overlaps session_db.rs and lands
  with the agent-core turn path.

## Feature / coupled-module cores (ported in parallel, verified + integrated)

- `authz_mixin.py` primitives -> authz.rs (allowlist parsing, gate-env reads,
  bech32 Nostr npub->hex, verified against a NIP-19 vector). Full
  _is_user_authorized decision needs the adapter registry / pairing store.
- `delivery.py` primitives -> delivery.rs (DeliveryTarget parse/render, silence
  filter, telegram private-chat heuristic). Router is adapter-coupled.
- `stream_consumer.py` display helpers -> stream_consumer.rs (code-fence escape /
  close, StreamConsumerConfig). The async sink is adapter-coupled.
- `browser_control_artifacts.py` -> browser_control_artifacts.rs (one-shot
  artifact store: SHA-256 provenance, MIME/size caps, traversal guard, atomic
  write, TTL + orphan sweep, rate limiter).
- `hosted_room_execution_policy.py` -> hosted_room_execution_policy.rs
  (RoomExecutionPolicy + strict parser + canonical-JSON/sha256 digest, golden-
  tested against real Python; config-resolution half deferred to the runner).
- `hosted_room_peer.py` -> hosted_room_peer.rs (GatewayRoomCatalog + strict
  parser/catalog digest, validate_room_link_url with by-hand urlsplit + IPv4/IPv6
  loopback classification, HostedMemberDispatch, select_room_link, and the grant
  issue/verify/decode machinery with hand-rolled HMAC-SHA256 and byte-exact
  canonical JSON; golden-tested against real Python). Deferred: the filesystem
  grant-secret minting (gateway_room_grant_secret) and the config/env-driven
  catalog/endpoint builders (catalog_mapping, local_catalog_mapping,
  local_room_link_endpoint), which couple to hermes_constants, gateway.config,
  and the deferred front half of execution_policy_mapping.
- `pairing.py` core -> pairing.rs (pairing store: salted-hash codes, rate limit,
  lockout, expiry, atomic 0600 writes, split-dir migration; codes/salts/ids use
  the kernel CSPRNG, fail closed). Operator-allowlist mirror (hermes_cli.config
  + live adapters) deferred to the runner.

## Hosted-room cluster (storage/logic tier COMPLETE; ported in parallel)

Bottom-up: `hosted_room_execution_policy.rs` (RoomExecutionPolicy + digest) ->
`hosted_room_peer.rs` (catalog + grants + validate_room_link_url) ->
`hosted_rooms.rs` (link-store) + `hosted_rooms_log.rs` (the 7-table room-log
authority layer: create_room/append_event/read_events with idempotent ingest +
gap + epoch-regression refusal) -> `hosted_room_links.rs` (StoredRoomLink
management), `hosted_room_replicas.rs` (replica store + promote/demote fencing),
`hosted_room_policy_checkpoint.rs` (bounded policy projection). All golden/vector
tested where crypto or canonical JSON is involved. Remaining in this cluster:
`hosted_room_discussion.py` (1461) + `hosted_room_driver.py` (1778) + the rest of
`hosted_rooms.py` orchestration — these are GatewayRunner/agent-core coupled and
land with the runner. `hermes_cli/install_identity.py` is now ported
(`install_identity.rs`: CSPRNG-minted 32-hex install id, flock publication
fence, atomic write, process cache) and wired into `hosted_rooms_log`, so
`local_authority_gateway_id` resolves a real `install:<id>` and the
promote/demote fencing takes over under it. It still fails closed if the id can
neither be read nor minted.

## Adapter-support + relay leaves (ported in parallel)

Self-contained slices that unblock the adapter and relay subsystems, each
golden-tested against real Python:

- [x] `code_skew.py` -> code_skew.rs (git-revision fingerprint + hot-pull skew
      detection; also ported the `.git` ref parser it borrows from hermes_cli.main)
- [x] platforms/base.py value tier -> platform_base_types.rs (MessageType,
      ProcessingOutcome, MessageEvent + is_command/get_command/get_command_args,
      SendResult, EphemeralReply, and the pure send-error classifiers
      classify_send_error / is_chat_level_not_found). BasePlatformAdapter, the
      media-cache primitives, TextDebounceState, MessageHandler and the
      caption/pending-event merges stay with the adapter/runner tier.
- [x] `relay/auth.py` -> relay_auth.rs (HMAC-SHA256 sign/verify, base64url
      tokens, delivery signatures; RFC 4231 verified)
- [x] `relay/command_manifest.py` -> relay_command_manifest.rs (Discord slash
      palette on the relay hello frame; byte-exact wire shape)
- [x] `relay/descriptor.py` -> relay_descriptor.rs (CapabilityDescriptor
      handshake; byte-exact json.dumps(sort_keys=True, ensure_ascii=False))
- [x] platforms/qqbot/crypto.py -> qqbot_crypto.rs (AES-256-GCM credential
      decrypt via RustCrypto aes-gcm, never hand-rolled; CSPRNG bind key; golden
      vectors vs Python cryptography AESGCM)
- [x] platforms/qqbot/{constants,utils}.py -> qqbot_common.rs
- [x] platforms/qqbot/keyboards.py -> qqbot_keyboards.rs (inline-keyboard wire
      structs, approval/update-prompt parsers + builders, InteractionEvent;
      ApprovalSender's adapter send is deferred to the QQ adapter)
- [x] platforms/signal_format.py -> signal_format.rs (markdown -> Signal
      bodyRanges, UTF-16 offset math + overlap suppression; 18 golden vectors)
- [x] platforms/signal_rate_limit.py -> signal_rate_limit.rs (token-bucket
      attachment scheduler; asyncio.Lock -> std Mutex held only across the brief
      refill/deduct sections, never across an await; process-global singleton)
- [x] `agent/retry_utils.py` -> retry_utils.rs (Retry-After parse incl. RFC 7231
      HTTP-date via chrono rfc2822; jittered/adaptive backoff; zai overload).
      Obsolete RFC 850 / asctime date forms are not accepted (documented gap,
      only affects a future date in those rare forms)
- [x] platforms/_http_client_limits.py -> http_client_limits.rs (adapter
      connection-pool tuning; httpx Limits -> reqwest pool knobs)
- [x] `relay/transport.py` -> relay_transport.rs (RelayTransport async trait;
      reuses MessageEvent + CapabilityDescriptor; pure interface)
- [x] platforms/webhook_filters.py -> webhook_filters.rs (declarative route
      filters + subprocess script transforms; timeout kills+reaps the direct
      child without joining reader threads, matching CPython subprocess.run's
      leak-the-grandchild-fd timeout behavior). Deferred: build_subprocess_env
      secret-scrub and agent.redact output redaction (base behavior reproduced).
- [x] platforms/yuanbao_proto.py -> yuanbao_proto.rs (Yuanbao WebSocket protobuf
      codec, hand-rolled varint + length-delimited ConnMsg framing; 14 golden
      test fns). Deviation: decode_conn_msg/decode_biz_msg degrade to a default
      struct on a hard wire error instead of raising.
- [x] platforms/yuanbao_sticker.py -> yuanbao_sticker.rs (sticker catalogue +
      fuzzy search + FaceMsg wire body). Gap: _normalize_text NFKC is strip+
      lowercase only (no NFKC crate); a no-op for ASCII/CJK, diverges only for
      compatibility-form search queries. Closing it needs unicode-normalization.

## Config + API-server foundation

- [x] gateway/config.py schema tier -> config_schema.rs (Platform enum with all
      24 built-in members, the coercion/normalization helpers, watchdog clamp,
      platform_binds_port). config.rs remains the runnable skeleton.
- [x] gateway/config.py dataclasses -> config_types.rs (HomeChannel,
      SessionResetPolicy, ChannelOverride, PlatformConfig, StreamingConfig; each
      Default + from_dict/to_dict, reusing config_schema).
- [x] GatewayConfig -> config_gateway.rs (fields, defaults, __post_init__,
      to_dict/from_dict, _has_usable_api_server_key).
- [x] _apply_env_overrides -> config_env_overrides.rs. All 158 env vars present,
      verified mechanically by rust/tools/check_env_override_parity.sh. Faithful
      quirks kept (the dead BLUEBUBBLES_REQUIRE_MENTION guard writing false; the
      warn helper bypassing the secret scope). The registry-driven plugin-enable
      pass is a documented no-op (Python wraps it in try/except -> debug).
- [x] load_gateway_config + _validate_gateway_config -> config_loader.rs, which
      composes the pipeline: file layers -> env overrides -> validate. The
      non-uniform precedence is reproduced exactly, including multiplex_profiles
      having no elif (so gateway.json beats gateway.multiplex_profiles).
      Deferred, matching Python's fail-open: managed_scope.apply_managed_overlay
      (identity) and plugin discovery (the _pr = None branch).

THE CONFIG LAYER IS COMPLETE AND DIFFERENTIALLY VERIFIED. rust/tools/
gen_config_goldens.py captures real Python `load_gateway_config().to_dict()` for
18 fixture homes; the `golden_corpus` test in config_loader.rs replays them
through the full Rust pipeline with a cleared process environment, and all 18
match exactly. That covers config_loader + config_env_overrides + config_gateway
+ config_types + config_schema together.
- [x] agent/secret_scope.py -> secret_scope.rs (get_secret fail-closed
      resolution, is_global_env tables, multiplex flag, parse_env_value /
      strip_inline_comment / load_env_file). The Python ContextVar is modeled as
      a tokio task-local with a scope-based with_secret_scope(...) API. Remaining:
      the external get_secret_source_values merge (its loader is unported) and
      wiring the scope into the multiplex turn path when multiplexing is built.
- [x] platforms/api_server_run_idempotency.py -> api_server_run_idempotency.rs
      (the full RunIdempotencyStore: SQLite dedup for POST /v1/runs, Created/
      Reused/Conflict via constant-time fingerprint check, terminal+expired
      prune). The rest of the api_server layer (api_server.py, room_grants,
      room_dispatch, runs) is aiohttp-mixin / runner coupled and lands with the
      API-server subsystem.

## Live HTTP surface

- `GET /healthz`, `GET /readyz` (readiness probes), `GET /status` — the last
  assembles the persisted runtime record (status.rs) + live disk_status +
  memory_status + readiness, so all the ported telemetry is observable over
  HTTP. `POST /message`, `GET /display/:platform`, `GET /search`.

## Deferred (port later, with the subsystem they belong to)

- `turn_context.py` — a Python closure-extraction artifact (~60 opaque handles,
  `[None]` single-element cells simulating shared closure state). Its Rust shape
  will be nothing like this; port it with the TurnRunner/_run_agent_inner loop.
- `session_state.py` legacy dict-view adapters — backward-compat shim for
  pre-refactor Python tests only; no Rust equivalent needed. (Data model ported.)
- `session_context.py` — Python `contextvars` + `os.environ` fallback emulating
  task-local session scope; its consumers are `os.getenv("HERMES_SESSION_*")`
  calls in Python tool code. The Rust dispatcher already threads the session
  scope explicitly (Message + session key), so like turn_context this belongs
  with the agent-core turn path, not a contextvar emulation.
- `agent/secret_scope.py` — DONE (secret_scope.rs), see the config foundation
  section. The ContextVar became a tokio task-local scope API. session_context
  can follow the same pattern when its turn-path consumers are ported.
- `wake.py` — the delegation-persistence half is DONE (wake.rs). The remaining
  push-capable and API-server self-POST wake paths are coupled to the adapter
  base (`MessageEvent`/`handle_message`) and the API-server adapter; port them
  with the adapter / API-server subsystem.
- `shutdown_flush.py` transcript-spool WRITER queue (flush_overflow_to_file /
  spool_dropped_transcript_message / drain_transcript_spool) — tied to run.py's
  transcript-cap-drop path; recovery already handles their files by reason.
- `stream_dispatch.py` — the event router hangs off the adapter render-hooks and
  the `GatewayStreamConsumer` sink, both in the `stream_consumer.py` hub (3.6k
  LOC, unported). Port it with that subsystem, not against stubs.
- `status.py` remainder — the scoped credential-lock protocol (acquire/release
  _scoped_lock, cross-profile --replace takeover markers) and the dashboard-side
  liveness ladder (`resolve_gateway_liveness` + cmdline/profile heuristics). Port
  with the CLI/dashboard surface that consumes them; the gateway's own
  process-singleton + status-writer core is done.
- `startup_watchdog.py` — a re-export shim for the repo-root
  `hermes_startup_watchdog` (a pre-import deadlock guard). A compiled binary has
  no import-time deadlock; if a boot-liveness watchdog is wanted it is a separate
  process-level concern, not this shim.

## Not applicable (no Rust analogue)

- `code_skew.py` — detects a Python interpreter running stale code after a hot
  `git pull` (frozen `sys.modules`, first-time lazy imports resolving against a
  stale cached dep). A compiled Rust binary is fully loaded into memory and has
  no lazy imports, so it can never run stale code this way. Nothing to port.

## Stack mapping

| Python            | Rust                         |
|-------------------|------------------------------|
| asyncio           | tokio                        |
| httpx / aiohttp   | reqwest / hyper              |
| fastapi / uvicorn | axum                         |
| pydantic          | serde (+ validator)          |
| sqlite3 + FTS5    | rusqlite (keep C FTS5 ext)   |
| prompt_toolkit    | ratatui + crossterm          |
| jinja2            | minijinja                    |
| croniter          | tokio-cron-scheduler / cron  |

## Gateway port order (from analysis/gateway-map.md)

68 modules directly in `gateway/`. 38 are pure leaves (0 intra-package deps),
2 are the hubs everything hangs off: `run.py` (34,847 LOC, 56 intra + 158
external deps) and `slash_commands.py` (6,576 LOC). Port leaves first, hubs
last. Next concrete targets, in order:

1. `stream_events.py` (171 LOC) — DONE. Ported verbatim as the `StreamEvent`
   enum in `hermes-core/src/stream.rs` (all 7 variants, exhaustiveness compiler-
   enforced). Only deviation: `GatewayNotice.kind` is `notice_kind` in Rust to
   avoid colliding with the enum's `kind` discriminator tag; it is an internal
   representation the bridge maps into, not a direct Python-dataclass decode.
2. `turn_context.py` (150), `session_state.py` (475), `turn_lease.py` (355) —
   per-turn/session state + the lease that serializes a session's turns.
   (turn_lease done; session_state data model done; turn_context deferred.)
3. `response_filters.py` (147), `display_config.py` (322) — done (leaves).
4. `platform_registry.py` (699) — done (platform.rs).
5. Status rollups (`disk_status.py`, `memory_status.py`) — done.
6. hubs last: `config.py`, `session.py`, `stream_consumer.py`, `status.py`,
   `slash_commands.py`, `run.py`.

## Gateway status

- [x] Workspace + toolchain + build
- [x] Server skeleton (axum) with `/healthz`, `/readyz`, graceful shutdown
- [x] Env-driven config (`HERMES_GATEWAY_BIND`)
- [x] Agent boundary trait (`AgentClient`) + `StreamEvent` contract
- [x] Strangler bridge: Rust drives the Python agent via
      `python -m hermes_cli.stream_turn` (JSONL over stdio), tested end to end
      (agent::tests::empty_prompt_terminates_cleanly)
- [x] Turn-dispatch loop (`Dispatcher`)
- [x] Runnable end to end: `POST /message` runs a turn through the agent and
      returns the reply (the "local" adapter over HTTP). Config via
      `HERMES_AGENT_PYTHON` / `HERMES_AGENT_CWD` / `HERMES_AGENT_MODEL`.
- [x] Dispatcher wired into main() with a real push-based adapter
- [x] Telegram adapter (long-poll getUpdates -> Dispatcher -> sendMessage),
      started when `HERMES_TELEGRAM_TOKEN` is set. Boot + graceful-backoff
      verified against the live API.
- [x] Discord adapter (Gateway WebSocket: HELLO/heartbeat/IDENTIFY/dispatch ->
      Dispatcher -> REST sendMessage), started when `HERMES_DISCORD_TOKEN` set.
      WS handshake verified against the live gateway.
- [x] Slack adapter (Socket Mode: apps.connections.open -> WS -> events_api
      envelopes with ack -> Dispatcher -> chat.postMessage), started when both
      `HERMES_SLACK_APP_TOKEN` and `HERMES_SLACK_BOT_TOKEN` are set. Handshake
      verified against the live API.
- [ ] More platform adapters: WhatsApp (Baileys bridge), Signal
- [x] Native (in-Rust) agent client for plain chat turns: opt-in via
      `HERMES_AGENT_NATIVE=1` + `HERMES_LLM_API_KEY`, calls an OpenAI-compatible
      `/chat/completions` and streams the reply (no Python). Narrow scope: no
      tools/history/memory yet. Auth path verified against OpenRouter (401 on a
      bad key, no spend); request-building + SSE parsing unit-tested.
- [x] CLI-backend agent client (`HERMES_AGENT_CLI`, e.g. claude/agy): runs a
      turn via an external agent CLI, no Python and no HTTP key. VERIFIED live:
      POST /message -> agy (gemini-3.8-flash-high) -> real reply, zero Python.
      This is the path that works for a CLI-backend hermes setup.
- [x] Native HTTP tool loop wired into run_turn (opt-in `HERMES_AGENT_TOOLS=1`;
      native_tools.rs ChatModel + run_tool_loop + CurrentTimeTool).
- [x] Conversation history: SessionDb (state.db) persists per-session
      user/assistant messages; the turn path loads prior history and threads it
      into stateless backends (native HTTP messages array, CLI transcript). The
      Python bridge opts out (manages_history) since it owns its own history.
      VERIFIED live multi-turn via agy: "my name is Denny" then "what is my
      name?" -> "Denny". Memory/skills (rest of run_agent.py) still remain.
- [x] Delivery ledger (durable outbound obligations): SQLite ledger over
      state.db (record/attempting/delivered/failed + startup sweep_recoverable
      that claims dead-owner rows via pid+/proc start-time liveness, with
      at-least-once markers; attempts cap + stale/retention pruning). Not yet
      wired into the delivery path; runtime-reconnect sweep deferred.
- [x] Drain control (drain_control.py): external drain-marker contract
      (.drain_request.json), with instantiation-epoch (boot_id + PID1 start) and
      max-age staleness so a restart/orphan clears the drain. Contract + tests
      done; the drain watcher that flips the gateway to "draining" is not wired.
- [x] Control socket (control_socket.py): local owner-only UDS answering
      identify/status (one JSON line in/out), with the sun_path fallback +
      pointer file. Wired live and cleaned up on shutdown; verified via a real
      UDS client (identify payload, unknown-verb listing, 0600 perms).

### Leaf modules ported (self-contained, tested; wiring into their real call
sites tracked separately)

- [x] `rich_sent_store.py` -> rich_sent_store.rs (Telegram rich-send text index)
- [x] `restart_loop_guard.py` -> restart_loop_guard.rs (auto-resume respawn breaker)
- [x] `sticker_cache.py` -> sticker_cache.rs (Telegram sticker description cache)
- [x] `systemd_notify.py` -> systemd_notify.rs (sd_notify READY/WATCHDOG/STOPPING
      + a tokio watchdog that feeds only while the runtime keeps making progress)
- [x] `cwd_placeholder.py` -> cwd_placeholder.rs (TERMINAL_CWD placeholder resolution)
- [x] `status_phrases.py` -> status_phrases.rs (generic status-line catalog;
      built-in asset embedded, profile-relative user catalogs, recent-repeat avoidance)
- [x] `runtime_footer.py` -> runtime_footer.rs (final-message metadata footer)
- [x] `disk_status.py` -> disk_status.rs (/api/status disk block; statvfs sample)
- [x] `memory_status.py` -> memory_status.rs (/api/status memory block; reads the
      persisted heartbeat + lifecycle sentinel, no new sampling)
- [x] `cgroup_cleanup.py` -> cgroup_cleanup.rs (ExecStopPost cgroup reaper)
- [x] `session_db_recovery.py` -> session_db_recovery.rs (recoverable per-path
      handle cache; single-flight opens, exponential backoff, health aggregate)
- [x] `profile_routing.py` -> profile_routing.rs (+ profile_name.rs for the
      profile-id path-traversal guard from hermes_cli/profiles.py)
- [x] `agent_cache_pressure.py` -> agent_cache_pressure.rs (pure/OS parts:
      bounds resolution, cgroup/total-mem limits, anon RSS, eviction planner;
      the AIAgent-shaped guard + sweep land in Phase 4)

## Running

    cd rust && cargo run -p hermes-gateway
    # then, from the hermes repo root as agent cwd:
    HERMES_AGENT_CWD=/path/to/hermes-agent-port cargo run -p hermes-gateway
    curl -s localhost:8787/healthz
    curl -s -X POST localhost:8787/message \
      -H 'content-type: application/json' -d '{"text":"hello"}'

Platform push paths start when their tokens are set: `HERMES_TELEGRAM_TOKEN`,
`HERMES_DISCORD_TOKEN`, `HERMES_SLACK_APP_TOKEN` + `HERMES_SLACK_BOT_TOKEN`.

## Footprint / deploy

Release binary is ~5 MB (LTO + strip), TLS roots compiled in. Build a
container with `rust/Dockerfile` (multi-stage -> debian-slim, non-root):

    cd rust && docker build -t hermes-gateway .
    docker run -p 8787:8787 -e HERMES_TELEGRAM_TOKEN=... hermes-gateway

The gateway runs standalone (health + adapters); to also run the strangler
agent bridge, mount the hermes repo + Python and set HERMES_AGENT_CWD /
HERMES_AGENT_PYTHON.
