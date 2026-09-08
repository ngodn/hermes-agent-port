# Full Rust port progress audit, 2026-09-08

Current estimate: **42% of the full native replacement**, with a reasonable
range of **40% to 44%**. This supersedes the 33% audit from 2026-09-07 and the
41% estimate recorded before native titles and manual compression landed.

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
| Gateway | 35% | 62% | 21.70 |
| Tool runtime and RPC | 30% | 12% | 3.60 |
| State and search | 15% | 64% | 9.60 |
| Native agent core | 20% | 35% | 7.00 |
| Total | 100% | | **41.90** |

`0.35 * 62 + 0.30 * 12 + 0.15 * 64 + 0.20 * 35 = 41.90`

The arithmetic is exact. The four completion inputs are bounded judgments based
on production wiring and remaining Python surfaces, so reporting more than a
small range would imply false precision.

## Evidence behind the scores

### Gateway, 62%

Production startup, profile-aware client construction, HTTP and push dispatch,
Telegram, Discord and Slack, session admission, delivery state, routing,
recovery, reset, destructive confirmation, secure resume, external-memory turn
lifecycle, and bounded conversation ownership are live and tested. The newest
checkpoints added atomic resume, DM ownership, named and full listings, manual
title controls, manual compression on HTTP and push ingress, cross-process turn
queuing, and durable route refresh after compression.

Most of the Python gateway breadth remains: many platform adapters, command
handlers, queue/steer/interrupt behavior, richer streaming delivery, adapter
callbacks, topic management, and desktop/TUI protocol parity. Three substantial
native adapters do not represent the roughly twenty Python platforms.

### Tool runtime and RPC, 12%

The native model tool loop, schema projection, malformed-call repair, duplicate
suppression, result framing, event emission, iteration-summary path, and a
persistent Python extension-host protocol exist. Startup can expose an
extension-backed tool surface, but the native built-in tool catalog remains
minimal and startup still registers only the small core fixture-level surface.

Terminal, file, browser, web, MCP, execution-environment backends, approval
runtime, delegation execution, most service tools, plugin discovery/management,
and full backend RPC account for most of this weighted area and remain.

### State and search, 64%

SQLite history, structured content replay, FTS foundations, route persistence,
legacy recovery, peer ownership, lineage, conversation generations, prompt
deduplication, frozen tool/plugin snapshot fields, lifecycle closure, reset,
atomic resume, title provenance, compression lineage, durable turn leases, and
atomic child-first compression are live. Injected rollback and two-connection
contention tests prove the newest multi-row transitions.

Full schema and migration parity, session search projection, archive/pin/read
state, pruning/export/import, topic bindings, auto-title,
broader transcript operations, cron state, and several desktop/session queries
remain.

### Native agent core, 35%

Native provider streaming and tool rounds, request shaping, output limits,
reasoning projection, message repair, prompt construction and restore, immutable
per-conversation prompt snapshots, tool/plugin freezing, external-memory turn
callbacks, and bounded client lifecycle are connected to production startup.
Manual rotation compression now makes one real tool-free summary request,
strictly redacts the checkpoint boundary, preserves tool metadata, and keeps
provider cache identity stable across physical session segments.

Automatic and in-place compression, pruning/micro-compaction, auxiliary summary
model routing, provider failover and credential retry loops,
delegation/subagents, full approval/clarification flows, memory and plugin
managers, skill execution, context invalidation policy, and several agent-loop
recovery behaviors remain. These are large behavioral systems, which is why the
core score remains low despite broad helper and oracle coverage.

## Why test and line counts are not the percentage

The workspace currently has 1,570 passing Rust tests and two expected ignores.
That is not a valid denominator against the Python product. Differential tests
can thoroughly prove a narrow helper while a large runtime consumer is still
missing. Likewise, Python contains adapters, UIs and compatibility code that do
not map line-for-line to Rust. Only a wired capability receives full credit.

## Current proof and uncertainty

- Full Rust workspace: 1,570 passed, two ignored.
- Selected Python compression and cross-process lease contract: 67 passed.
- Formatting, Clippy with warnings denied, and `git diff --check`: passed.
- The lower end of the range assumes native extension-host functionality earns
  little tool-runtime credit until managers and built-ins use it broadly.
- The upper end gives more credit to the now-live prompt, memory and session
  lifecycle verticals, but still does not count Python fallback behavior.

## What moves the estimate next

1. Automatic/in-place compression, auxiliary model policy, and checkpoint hooks
   complete the current session-lifecycle cluster.
2. Transparent extension-host recovery plus native plugin and memory managers
   turn the existing protocol into a broader production capability.
3. Native terminal/file/browser/MCP and approval/delegation execution move the
   largest remaining weighted area.

Older audits remain useful historical snapshots, but their percentages are
superseded by this document.
