# Full Rust port progress audit, updated 2026-09-10

Current estimate: **56.95% of the full native replacement**, reported as
**about 57%**, with a reasonable judgment range of **55% to 60%**. The native
terminal now executes Unix-local foreground commands and managed non-PTY
background commands through the frozen conversation tool loop, including
static user deny rules with live last-known-good reload and manual approval on
the three native push adapters. Full-compression summaries also use frozen,
failure-scoped task and top-level configured fallback tiers, followed by the
chat-compatible built-in discovery subset with profile-qualified shared
health. Store-backed static API-key discovery routes now recover from exact-key
auth, payment, and rate failures with durable cooldowns and fresh request
clients. Canonical Nous device-code discovery and its cross-profile single-use
OAuth refresh are now native for compression. The main chat-completions route
also selects and recovers store-backed static API keys across streaming and
tool rounds without rebuilding its frozen request. The estimate remains
conservative because other OAuth paths, general provider fallback, non-chat provider transports,
smart approval, PTY and notification
support, remote execution, most tools, and the underlying plugin and
external-memory managers are not native.

This is a weighted engineering inventory, not LOC coverage and not the ratio of
passing tests. Frontend TypeScript stays in scope as an existing client, while
replacing its Python backend, native CLI behavior, plugins, jobs, tools and
execution environments remains part of the port. Python subprocess fallbacks
do not count as native completion.

## Calculation

The scope weights remain unchanged so progress is comparable with the prior
audits.

| Area | Full-port weight | Current area completion | Overall points |
| --- | ---: | ---: | ---: |
| Gateway | 35% | 67% | 23.45 |
| Tool runtime and RPC | 30% | 25% | 7.50 |
| State and search | 15% | 76% | 11.40 |
| Native agent core | 20% | 73% | 14.60 |
| Total | 100% | | **56.95** |

`0.35 * 67 + 0.30 * 25 + 0.15 * 76 + 0.20 * 73 = 56.95`

The arithmetic is exact. The four completion inputs are bounded judgments based
on production wiring and remaining Python surfaces, so reporting more than a
small range would imply false precision.

## Evidence behind the scores

### Gateway, 67%

Production startup, profile-aware client construction, HTTP and push dispatch,
Telegram, Discord and Slack, session admission, delivery state, routing,
recovery, reset, destructive confirmation, secure resume, external-memory turn
lifecycle, and bounded conversation ownership are live and tested. Native HTTP
and push admission now run automatic compression before inbound persistence,
under stable route, transcript, and cross-process lineage leases. Both default
in-place publication and explicit rotation are exercised end to end.

The generic `session:compress` event now reaches profile-scoped user hooks from
every committed native full-compression path. Python function handlers retain
their sync or async `handle(event_type, context)` ABI through a bounded runner.
Ambient profile secrets are cleared before each handler subprocess, and timeout
cleanup covers descendant processes. Other lifecycle hook events remain tied to
the Python gateway.

Most of the Python gateway breadth remains: many platform adapters, command
handlers, queue/steer/interrupt behavior, richer streaming delivery, adapter
callbacks, topic management, and desktop/TUI protocol parity. Three substantial
native adapters do not represent the roughly twenty Python platforms.

The gateway now owns one bounded native background-process registry. Its
stable session-key liveness probe prevents reset and pruning from orphaning
active work, and graceful shutdown terminates all owned groups within shared
bounded grace windows. Restart adoption and autonomous completion delivery
remain Python-only.

The gateway also owns one bounded manual-approval broker. Telegram, Discord,
and Slack surface immutable approval prompts immediately, while typed replies
resolve before session admission and transcript lease acquisition. Requests
are scoped to a stable route and the current sender, and reset, resume,
freshness rotation, and shutdown cancel their pending state. The control plane
does not mutate the transcript or frozen provider prefix.

### Tool runtime and RPC, 25%

The native model tool loop, schema projection, malformed-call repair, duplicate
suppression, result framing, event emission, iteration-summary path, and a
persistent Python extension-host protocol exist. Startup can expose an
extension-backed tool surface, but the native built-in tool catalog remains
minimal and startup still registers only the small core fixture-level surface.
Live native tool rounds now persist assistant call rows before side effects,
persist each result before the next provider request, and replay complete tool
groups on later turns. Deterministic old-result pruning can summarize those
durable rows without changing their call/result identity.

