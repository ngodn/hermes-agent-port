# Native main-provider successful-body resolution

Date: 2026-09-10

## Result

The ordinary native chat-completions path now validates and recovers successful
HTTP response bodies after the pre-body dispatcher accepts them. Streaming and
tool rounds share the existing frozen route cursor, but keep transport-specific
body handling so streaming output is never buffered or replayed after delivery.

The implemented subset covers malformed buffered bodies, empty finished
responses, zero-chunk streams, content-policy refusals, reasoning-only output,
valid empty-content tool calls, and the post-tool empty nudge. It also keeps
terminal diagnostics out of durable transcript replay and external memory.

## Recovery behavior

- A buffered response without `choices[0].message` advances immediately to an
  eligible frozen fallback. Without a fallback it uses the configured
  `agent.api_max_retries` attempt budget and returns an error on exhaustion.
- A stream with no finish signal and no generated content is an empty transport
  stream, not a model-authored empty answer. The default three-attempt stream
  budget runs before provider fallback. A finished empty assistant response
  instead enters the empty-response guard.
- Two identical evidence-backed empty responses skip the remaining retries.
  Ambiguous empties retain the three-retry, four-request fail-open budget.
  `agent.empty_response_guard.enabled` and threshold coercion match the Python
  corpus. Cost-based budget reduction is explicitly deferred because native
  pricing and billing-route normalization do not exist yet.
- A reasoning-only response receives two bounded continuation attempts before
  the normal empty ladder. Six exhausted attempts produce a labeled, 500
  character reasoning excerpt. The request remains byte-identical because the
  live Python chat-completions sanitizer also removes its conceptual prefill
  marker before provider I/O.
- `finish_reason="content_filter"` and refusal-only payloads never retry the
  same provider. They advance to an eligible fallback or return one formatted,
  actionable refusal notice.
- Empty visible content with valid tool calls remains usable. A post-tool empty
  gets one alternation-safe nudge for each latest tool round, rather than one
  nudge for the entire turn.

Rejected successful bodies do not enter the static credential pool, arm the
primary cooldown, or contribute accepted-response usage. Each fallback keeps
its own frozen model, endpoint, headers, prompt identity, tools and request
policy.

## Replay and persistence safety

The first visible streaming delta remains the no-replay boundary. A later body
error or policy finish fails without issuing another request. Before that
boundary, retries and fallback are safe because no message, tool call or tool
side effect has escaped.

Python treats `"(empty)"`, the reasoning-only terminal excerpt and the refusal
wrapper as delivery text rather than assistant history. Rust now exposes the
same distinction through the existing agent boundary. The native client marks
the active physical session after any same-turn compression rotation, both HTTP
and push persistence paths skip the reply, and post-persist finalization skips
micro-compaction and external-memory completion while still recording accepted
usage. A real SQLite test proves that the user turn remains durable while the
reasoning excerpt never appears in provider replay.

Reasoning tag extraction uses ASCII-only case folding so UTF-8 byte offsets stay
valid. SSE finish reasons preserve strings and arbitrary JSON numbers, including
Poolside's integer values, and Nous `lastOne` usage frames become a clean `stop`.

## Helper split and review disposition

AGY owned the source-executed Python contract and produced 114 cases across
eight sections. A separate AGY task audited the cost-aware retry dependency and
showed that the current fixed budget is Python's required fail-open behavior
until native pricing exists.

Claude independently mapped the Rust replay seam, then reviewed the initial
implementation. The primary lane reproduced and fixed its reasoning-only,
delivery-only, latest-tool-round and generation-detection findings. Its
cost-aware finding is the documented dependency deferral. Length continuation,
operator notices and remaining provider quirks stay outside this checkpoint.
A final narrow Claude delta review timed out without producing an artifact, so
it was not used as evidence.

## Verification

- Full Rust gateway: 1,884 passed, two expected ignores.
- Full Rust workspace: 1,885 passed, two expected ignores.
- Source-executed Python corpus: 114 cases across eight sections, byte-for-byte
  regeneration.
- Focused Python success-body, refusal, empty, continuation and transport suite:
  161 passed.
- Rust formatting, Python formatting, Ruff, workspace Clippy with warnings
  denied, and diff hygiene pass.

## Progress

The capability inventory moves native agent core from 76% to 78%. Gateway,
tool/RPC, and state/search estimates are unchanged. With the stable full-port
weights, the calculation is:

`0.35 * 67 + 0.30 * 25 + 0.15 * 76 + 0.20 * 78 = 57.95`

The refreshed estimate is **57.95%, reported as about 58%**, with a judgment
range of 55% to 61%. This is a production behavior inventory, not a file, line,
commit or test-count ratio. The rounded headline remains 58% because this
checkpoint deepens one agent-core area while large plugin, memory, tool,
transport and gateway surfaces remain Python-only.
