# Native System Prompt Mapping & Profile Invariants

Maintainer update (2026-09-07): NativeAgentClient now accepts an immutable
assembled system prompt and supplies it to streaming/tool-loop requests. Real
HTTP tests cover exact bytes across turns and stable keys within tool rounds.
SessionDb::update_system_prompt now performs Python's insert/pointer-update/GC
transaction with null/empty, shared-body and rollback tests. Full assembly and
restore/build lifecycle are still missing, so production does not yet install
the new prompt input. Treat the map below as source navigation, not proof that
all listed invalidation paths or loaders have been ported.

The stored-runtime identity guard is now ported in system_prompt.rs and verified
against 924 executions of agent/conversation_loop.py's actual function. It uses
Python-compatible splitlines and anchored cwd lookup. This does not yet include
Bot Chat capability epochs or the full restore/build lifecycle.

PromptParts tier finalization/join and the model-guidance gate are now ported.
Their inline test checks 64 tier combinations and 240 guidance settings against
AST execution of Python's actual finalizer/join/enforcement code. These are
assembly primitives; the complete ordered section selection is still pending.

## Python Assembly Order (`agent/system_prompt.py`)
Joined with `\n\n` across three cache tiers:
1. **Stable Tier** (cross-session prefix):
   - **Identity**: `load_soul_md` (`_agent_home`) -> fallback `DEFAULT_AGENT_IDENTITY` (L476-488).
   - **Help Guidance**: `HERMES_AGENT_HELP_GUIDANCE_NO_SKILLS` (L498), upgraded to `HERMES_AGENT_HELP_GUIDANCE` if `skill_view` exists and `hermes-agent` skill is in the index (L655-656).
   - **Universal Guidance**: `TASK_COMPLETION_GUIDANCE` (L506-508) and `PARALLEL_TOOL_CALL_GUIDANCE` (L516-518) when tools are loaded.
   - **Tool Behavioral Guidance**: `MEMORY_GUIDANCE` / `USER_PROFILE_GUIDANCE`, `SESSION_SEARCH_GUIDANCE`, `SKILLS_GUIDANCE`, `KANBAN_GUIDANCE` (L521-552).
   - **Steer Channel Note**: `STEER_CHANNEL_NOTE` when tools are present (L556-557).
   - **Tool-Use Enforcement & Operations**: `TOOL_USE_ENFORCEMENT_GUIDANCE` (L566-582) + `GOOGLE_MODEL_OPERATIONAL_GUIDANCE` for Gemini/Gemma models (L583-586).
   - **Execution Discipline**: `OPENAI_MODEL_EXECUTION_GUIDANCE` via `execution_guidance_text` (L601-617).
   - **Provider Workarounds**: Alibaba model-id workaround (`agent.provider == "alibaba"`, L663-670).
   - **Environment Hints**: OS / WSL / host details via `build_environment_hints` (L675-677).
   - **Coding Posture Prefix**: `coding_prefix_parts` from `coding_system_prompt_parts` (L686-696).
   - **Post-Workspace Guidance** (in stable tier if no workspace snapshot; deferred to context tier if snapshot present): Python toolchain probe (L717-725), Bot Mode protocol & capability epoch (L735-760), Active profile hint (L776-826), Platform hint with overrides / TUI clarifier (L827-869).
2. **Context Tier** (workspace/session dynamic):
   - **Coding Snapshot & Post-Workspace**: `coding_workspace_parts` + `coding_trailing_parts` + deferred post-workspace parts if snapshot was present (L874-878).
   - **Caller Message**: Optional `system_message` (L881-883).
   - **Context Files**: `build_context_files_prompt` discovering `AGENTS.md`, `.cursorrules`, etc. under resolved context cwd, with threat scanner, dynamic context caps, and install-tree guards (L884-910).