The persistent host also exposes the real `MemoryManager` pre-compression
boundary with strict versioned request and response validation. It preserves
legacy raw-transcript callbacks, gives version 2 providers normalized direct
evidence, sanitizes returned context, and differentiates required fail-closed
requests from optional best-effort requests. This is a production extension
protocol seam, but not yet a native plugin or memory manager.

Fatal extension-host transport failures no longer strand a frozen conversation.
The failed request is not replayed, a replacement child receives the original
profile-scoped initialization, and Rust admits it only when the plugin and
memory capability projection exactly matches the conversation's first snapshot.
This strengthens the existing RPC seam but does not increase its native scope.

A session-bound native local `terminal` tool now runs through the production
provider loop for the explicitly safe configuration slice. Its foreground
boundary owns process groups, concurrent pipe draining, streaming head and tail
retention, private overflow spills, timeout cleanup, and reaping. The
conversation runtime owns a profile-isolated persistent shell environment and
cwd, with cwd updates following the active SQLite route across compression
rotation. Visible and spilled output is ANSI-stripped and redacted. The
unconditional hardline and `sudo -S` floor is source-checked against a
Python-generated corpus, and extension-name collisions cannot replace native
tools. Static user deny rules use Python-compatible normalized glob matching,
reload before each call, retain their last-known-good policy after malformed
edits, and run before foreground or background process creation. Live mode
changes fail closed without mutating the frozen schema. Manual mode now
classifies dangerous commands with the checked-in Python pattern corpus, waits
for a route-scoped decision, and supports once, session, permanent, deny,
timeout, cancellation, and overload outcomes. Session grants survive client
reconstruction, while permanent grants merge through the lossless config
writer. Smart approval, Tirith-enabled manual mode, and remote backends keep
the native tool hidden.

Eligible conversations also expose managed local non-PTY background execution
plus an owner-isolated `process_manage` surface for list, poll, log, wait, and
kill. The gateway registry survives conversation-client eviction, uses
character-bounded incremental output capture, retains results from exit time,
protects active session routes, and owns bounded process-group shutdown. The
live provider loop starts a delayed process and retrieves its result through
the frozen schema. PTY input, notifications, restart adoption, systemd cgroup
isolation, remote processes, and delegation attribution remain open.

The remaining terminal modes, file, browser, web, MCP, execution-environment
backends, smart and Tirith approval runtime, delegation execution, most service
tools, plugin
discovery/management, and full backend RPC account for most of this weighted
area and remain.

### State and search, 76%

SQLite history, structured content replay, FTS foundations, route persistence,
legacy recovery, peer ownership, lineage, conversation generations, prompt
deduplication, frozen tool/plugin snapshot fields, lifecycle closure, reset,
atomic resume, title provenance, compression lineage, durable turn leases,
atomic child-first compression, same-session soft-archive compaction, durable
compression cooldown/breaker fields, and byte-exact protected-prefix cloning
are live. Compacted originals stay searchable, verbatim duplicates stay
hidden, and active message/tool counters are reconciled at commit. Injected
rollback and two-connection contention tests prove the newest multi-row
transitions.

The shared schema now carries the Python reasoning and Codex replay sidecars,
provider usage totals, and the per-model/per-task usage ledger. Its open-time
healer transactionally upgrades the stale five-column Python usage primary key
so task-aware upserts work without losing legacy or orphan rows. Proactive
prune publication atomically archives the original active generation, clones
every wide column, rewrites only candidate content/tool arguments, merges the
durable rearm watermark, and checks the exact snapshot plus turn lease before
commit.

Post-turn micro-compaction now has its own guarded in-place publication. It
requires the live lineage turn lease and exact active snapshot, preserves wide
columns through SQL cloning, keeps absorbed assistant/tool rows searchable,
hides carried-forward duplicate originals, persists the summary marker, and
reconciles live counters atomically. An injected insert failure proves the
archive, clones, marker, and counters roll back together.

Compression publication accepts transcripts ending after a user, a completed
assistant reply, or a fully answered tool-call group. It rejects dangling or
mismatched calls in both in-place and rotation modes. This covers the restored
user-anchor shape used before the next provider request.

