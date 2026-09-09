# Native ordinary main-provider fallback resolution

Date: 2026-09-10

## Result

The native ordinary main chat-completions path now freezes and executes the
top-level `fallback_providers` chain followed by legacy `fallback_model`.
Fallback begins only after the active route's static API-key pool cannot
recover. The selected route stays active through later tool rounds and through
the configured primary cooldown.

This checkpoint covers static API-key routes using the OpenAI-compatible
`chat/completions` wire. OAuth routes, Anthropic Messages, Codex Responses,
Bedrock, dynamic provider plugins, and retry-triggered transport fallback remain
outside this checkpoint.

## Runtime design

Each conversation owns one immutable ordered route plan and one small shared
cursor. Every route owns its model, provider identity, endpoint, credentials,
pool, headers, request overrides, reasoning policy, output cap, and context
window. Conversation state such as transcript usage, hooks, memory callbacks,
tools, and compression policy is projected onto the selected route without
copying provider-local transport state.

Streaming calls and non-streaming tool rounds enter the same dispatcher. The
route first exhausts its own credential recovery. A terminal auth, billing,
rate-limit, or upstream-rate-limit response may then advance the cross-provider
cursor. A successful response ends fallback handling before body streaming, so
partial output and tool side effects are never replayed.

The primary prompt remains byte-identical. Each fallback receives a frozen
variant that changes only the final `Model:` and `Provider:` identity lines.
History, user content, tool definitions, and earlier prompt bytes remain
unchanged. Usage is attributed to the route that actually served the request.

## Turn lifecycle and review fixes

Rate and billing failures leaving the primary arm Python's 60-second
exponential cooldown with a four-hour cap. Auth fallback has no ordinary
cooldown and returns to a direct-key primary on the next turn. When a durable
primary pool carries a later provider reset timestamp, turn-start restoration
reloads that pool off the async executor and stays on fallback until the reset.
Pool read failures deliberately fail open.

A fully exhausted non-rate fallback chain floors the existing cooldown at the
live Python value of five seconds. This prevents immediate cross-turn replay of
the whole chain while retaining the last active fallback route.

Claude's separate post-implementation review found the missing durable reset
gate and exhaustion floor. Both were reproduced with failing tests before the
fixes. The helper reports initially called the floor 10 seconds, but the primary
lane checked the current Python constant and corrected both reports and the
source-executed oracle to five seconds.

The review's minor suggestion to apply `FallbackChainEntry` reasoning and
output-cap fields directly to ordinary main fallback entries was rejected after
checking the live Python switch. That path re-resolves reasoning from the normal
provider and model configuration and does not read those entry fields. They are
shared parser fields used by auxiliary compression, not a main-turn override.

## Evidence

- A 104-case Python corpus covers chain parsing, credentials, trigger classes,
  pool ordering, cooldowns, backend identity, route reconfiguration, prompt
  stability, tool-loop carryover, terminal propagation, and the safe-port
  boundary.
- Live HTTP tests prove primary 429 fallback, cooldown stickiness, prompt-prefix
  stability, fallback tool rounds, next-turn direct-key auth restore, and
  primary-pool exhaustion before cross-provider activation.
- A real local HTTP plus `auth.json` test proves a future durable pool reset
  prevents a premature return to the primary after the local cooldown expires.
- The full Rust workspace passes 1,854 tests with two expected ignores.
- The selected Python fallback and restore suite passes 150 tests.
- Rust formatting, Ruff, Python formatting, Clippy with warnings denied, oracle
  regeneration, and diff hygiene pass.

## Remaining work

The next main-fallback seam is transport and retry policy: timeouts, overload,
generic server failures, malformed successful responses, safety refusals, and
the exact retry budgets that precede fallback. After that, native non-chat and
OAuth provider clients can join the same route plan. Operator fallback notices,
proactive primary-pool reselection, dynamic plugin providers, and broader
invalidation and eviction policy also remain.
