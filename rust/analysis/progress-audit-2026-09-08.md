# Full Rust port progress audit, 2026-09-08

Current estimate: **41% of the full native replacement**, with a reasonable
range of **39% to 43%**. This supersedes the 33% audit from 2026-09-07.

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
| Gateway | 35% | 61% | 21.35 |
| Tool runtime and RPC | 30% | 12% | 3.60 |
| State and search | 15% | 63% | 9.45 |
| Native agent core | 20% | 33% | 6.60 |
| Total | 100% | | **41.00** |

`0.35 * 61 + 0.30 * 12 + 0.15 * 63 + 0.20 * 33 = 41.00`

The arithmetic is exact. The four completion inputs are bounded judgments based
on production wiring and remaining Python surfaces, so reporting more than a
small range would imply false precision.

## Evidence behind the scores

### Gateway, 61%

Production startup, profile-aware client construction, HTTP and push dispatch,
Telegram, Discord and Slack, session admission, delivery state, routing,
recovery, reset, destructive confirmation, secure resume, external-memory turn
lifecycle, and bounded conversation ownership are live and tested. The current
checkpoint added atomic resume, DM ownership, named and full listings, and
in-flight route fencing.

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

### State and search, 63%

SQLite history, structured content replay, FTS foundations, route persistence,
legacy recovery, peer ownership, lineage, conversation generations, prompt
deduplication, frozen tool/plugin snapshot fields, lifecycle closure, reset and
atomic resume are live. The injected rollback test proves the newest multi-row
transition.

Full schema and migration parity, session search projection, archive/pin/read
state, pruning/export/import, topic bindings, title provenance and auto-title,
broader transcript operations, cron state, and several desktop/session queries
remain.

### Native agent core, 33%

Native provider streaming and tool rounds, request shaping, output limits,
reasoning projection, message repair, prompt construction and restore, immutable
per-conversation prompt snapshots, tool/plugin freezing, external-memory turn
callbacks, and bounded client lifecycle are connected to production startup.

Real compression and compaction, provider failover and credential retry loops,
delegation/subagents, full approval/clarification flows, memory and plugin
managers, skill execution, context invalidation policy, and several agent-loop
recovery behaviors remain. These are large behavioral systems, which is why the
core score stays near one third despite broad helper and oracle coverage.

## Why test and line counts are not the percentage

The workspace currently has 1,541 passing Rust tests and two expected ignores,
and the Rust crates contain roughly 124,000 lines including tests and fixtures.
Neither is a valid denominator against the Python product. Differential tests
can thoroughly prove a narrow helper while a large runtime consumer is still
missing. Likewise, Python contains adapters, UIs and compatibility code that do
not map line-for-line to Rust. Only a wired capability receives full credit.

## Current proof and uncertainty

- Full Rust workspace: 1,541 passed, two ignored.
- Selected Python `/resume` and continuation contract: 35 passed.
- Formatting, Clippy with warnings denied, and `git diff --check`: passed.
- The lower end of the range assumes native extension-host functionality earns
  little tool-runtime credit until managers and built-ins use it broadly.
- The upper end gives more credit to the now-live prompt, memory and session
  lifecycle verticals, but still does not count Python fallback behavior.

## What moves the estimate next

1. Native title persistence and real compression boundaries complete the
   current session-lifecycle cluster.
2. Transparent extension-host recovery plus native plugin and memory managers
   turn the existing protocol into a broader production capability.
3. Native terminal/file/browser/MCP and approval/delegation execution move the
   largest remaining weighted area.

Older audits remain useful historical snapshots, but their percentages are
superseded by this document.