Full-compression publication now consumes a complete replacement plan instead
of fixed summary rows plus ID ranges. Retained rows are cloned in plan order,
all durable message columns survive, and only content, API content, and the
summary marker may change. The transaction compares replay fields, display
metadata, and timestamps as part of its exact snapshot CAS. Stored adjacent
summary and anchor user rows are accepted only through the same provider repair
used on outbound requests, while incomplete tool-call groups remain invalid.

The auth store now has production read/modify/write paths for ordinary API-key
pools and the canonical Nous device-code grant. API-key updates merge concurrent
additions and newer live cooldowns. Nous refresh locks the owning profile or
borrowed root through single-use rotation, then mirrors the pair under a shared
cross-profile lock. Both paths use private atomic writes and preserve exact
source ownership. Other OAuth providers and pool-only Nous rows remain
deferred.

The main request path now consults that durable API-key state at startup and
before a same-key 429 retry. Exact wire-key attribution survives stale row IDs,
and every recovery mutation reloads and publishes pool state before a
replacement request. This adds a production consumer for the existing state
machinery rather than counting the oracle alone.

Full schema and migration parity, session search projection, archive/pin/read
state, pruning/export/import, topic bindings, auto-title,
broader transcript operations, cron state, and several desktop/session queries
remain.

### Native agent core, 73%

Native provider streaming and tool rounds, request shaping, output limits,
reasoning projection, message repair, prompt construction and restore, immutable
per-conversation prompt snapshots, tool/plugin freezing, external-memory turn
callbacks, and bounded client lifecycle are connected to production startup.
Manual compression now makes one real tool-free summary request, strictly
redacts the checkpoint boundary, preserves tool metadata, and defaults to
same-session publication without evicting the frozen-prompt conversation
client. Explicit rotation keeps provider cache identity stable across physical
session segments. Automatic compression now uses the frozen provider-visible
request, resolved context window, output reservation, per-model threshold,
attempt cap, complete head/tail boundaries, and durable retry guards. Native
turns load the full active transcript instead of silently truncating at 40
messages.

Native streaming and non-streaming calls now normalize provider usage into
fresh input, output, cache-read, cache-write, reasoning, and request-count
buckets. Main usage reaches both session totals and the model ledger, while
compression usage remains auxiliary. Count-based proactive pruning is live at
the next admitted pre-turn boundary and immediately after a durable tool-result
batch. Restart-safe hysteresis, minimum reclaim gating, exact transcript CAS,
and end-to-end tests prove that the provider sees the summary instead of the
archived large result, including on the next request in the same turn.

Full compression now starts with Python-compatible token-budget pruning. Its
estimator includes CJK density, multimodal images, full tool-call envelopes,
reasoning text, and Codex replay sidecars. The strict protected-tail boundary,
three pressure-demotion stages, lean and legacy tail budgets, and token-aware
summary tail are connected to live HTTP and push ingress through the guarded
SQLite publication path. A source-executed Python corpus proves 17 estimator,
14 prune, and 3 tail-cut cases, while a live oversized-tool test proves the
rewrite commits before the triggering turn.

Opt-in rolling micro-compaction now runs after the completed assistant reply is
durable and before external-memory completion or the next provider request. It
absorbs one complete assistant/tool exchange per due pass while preserving all
user bytes, rehydrates cumulative state on resume, supersedes only contained
markers, skips a poison exchange after three failures, and gives one-call
defragmentation priority when the rolling summary grows too large. Its bounded,
redacted auxiliary request rejects truncated, empty, tool-call, and
reasoning-only output. A source-executed Python oracle covers 21 state-machine
scenarios, and a live HTTP plus SQLite test proves post-persist ordering,
search semantics, auxiliary usage attribution, and next-request adoption.

Default in-place full compression now runs inside the live tool loop after all
results are durable and before the next provider request. It prefers real
provider prompt usage, falls back to full request sizing, preserves the
post-compaction no-usage sentinel, and rearms attempts only after provider
usage proves recovery. It reuses token-budget Phase 1, durable cooldown and
breaker state, and exact-snapshot publication. The live HTTP and SQLite test
proves persistence-before-summary, adoption-before-follow-up, archived-source
search, and byte-identical system prompts across every main request.

