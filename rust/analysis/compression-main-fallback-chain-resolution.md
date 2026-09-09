# Native compression top-level fallback-chain resolution

Date: 2026-09-10

## Outcome

Native full-compression summaries in auxiliary `provider: auto` mode now
freeze and consult the main agent's top-level `fallback_providers` and legacy
`fallback_model` policy. The production order is main route, one eligible
`auxiliary.compression.fallback_chain` candidate, then one eligible top-level
candidate. The top-level tier is reached only when the task tier has no
eligible client. A task or top-level candidate that executes consumes the one
configured-fallback attempt. Unusable output or a non-auth request error from
the selected top-level candidate propagates without trying a second configured
entry, matching the Python request path.

The route plan remains conversation-owned and immutable. Startup resolves all
provider profiles, endpoints, transports, and secrets through the existing
compression-client builder and the active profile's frozen dotenv and secret
scope. Summary prompt construction is unchanged, and every route receives the
same prompt bytes without tools or recursive fallback state.

## Contract implemented

`compression_auxiliary::main_fallback_chain` accepts an object or array from
each source, reads `fallback_providers` before `fallback_model`, requires
truthy provider and model values, applies Python string coercion, normalizes
string base URLs, and keeps the first case-insensitive
`(provider, model, base_url)` identity. The 76-case source-executed corpus pins
container coercion, scalar behavior, merge order, deduplication, credentials,
transport aliases, provider skip rules, the 64,000-token compression floor,
resolution failures, timeout behavior, and the one-candidate request budget.

Top-level entries deliberately differ from task fallback entries. Their
inline or env-backed credentials and transport still participate in client
resolution, but their `timeout`, `reasoning_effort`, and
`max_output_tokens` do not become per-entry request policy. They inherit the
task timeout and task request extensions. This mirrors the current Python path,
where only `auxiliary.<task>.fallback_chain[...]` labels can recover those
per-entry controls.

Provider skipping uses the raw configured labels, as Python does. Startup
removes `auto` plus entries matching the configured main provider before
building clients. Runtime still applies the task chain's model-aware or
credential-aware failed-route predicate separately, so this checkpoint does
not collapse two different skip contracts.

## Auto route correction found by integration

The first implementation used `needs_separate_client` as the auto-mode test.
That field answers a different question: an auto route with task-specific
request settings needs a dedicated frozen client even though its routing mode
is still auto. The live HTTP integration exposed the error because that shape
retried the main model and never consulted the top-level chain.

The corrected plan derives auto mode from the normalized provider. When an
auto route needs a dedicated client, that client occupies the first main-route
slot, stays pinned to the active conversation model instead of the provider's
default auxiliary model, and carries the frozen task request policy. Otherwise
the conversation client itself occupies that slot. Both shapes then share the
same task and top-level fallback tiers.

## Helper lane disposition

AGY owned the Python contract report and deterministic oracle generator.
Primary review corrected its initial claim that truthy non-string provider and
model values are rejected, and corrected the shallow-copy behavior of
non-string base URLs. The corpus was extended from 70 to 76 cases to pin those
edges before Rust consumed it.

Claude independently mapped the narrow Rust seam and correctly recommended
reusing `FallbackChainEntry`, the existing client builder, two named frozen
tiers, and one shared request gate. Its recommendation to use
`!needs_separate_client` as the auto predicate was rejected after the live
integration proved the auto-with-request-settings counterexample. No helper
owned the production integration or committed code.

The codebase-design skill kept route selection inside the existing frozen
compression plan. No second resolver, prompt path, or process-global state was
introduced.

## Production proof

The local HTTP startup integration exercises three distinct shapes: explicit
auxiliary routing, ordinary auto mode with a task fallback, and auto mode with
task request settings plus a top-level fallback. The last shape proves the
main route fails first, the duplicate configured-main provider is skipped, the
top-level endpoint receives its own credential and model, task request
extensions remain present, and inert per-entry timeout, reasoning, and output
cap controls do not reach the wire.

Route-plan tests separately pin ordering, the shared one-shot budget, known
small-context rejection, unknown-context admission, and startup-unavailable
route selection. Rust parser tests consume the Python corpus directly.

Final validation passed: 1,815 Rust workspace tests with two expected ignores,
20 selected Python fallback tests, byte-for-byte regeneration of the 76-case
corpus, Rust formatting, Python formatting and Ruff, Clippy with warnings
denied, and diff hygiene.

## Deferred boundaries

- Built-in auxiliary provider discovery after configured policy exhaustion.
- Provider unhealthy-cache state and stale-credential refresh or rotation.
- Non-chat auxiliary transports.
- Streaming progress fences and stall-triggered pinned retries.
- Broader provider failover and overflow recovery.

These remain separate checkpoints because they require new runtime state or
transport behavior. They are not hidden behind the now-live configured route
plan.
