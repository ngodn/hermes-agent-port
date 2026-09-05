# Full Rust port progress audit, 2026-09-06

Latest handoff estimate: **about 28%, roughly 25-35%**, recorded in
[PORT.md](../PORT.md). Subsequent credential/provider groundwork raises the
subjective core estimate from 20% to 25%; the same weights yield 27.5%. The
original audit below is retained as the baseline and is not a measured census.

Original audit estimate: **27% of the full native replacement**, with substantial
uncertainty (roughly 20–35%). This is an engineering scope estimate, not a
measured percentage of functions, source lines, tests, or checked boxes.

The assessment uses the four phases in PORT.md. It gives partial credit to
verified support code, while reserving completion credit for real runtime
integration. The weights and phase estimates below are explicit judgments about
remaining scope; they are not measured effort or statistical confidence bounds.
Frontend TypeScript is retained per the plan, but its Python backend/RPC work
belongs to the tool/RPC and agent phases. Native command, plugin, scheduled-job,
and backend support are included in remaining scope.

| Phase | Scope weight | Estimated completion | Evidence and remaining scope |
| --- | ---: | ---: | --- |
| Gateway | 35% | 45% | Runnable dispatch, lifecycle, history/delivery integration, substantial configuration/media/session/relay support. Three push adapters wired. Full adapter coverage, rich inbound pipeline, streaming delivery orchestration, slash handlers, API-server routes, queue/interrupt/room orchestration remain. |
| Tool runtime and RPC | 30% | 5% | Tool-call interface and validation/replay loop work. Startup registers only current_time. Native terminal/file/browser tools, tool registry/discovery, MCP/RPC hosts, environment backends, plugin execution, approvals and delegation execution remain. External Claude/Gemini/Python backends do not count as native implementations of those capabilities. |
| State and search | 15% | 35% | SQLite sessions, append/load, FTS search, delivery obligations and recovery exist. Full schema/migrations, richer transcript replay, state-management operations and CJK parity remain. |
| Native agent core | 20% | 20% | Native chat/SSE/tool iteration, several provider/request policies, caching keys, validation, summaries and bounded recovery exist. Full prompt construction, memory/skills, compression, provider transports/failover, credential pools, interruption, verification and delegation lifecycle remain. |

Calculation: 0.35 × 45 + 0.30 × 5 + 0.15 × 35 + 0.20 × 20 = **26.5%**, rounded
to **27%**. The decimal is arithmetic, not precision in the underlying estimate.
The earlier conversational estimate was insufficiently grounded. Recalibrating
it does not mean recent work was lost.

## Sources inspected in this audit

- `src/main.rs`: registers Telegram, Discord and Slack push paths; six server
  routes; native tool startup registers `CurrentTimeTool` only.
- `src/dispatch.rs`: real session lease, history, delivery and ledger integration;
  no complete rich-media/queued-event runner yet.
- `src/slash.rs`: three built-ins, help/whoami/status; other allowed commands
  flow to the agent instead of native command handlers.
- `src/session_db.rs`: concrete persistence and search API, with simplified
  replay compared with the reference state layer.
- `src/native_agent.rs`, `src/native_tools.rs`: current request, streaming,
  tool-loop, replay and recovery consumers.
- PORT.md's phase order, handoff table, deferred section and older status list.
  The older list contains stale qualifications: e.g. it still says the delivery
  ledger is unwired, while Dispatcher now calls it. Current code wins.
- Current Rust tree: 143 .rs files, 140 in the gateway. These counts are evidence
  of code volume only and are not used in the completion calculation.
- Python `gateway/run.py` remains a 34,847-line, 647-function reference hub;
  `gateway/slash_commands.py` has 101 functions and `gateway/stream_consumer.py`
  has 61. Python `tools/` contains 160 .py files. These demonstrate remaining
  breadth, not one-to-one Rust port targets.

The audit includes current uncommitted work after commit `75aad17d8e`, including
stream filtering, transcription HTTP and rejected-container conversion. Current
validation: 1,209 default workspace tests passed, two ignored. The optional
FFmpeg/HTTP integration test was also explicitly run and passed (four retry
scenarios). Formatting and Clippy with warnings denied pass. Test totals are
verification evidence for implemented behavior, not a progress denominator.
