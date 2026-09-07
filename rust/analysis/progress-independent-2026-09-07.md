# Independent Native Hermes Rust Port Progress Audit (2026-09-07)

Overall progress estimate: **33%** (rough engineering scope range 30% to 36%), compared to 27% to 28% baseline in [progress-audit-2026-09-06.md](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/progress-audit-2026-09-06.md) and [PORT.md](file:///home/eins0fx/development/hermes-agent-port/rust/PORT.md).
Assessment evaluates functional scope completion against the Python reference, separating offline verified helpers from production runtime wiring. Percentages reflect scope judgments rather than line counts or test tallies.

| Area | Scope Weight | Estimated Completion | Concrete Code Evidence and Integration Status |
| --- | ---: | ---: | --- |
| Gateway | 35% | 50% | Live: Inbound STT pipeline wired end-to-end; Slack adapter deepened (Block Kit, multi-token, routing); [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) active in [`main`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L460) and [`Dispatcher`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L30). Unwired: streaming outbound delivery buffers to single reply; only 3 of 10+ platforms implemented; slash commands mostly fallback; API server routes minimal. |
| Tool runtime and RPC | 30% | 8% | Live: Only [`CurrentTimeTool`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L717) registered at startup in [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L373); tool-calling loop executes in [`run_tool_loop`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L693). Helper: [`toolset_resolution.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/toolset_resolution.rs) and [`command_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/command_catalog.rs). Unwired: No native bash/terminal, file, browser, MCP host, plugin execution, approval engine, or delegation runtime. |
| State and search | 15% | 52% | Live: Full [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) runtime integration in [`dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L298); legacy session adoption via [`claim_legacy_gateway_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1361); shared handle registry via [`open_shared`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L764). Helper: Compression walks, peer discovery, recovery pruning. Unwired: Broader state schema (cron, memories) and CJK FTS5 tokenizer parity. |
| Native agent core | 20% | 28% | Live: Chat completions, output caps, tool replay, and credential pool for STT. Helper: Extensive prompt tier assemblies ([`system_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs), [`skills_index.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_index.rs), [`memory_snapshot.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/memory_snapshot.rs)). Unwired: [`with_system_prompt`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L248) is dead code; turns run without system prompt, memory, skills, compaction, or delegation. |

Calculation: 0.35 * 50 + 0.30 * 8 + 0.15 * 52 + 0.20 * 28 = **33.3%**, rounded to **33%**.

## Area 1: Gateway (35% Weight, 50% Complete)
- **Live Integration:**
  - Audio STT pipeline: [`TelegramAdapter`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/telegram.rs#L31), [`DiscordAdapter`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/discord.rs#L34), and [`SlackAdapter`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slack.rs#L173) fetch attachments into `audio_paths`. [`build_gateway_transcription`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/transcription_http.rs#L532) instantiates real HTTP STT, and [`handle_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L225) transcribes voice notes before execution.
  - Ingress and routing: [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) is constructed in [`main`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L564) and called in [`handle_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L319). Turn serialization uses [`SessionTurnLeaseRegistry`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/turn_lease.rs#L93), and [`run_admitted_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L383) owns task completion independently of client cancellation.
  - Slack adapter: Auth testing, token maps, deduplication, Block Kit serialization ([`slack_blocks.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slack_blocks.rs)), and channel policy.
- **Remaining Scope:**
  - Outbound delivery in [`Dispatcher`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L409) buffers the entire turn into a single string. Partial message chunks, tool chrome, and gateway notices are discarded.
  - Missing adapters: WhatsApp, Signal, QQBot, WeChat, and Matrix are unbuilt or stubs.
  - Slash commands: Only 3 built-in commands exist in [`slash.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/slash.rs#L84); other commands fall through to the agent model.
  - API server: [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L679) registers 6 basic endpoints; the full `/v1/runs` and room execution policy are unported.

## Area 2: Tool Runtime and RPC (30% Weight, 8% Complete)
- **Live Integration:**
  - [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L193) registers only [`CurrentTimeTool`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L717) at startup ([`main.rs:L373`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L373)).
  - [`run_tool_loop`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L693) and [`parse_message_step`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L105) handle function calling, argument repairs, duplicate suppression, and iteration limits.
- **Helper Ports:**
  - [`toolset_resolution.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/toolset_resolution.rs) provides offline toolset graph resolution and alias normalization.
  - [`command_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/command_catalog.rs) generated static metadata for 101 commands.
- **Remaining Scope:**
  - No native implementations for core tools: bash/terminal, filesystem operations, browser/CDP automation.
  - No MCP host or client implementation; external subagents or CLI tools do not count as native tool support.
  - No plugin execution environment, RPC server/host, user approval interceptor, or subagent delegation runtime.

## Area 3: State and Search (15% Weight, 52% Complete)
- **Live Integration:**
  - Active [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) orchestrates routing flights, candidate creation, and state updates during turns.
  - [`SessionDb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L159) handles multi-turn persistence, content encoding ([`encode_message_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L57)), shared multi-handle caching ([`open_shared`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L764)), and legacy session adoption ([`claim_legacy_gateway_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1361)).
  - Delivery ledger records and settles delivery obligations.
- **Helper Ports:**
  - Lineage walk ([`session_lineage_root_to_tip`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L590)), compression tip discovery, and recovery pruning in [`session_routing.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs).
- **Remaining Scope:**
  - Full schema migrations for other state domains (cron schedules, user memory tables, multi-profile sync).
  - CJK FTS5 tokenizer parity (standard FTS5 SQLite search only).

## Area 4: Native Agent Core (20% Weight, 28% Complete)
- **Live Integration:**
  - Streaming completions and non-streaming tool loop via [`NativeAgentClient`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L193).
  - Provider profile headers, reasoning parameters, and custom request bodies configured from user settings.
  - Credential pool resolution wired for STT.
- **Helper Ports (Tested Offline, Not Integrated in Production):**
  - Prompt assembly components: [`system_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs), [`skills_index.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_index.rs), [`memory_snapshot.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/memory_snapshot.rs), [`coding_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/coding_prompt.rs), [`bot_mode.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/bot_mode.rs), [`context_files.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/context_files.rs), and [`prompt_footer.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/prompt_footer.rs).
  - In production, [`build_agent_client_for_home`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L212) never calls [`with_system_prompt`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L248). The method remains marked `#[allow(dead_code)]`.
- **Remaining Scope:**
  - Production prompt construction and lifecycle wiring into live agent turns.
  - Live memory mutations, skills loading and execution, context compression/compaction, provider failover, and subagent delegation.