3. **Volatile Tier** (rebuild tail):
   - **Skills Index**: `build_skills_system_prompt` scoped to profile skills directory (L925-926).
   - **Memory Snapshot**: `_memory_store.format_for_system_prompt` for `memory` (MEMORY.md) and `user` (USER.md) (L928-937).
   - **External Memory**: `_memory_manager.build_system_prompt` (L943-954).
   - **Plugin Sections**: Frozen plugin sections for `after_memory` (L959-961).
   - **Timestamp & Metadata**: `Conversation started: ...` (from `_session_start_like` using lineage root / session ID date), optional cross-day rebuild date, timeless Bot Chat mode, `Session ID`, `Model`, `Provider`, `Platform` (L963-1027).

## Invariants & Profile Ownership
- **Build-Once & DB Reuse**: Built on first turn via `_restore_or_build_system_prompt`, stored in `_cached_system_prompt`, and persisted to `session_db`. Verbatim DB reuse across subsequent turns keeps upstream prefix caches warm (`agent/conversation_loop.py:993-1227`).
- **Profile Ownership**: Assets (`SOUL.md`, `<home>/skills/`, `<home>/memories/`, plugins, active profile name) resolve from `_agent_home` (`HERMES_HOME` ContextVar override or `_session_db.db_path.parent`), preventing launch profile leakage.
- **SOUL.md Alone is Insufficient**: `SOUL.md` only covers identity in Tier 1. Full prompt parity requires behavioral guidance, tool enforcement, execution discipline, environment/platform hints, context files, skills index, memory, and session metadata.
- **Strict Invalidation Boundaries**: Rebuild occurs ONLY on context compression (`invalidate_system_prompt`), runtime identity drift (`_stored_prompt_matches_runtime` checking model/provider/platform/cwd), or Bot Chat capability epoch mismatch.
- **Static Prefix Reconstruction**: `_cached_system_prompt_static` is reconstructed on resume/failover for 2-tier caching, guarded by `stored.startswith(static)` without altering stored prompt bytes (`agent/system_prompt.py:1088-1140`).
- **Ephemeral Isolation**: `ephemeral_system_prompt` and channel overrides are injected at API call-time only and NEVER written to `session_db` or cached system prompts (`agent/system_prompt.py:879-880`).

## Reusable Rust Code
- `rust/crates/hermes-gateway/src/session_db.rs`: Content-addressed `system_prompts` table with SHA-256 hash deduplication (L674-684, L1006) and `COALESCE(sp.prompt, s.system_prompt) AS _system_prompt_resolved` (L372, L993).
- `rust/crates/hermes-gateway/src/profile_name.rs`: `active_profile_name` (L92), `normalize_profile_name` (L46), `validate_profile_name` (L75) for profile root mapping.
- `rust/crates/hermes-gateway/src/prompt_cache.rs`: `static_prompt_instructions` (L273) extracting static prefix for cache key routing.
- `rust/crates/hermes-gateway/src/threat_patterns.rs`: `scan_for_threats` (L1-40) for security scanning of discovered context files.
- `rust/crates/hermes-gateway/src/config_types.rs`: `ChannelOverride.system_prompt` (L305-309).
- `rust/crates/hermes-gateway/src/native_agent.rs`: `build_messages_with_content` (L54-65) message projector.

## Missing Rust Components
- **System Prompt Builder Engine**: No 3-tier prompt builder (`build_system_prompt_parts`) or static guidance catalog (`TASK_COMPLETION_GUIDANCE`, `EXECUTION_GUIDANCE`, `GOOGLE_MODEL_OPERATIONAL_GUIDANCE`, etc.).
- **Profile-Scoped Asset Resolvers**: Missing loaders for `<profile_home>/SOUL.md`, `<profile_home>/skills/`, `<profile_home>/memories/`, and active profile prompt text.
- **Context & Memory Crawlers**: Missing context file discovery with timeouts/threat scans (`build_context_files_prompt`), git workspace snapshotter, and memory store formatters.
- **Session DB Update API**: `session_db.rs` lacks `update_system_prompt` (Python `hermes_state.py:9621`) to persist freshly assembled prompts.
- **Native Agent Lifecycle Wiring**: `native_agent.rs` currently lacks first-turn restore/build logic, runtime drift validation, and system prompt injection into outgoing API requests.