Explicit rotation now runs at that same mid-turn boundary. A shared physical
session authority publishes the child through the existing atomic store,
rebinds the held process-local lease, and routes later tool rows, provider
requests, the final assistant, usage, memory completion, and hook events to the
child. The bounded conversation cache rekeys the exact frozen client instead of
rebuilding it. Committed lineage fallback covers repeated rotations followed by
a callback failure. A live two-tool integration proves the first batch is
durable before rotation and the second batch never writes back to the closed
parent.

Full summary requests now use a separately constructed, conversation-frozen
`auxiliary.compression` route. Startup resolves its provider, model, endpoint,
credentials, request fields, reasoning controls, 300 second timeout floor, and
exact-route fast-lane cap. A two-endpoint test proves that auxiliary controls do
not leak into the main request, truncated output gets one uncapped main retry,
and successful auxiliary output skips that retry.

The same frozen route plan now includes the ordered task-level configured
fallback chain. Explicit auxiliary mode tries one eligible candidate before
its main-model safety route; auto mode tries main first. Selection applies
model-versus-credential failure scope, preserves sibling models and distinct
endpoints, skips known context windows below 64,000 tokens, and calls no more
than one configured candidate per summary attempt. Per-entry timeouts remain
independent of the task floor. Exact-route reasoning controls cannot leak to an
uncertified candidate, and all attempts remain tool-free, non-recursive, and
separately attributed to auxiliary usage.

Auto-mode compression now continues into the main agent's top-level
`fallback_providers` and legacy `fallback_model` policy when the task chain has
no eligible client. Startup merges and deduplicates both sources, filters raw
provider labels, resolves routes in the active profile's secret scope, and
shares one configured-request budget across the task and top-level tiers. An
auto route with task-specific request policy keeps a dedicated frozen client
in the main-first slot instead of being misclassified as explicit mode. The
same prompt bytes cross every route, while top-level per-entry timeout,
reasoning, and output-cap fields remain inert to match Python runtime behavior.

The final auto-mode tier now discovers a strict native subset of Python's
built-in provider chain. OpenRouter, canonical Nous device-code OAuth, the
active chat-compatible custom route, and registered API-key profiles with
native chat-completions models retain their source order. Non-chat transports
remain excluded until their credential and wire managers exist. Candidate clients freeze inside each
conversation's profile secret scope, while one gateway-owned health table keys
quarantine state by profile home and normalized provider. Discovered routes do
not inherit the configured-chain 64K floor. Non-auth failures stop traversal,
and an unrefreshable 401 admits at most one successor. Local HTTP tests prove
request order, health reuse, failure bounds, exact message reuse, and auxiliary
usage identity.

Static API-key discovery candidates now consult the profile/root credential
pool before environment keys. Recovery attributes the failure to the exact
dispatched key, persists duplicate-key cooldowns before retry, and reloads the
store for each mutation so concurrent processes share durable truth. Auth and
payment failures rotate immediately; ordinary 429 responses get one retry on
the dispatched key first. One rotated-key request uses a newly built HTTP
client, and a second credential failure is recorded without another request.
The request body stays byte-identical and tool-free across rotation.

The native main chat-completions route now uses the same durable static-key
foundation. Streaming and tool-loop calls share one bounded dispatcher. Auth
and billing failures rotate immediately, ordinary 429 responses retry the
exact key once, and usage-limit or pre-exhausted 429 responses rotate without
that redundant retry. Each replacement gets a fresh HTTP client and its own
endpoint-specific headers. The shared cursor survives turn clones, while the
already-built request, system prompt, and tool schema remain stable.

Canonical Nous discovery resolves its OAuth material lazily before the first
auxiliary request. Valid inference JWTs avoid both network and shared-lock
contention. Refresh-due callers serialize profile, borrowed-root, and shared
state, adopt a peer rotation when available, persist the new single-use pair
before use, rebuild the request client, and retry the identical summary bytes
once. Reuse descriptions and terminal codes quarantine stale singleton rows.
Portal and network-provided inference routes are allowlisted, while the trusted
operator inference override remains runtime-only.

Structurally impossible full-compression attempts now arm a 300 second
conversation-local monotonic guard instead of incrementing the durable
ineffective breaker. Pre-turn and same-turn paths share the guard, a live
HTTP and SQLite test proves it suppresses summary I/O after the transcript
becomes large enough to compress, and successful or manually forced attempts
clear it. The state is intentionally not persisted.

