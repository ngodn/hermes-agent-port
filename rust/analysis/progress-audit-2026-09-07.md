# Full Rust port progress audit, 2026-09-07

Current estimate: **33% of the full native replacement**. The weighted result
is 33.3%, rounded because the completion scores are engineering judgments,
not measured coverage. This supersedes the previous 28% estimate.

Scope: HEAD f1859c87cd plus the validated working-tree changes. The unfinished
skill snapshot load/write methods are excluded. Frontend TypeScript stays;
replacing its Python backend, native CLI behavior, plugins, scheduled jobs,
tools and execution environments remains in scope. External Claude/Gemini CLI
capabilities and the Python subprocess bridge do not count as native Rust work.

## Calculation

The four scope weights are unchanged from the September 6 audit. Tested support
code earns partial credit, but a ported policy or renderer is not a completed
runtime feature. Gateway scores cover messaging orchestration, state scores
cover persistence/search, core scores cover agent behavior, and tool/RPC scores
cover executing capabilities and exposing backend protocols.

| Area | Full-port weight | Estimated area completion | Overall percentage points |
| --- | ---: | ---: | ---: |
| Gateway | 35% | 50% | 17.5 |
| Tool runtime and RPC | 30% | 8% | 2.4 |
| State and search | 15% | 52% | 7.8 |
| Native agent core | 20% | 28% | 5.6 |
| Total | 100% | | **33.3** |

`0.35 * 50 + 0.30 * 8 + 0.15 * 52 + 0.20 * 28 = 33.3`

These weights are a scope model, not a complete feature census or an estimate
of hours remaining. Arithmetic is exact; the inputs are approximate. Test,
file, fixture and line counts are not used as a completion denominator.

## Evidence behind the scores

- **Gateway, 50%:** Startup, dispatch, session admission, durable delivery,
  routing and recovery are substantial. Three live push adapters and inbound
  audio download/STT paths exist. Broad adapter coverage, full commands,
  streaming delivery, room/queue/interrupt orchestration and API behavior remain.
  [Startup](../crates/hermes-gateway/src/main.rs) registers Telegram, Discord
  and Slack and six HTTP routes. [Dispatch](../crates/hermes-gateway/src/dispatch.rs)
  wires transcription and session ownership but buffers output into a final
  reply. [Slash handlers](../crates/hermes-gateway/src/slash.rs) still implement
  only help, whoami and status. Recognition/catalog metadata is not handler parity.
- **Tool runtime/RPC, 8%:** The native tool loop, validation, replay and result
  handling work. Startup still installs only CurrentTimeTool in
  [main.rs](../crates/hermes-gateway/src/main.rs). Other concrete tools in
  [native_tools.rs](../crates/hermes-gateway/src/native_tools.rs) are test fixtures.
  Native terminal/file/browser tools, execution environments, MCP, backend RPC,
  plugin execution and approval/delegation execution account for most of this
  area and remain pending. Toolset and plugin-prompt helpers get limited credit.
- **State/search, 52%:** [SessionDb](../crates/hermes-gateway/src/session_db.rs)
  and [SessionStore](../crates/hermes-gateway/src/session_store.rs) provide real
  SQLite history, routing persistence, legacy adoption, lineage, lifecycle and
  shared handles. Session ownership is used by live ingress. Full schema and
  migration parity, CJK search, broader transcript operations, topic bindings,
  archive/pruning and handoff behavior remain when compared with
  [hermes_state.py](../../hermes_state.py). This earns the largest increase
  from the earlier audit's 35% area score.
- **Native core, 28%:** [NativeAgentClient](../crates/hermes-gateway/src/native_agent.rs)
  supports native chat/SSE, tool rounds and provider request configuration.
  Prompt, memory, skills and profile support have extensive parity work, but
  production startup does not call with_system_prompt or assemble
  [ResolvedPromptSections](../crates/hermes-gateway/src/system_prompt.rs).
  The immutable prompt path is tested, not a completed production lifecycle.
  Live memory mutation, complete skills loading/execution, compression,
  provider failover/transports and delegation remain. Startup still shares one
  agent; a tested profile factory is not live per-profile agent multiplexing.

## Comparison and validation

The previous estimate was 27.5%, rounded to 28%, using area scores
45 / 5 / 35 / 25. The revised scores add 1.75 gateway points, 0.9 tool/RPC
points, 2.55 state/search points and 0.6 core points: **5.8 overall points**.
This comparison includes reassessment of the current code, not just work
performed in one session. Prompt helper work matters, but cannot receive full
feature credit before its production consumers exist.

Gemini independently proposed the same 33.3% weighted estimate in
[its review](progress-independent-2026-09-07.md). The primary agent checked
startup, dispatch, slash handlers, state APIs and prompt call sites directly.
That review is supporting analysis, not authoritative source: its statement
that with_system_prompt is dead code should be read as absent from production
startup, since it is used in tests. State scope above uses the actual Python
state API rather than assuming cron and memory are SessionDb tables.

Last completed workspace validation: **1,449 passed, two ignored**, from
`/tmp/hermes-skill-manifest-workspace.log` (1 core plus 1,448 gateway tests).
This is a prior completed run, not a fresh validation of the unfinished snapshot
edits. This audit changes documentation only and makes no live deployment claim.
