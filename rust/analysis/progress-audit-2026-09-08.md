# Full Rust port progress audit, 2026-09-08

Current estimate: **46% of the full native replacement**, with a reasonable
range of **44% to 48%**. This supersedes the 45% estimate recorded after native
provider usage, incremental tool history, and next-turn proactive pruning
landed.

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
| Gateway | 35% | 64% | 22.40 |
| Tool runtime and RPC | 30% | 13% | 3.90 |
| State and search | 15% | 70% | 10.50 |
| Native agent core | 20% | 44% | 8.80 |
| Total | 100% | | **45.60** |

`0.35 * 64 + 0.30 * 13 + 0.15 * 70 + 0.20 * 44 = 45.60`

The arithmetic is exact. The four completion inputs are bounded judgments based
on production wiring and remaining Python surfaces, so reporting more than a
small range would imply false precision.

## Evidence behind the scores

### Gateway, 64%

Production startup, profile-aware client construction, HTTP and push dispatch,
Telegram, Discord and Slack, session admission, delivery state, routing,
recovery, reset, destructive confirmation, secure resume, external-memory turn
lifecycle, and bounded conversation ownership are live and tested. Native HTTP
and push admission now run automatic compression before inbound persistence,
under stable route, transcript, and cross-process lineage leases. Both default
in-place publication and explicit rotation are exercised end to end.

Most of the Python gateway breadth remains: many platform adapters, command
handlers, queue/steer/interrupt behavior, richer streaming delivery, adapter
callbacks, topic management, and desktop/TUI protocol parity. Three substantial
native adapters do not represent the roughly twenty Python platforms.

### Tool runtime and RPC, 13%

The native model tool loop, schema projection, malformed-call repair, duplicate
suppression, result framing, event emission, iteration-summary path, and a
persistent Python extension-host protocol exist. Startup can expose an
extension-backed tool surface, but the native built-in tool catalog remains
minimal and startup still registers only the small core fixture-level surface.
Live native tool rounds now persist assistant call rows before side effects,
persist each result before the next provider request, and replay complete tool
groups on later turns. Deterministic old-result pruning can summarize those
durable rows without changing their call/result identity.

Terminal, file, browser, web, MCP, execution-environment backends, approval
runtime, delegation execution, most service tools, plugin discovery/management,
and full backend RPC account for most of this weighted area and remain.

### State and search, 70%

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

Full schema and migration parity, session search projection, archive/pin/read
state, pruning/export/import, topic bindings, auto-title,
broader transcript operations, cron state, and several desktop/session queries
remain.

### Native agent core, 44%

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

Token-budget tail selection, pressure demotion, micro-compaction, same-turn LLM
summary compression, auxiliary summary model routing, provider failover and
credential retry loops,
delegation/subagents, full approval/clarification flows, memory and plugin
managers, skill execution, context invalidation policy, and several agent-loop
recovery behaviors remain. These are large behavioral systems, which is why the
core score remains low despite broad helper and oracle coverage.

## Why test and line counts are not the percentage

The workspace currently has 1,621 passing Rust tests and two expected ignores
(1,620 gateway plus one core test).
That is not a valid denominator against the Python product. Differential tests
can thoroughly prove a narrow helper while a large runtime consumer is still
missing. Likewise, Python contains adapters, UIs and compatibility code that do
not map line-for-line to Rust. Only a wired capability receives full credit.

## Current proof and uncertainty

- Full Rust workspace: 1,621 passed, two ignored (1,620 gateway plus one core).
- Selected Python usage, pruning, restart-safety, wiring, and incremental
  persistence contract: 91 passed.
- Formatting, Clippy with warnings denied, and `git diff --check`: passed.
- The lower end of the range assumes native extension-host functionality earns
  little tool-runtime credit until managers and built-ins use it broadly.
- The upper end gives more credit to the now-live prompt, memory and session
  lifecycle verticals, but still does not count Python fallback behavior.

## What moves the estimate next

1. Token-budget pressure demotion, micro-compaction, same-turn full compression,
   auxiliary model policy, and checkpoint hooks complete the current
   compression cluster.
2. Transparent extension-host recovery plus native plugin and memory managers
   turn the existing protocol into a broader production capability.
3. Native terminal/file/browser/MCP and approval/delegation execution move the
   largest remaining weighted area.

Older audits remain useful historical snapshots, but their percentages are
superseded by this document.