Manual, pre-turn, and same-turn full compression now share a pure durable
handoff planner. It rehydrates prior summary bodies separately from new turns,
normalizes old standalone and merged carriers, selects roles against
template-visible neighbors, merges collisions into retained tail rows, and
restores a real user anchor or deterministic continuation marker. String and
structured content are supported. The active tool loop suppresses a provider
request if only a reference handoff would drive it, but continues for real user
input and in-flight tool traffic. A 60-case source-executed oracle pins the
role, carrier, anchor, and call-suppression contract.

Every native full-compression path now invokes the version 2 external-memory
checkpoint before summary I/O. The hook sees the complete exact durable
snapshot, while the summary still sees only the selected history plus sanitized,
data-fenced provider context. Required failures stop before any transcript
mutation, optional failures proceed without context, and successful empty
returns still count as completed checkpoints. The wide lifecycle projection
preserves the compressed-summary marker so resumed derivative context is never
misclassified as direct evidence. Preflight, checkpoint, summary, and ordinary
turns reuse the same frozen per-conversation client.

After a full-compression publication commits, every live native path now
notifies the real Python `MemoryManager` of the boundary. Rotation rekeys the
same initialized client from parent to child instead of rebuilding its frozen
prompt and extension process. In-place publication notifies with the same ID.
Protocol failure cannot roll back SQLite and retires the stale host so the next
child turn can recover cleanly. Cache tests cover target contention and a hard
retirement racing the asynchronous observer, while a live Python-child test
proves the exact provider callback.

The same post-publication seam now schedules the exact five-key generic
`session:compress` payload after the memory callback attempt. In-place events
retain Python's empty `old_session_id`; rotation events carry the archived
parent. A clone-shared conversation counter survives frozen-client cache rekeys,
and hook execution never delays or rolls back compression.

Other OAuth providers, pool-only Nous
credentials and interactive login, non-chat auxiliary transports, dynamic provider-plugin
discovery, the general auxiliary client cache, context-engine and
relay-boundary notifications, overflow recovery, broader provider failover loops,
delegation/subagents, full
smart approval and clarification flows, memory and plugin managers, skill execution,
context invalidation policy, and several agent-loop recovery behaviors remain.
These are large behavioral systems, which is why the core score remains low
despite broad helper and oracle coverage.

## Why test and line counts are not the percentage

The workspace currently has 1,845 passing Rust tests and two expected ignores
(1,844 gateway plus one core test).
That is not a valid denominator against the Python product. Differential tests
can thoroughly prove a narrow helper while a large runtime consumer is still
missing. Likewise, Python contains adapters, UIs and compatibility code that do
not map line-for-line to Rust. Only a wired capability receives full credit.

## Current proof and uncertainty

- Full Rust workspace: 1,845 passed, two ignored (1,844 gateway plus one core).
- Selected Python main-provider classifier, credential-pool, provider-boundary,
  and runtime-resolution contracts: 256 passed.
- Source-executed differential corpora: 17 estimator, 14 pruning, 3
  tail-selection, 21 micro-compaction state-machine, 25 same-turn decision and
  adoption, 129 auxiliary routing/config, 112 task fallback-chain, 76 top-level
  fallback-chain, 97 built-in discovery, 64 compression credential-recovery,
  86 main-provider credential-recovery, 87 Nous OAuth, 24
  structural-backoff, and 60 handoff-layer cases, plus the 233-case terminal and approval corpus, 76
  interactive-approval cases, and 251 exhaustive dangerous-command cases.
- Rust and Python formatting, Ruff, Clippy with warnings denied, and
  `git diff --check`: passed.
- The lower end of the range assumes the versioned extension-host boundary earns
  little tool-runtime credit until native managers and built-ins use it broadly.
- The upper end gives more credit to the now-live prompt, memory and session
  lifecycle verticals, but still does not count Python fallback behavior.

## What moves the estimate next

1. Remaining OAuth providers, general provider fallback, remaining discovery
   transports, overflow recovery, repeated-compression status, and
   context-engine adoption complete the current compression cluster.
2. Native plugin and memory managers replace the now-recoverable compatibility
   host with a broader native production capability.
3. Smart and Tirith approval, PTY and remote terminal modes, then file, browser,
   MCP and delegation execution move the largest remaining weighted area.

Older audits remain useful historical snapshots, but their percentages are
superseded by this document.
