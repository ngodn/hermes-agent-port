# Per-Profile Agent Runtime Construction & Provider Factory Map

Maintainer checks (2026-09-07): build_agent_client_for_home now accepts an
explicit profile home and reads its .env once per native client construction.
The default startup wrapper preserves its ambient-home behavior. The existing
factory HTTP test now constructs two clients from different homes concurrently
and verifies their distinct model/auth headers without changing HERMES_HOME.
This is a factory input seam, not live multiplexing. The explicit-home factory
now returns native initialization errors; only the default startup wrapper
retains subprocess fallback. Prepared scopes override file reads, multiplexed
native construction requires a scope, and key/endpoint/limit reads use get_secret.
Real HTTP tests cover empty-scope rejection despite file/process keys and an
authoritative hydrated scoped key. Routed config/explicit overrides and scoped
environment propagation into CLI, bridge and native tools remain to be wired.

The suggested integration below is incomplete. Python _profile_runtime_scope
also installs the profile's complete terminal policy (gateway/run.py:2655-2670).
Native prompt_cache::apply supplies request cache keys, not persona loading.
Client reuse alone does not prove a byte-stable conversation prompt: cache
invalidation and profile file changes must respect active conversation state.
Tokio task-local scope does not automatically propagate into spawned children;
scope every actual factory/agent/subprocess boundary explicitly. Do not enable
profile attribution with only the history database switched.

## 1. Python Architecture vs Existing Rust Assets
In Python, an inbound turn enters `_profile_runtime_scope(profile_home)` (`gateway/run.py:2614-2668`), which isolates credentials (`set_secret_scope`), redirects home paths (`set_hermes_home_override`), and prevents `os.environ` pollution. `GatewayRunner._run_agent` (`gateway/run.py:31154-31188`) matches profile routes (`gateway/run.py:31190-31247`), loads scoped config (`gateway/run.py:4190`), resolves runtime model/credentials (`gateway/run.py:8876-9046`), and caches `AIAgent` instances keyed by config signature (`gateway/run.py:29405-29477`, `6127-6387`) to preserve prompt caches.

| Dimension | Python Reference Construction | Existing Rust Factory / Asset |
|---|---|---|
| **Model** | `_resolve_session_agent_runtime` (`gateway/run.py:8876`), reading profile `config.yaml` `model.default` | `config_loader::load_gateway_config_from` (`src/config_loader.rs:194`), `config_file::load_config_from` (`src/config_file.rs:302`) |
| **Credentials** | `_load_profile_secret_scope` (`gateway/run.py:2648`), `build_profile_secret_scope` (`agent/secret_scope.py:289`) reading `<home>/.env` | `secret_scope::build_profile_secret_scope` (`src/secret_scope.rs:361`), `config_file::resolve_profile_api_key` (`src/config_file.rs:250`) |
| **Prompt** | `build_system_prompt_parts` (`agent/system_prompt.py:435`) loading `<profile_home>/SOUL.md` via `_agent_home` (`line 370`) | `prompt_cache::apply` (`src/prompt_cache.rs:41`), `native_agent::build_messages_with_content` (`src/native_agent.rs:54`) |
| **Tools** | `_resolve_enabled_toolsets_for_source` (`gateway/run.py:31369`), `agent/agent_init.py:1625` | `native_agent::NativeAgentClient::with_tools` (`src/native_agent.rs:209`), `native_tools::CurrentTimeTool` (`src/native_tools.rs:719`) |
| **History** | `SessionStore._named_profile_for_key` (`gateway/session.py:1465`) routing `agent:<profile>:*` to `<home>/state.db` | `session_db_recovery::SessionDatabases::for_key` (`src/session_db_recovery.rs:53`), `session_store::database_for_key` (`src/session_store.rs:32`) |

## 2. Reusable Code vs Existing Implementation Gaps
### Reusable Rust Assets
- **Route Matching & Key Namespacing**: `profile_routing::parse_profile_routes` / `match_profile_route` (`src/profile_routing.rs:181-238`), `profile_name::normalize_profile_name` (`src/profile_name.rs:15-30`), and `session::build_session_key` (`src/session.rs:318-350`), which stamps `agent:<profile>:...`.
- **Zero-Env Credential Scope**: `secret_scope::with_secret_scope` (`src/secret_scope.rs:96`) installs a tokio task-local secret map without mutating `std::env`. `config_schema::getenv_str` (`src/config_schema.rs:460-476`) queries this scope.
- **Fail-Closed DB Isolation**: `SessionDatabases` (`src/session_db_recovery.rs:34-115`) checks tombstone markers (`.deleted`) and resolves profile `state.db` under `<root>/profiles/<name>` using `RecoverableHandleCache`. Unresolvable profiles fail closed (`None`).
- **Prompt Cache Routing**: `prompt_cache::apply` (`src/prompt_cache.rs:41-88`) injects content-addressed `prompt_cache_key` into `api_kwargs`.
- **Native Client Assembly**: `NativeAgentClient` (`src/native_agent.rs:193-285`) supports headers, reasoning config, extra body, output cap, and tool loops.

### Implementation Gaps
- **Static Dispatcher Agent**: `Dispatcher` (`src/dispatch.rs:31, 55-100`) holds a single global `agent: Arc<dyn AgentClient>` constructed once at boot in `main.rs:444`; lacks an `AgentClientFactory` or per-profile dynamic resolution.
- **Unstamped Dispatch Source**: `Dispatcher::handle_turn` (`src/dispatch.rs:301-326`) constructs `SessionSource` without calling `match_profile_route` or setting `source.profile`.
- **Missing Secret Scope Wrapper**: `Dispatcher::run_admitted_turn` (`src/dispatch.rs:383-402`) executes turns on `tokio::spawn` without calling `with_secret_scope`, leaving credentials unscoped.
- **Native System Prompt & Persona Loading**: `NativeAgentClient` only projects conversation history (`src/native_agent.rs:54-65`) and lacks system prompt injection or `SOUL.md` persona loading.
- **Subprocess Bridge Arguments**: `SubprocessAgentClient` (`src/agent.rs:194-206`) calls `hermes_cli.stream_turn` without passing `--profile` or `HERMES_HOME`.

## 3. Smallest Complete Integration
The minimal non-breaking integration wires profile matching into `dispatch.rs` and introduces per-profile agent client resolution:

1. **Inbound Profile Matching** (`src/dispatch.rs:301-326`):
   When `config.multiplex_profiles` is true, evaluate `profile_routing::match_profile_route(&routes, ...)` against `msg.platform`, `channel_id`, `thread_id`. If matched, set `source.profile = Some(matched.profile)`. `session::build_session_key(&source)` automatically creates `agent:<profile>:...`.
2. **Profile History Resolution** (`src/dispatch.rs:328`):
   `store.database_for_key(&entry.session_key)` resolves the profile's isolated SQLite database via `SessionDatabases.for_key()`. If unresolvable or tombstoned, fail closed (returns `None`, preventing fallback to root DB).
3. **Secret Scope Hydration** (`src/dispatch.rs:372`):
   Resolve `profile_home = hermes_root().join("profiles").join(&profile)`. Load secrets via `secret_scope::build_profile_secret_scope(&profile_home)`. Wrap `run_admitted_turn` inside `secret_scope::with_secret_scope(Some(secrets), async move { ... })`.
4. **Per-Profile Agent Client Resolution & Cache** (`src/dispatch.rs:398`):
   Replace the static `self.agent` reference with a factory `resolve_agent_client(&profile, &profile_home, &user_config)`:
   - **Model**: Read profile `config.yaml` via `config_file::load_config_from(&profile_home.join("config.yaml"))` to extract `model.default`.
   - **Credentials**: Resolve provider API key via `config_file::resolve_profile_api_key(..., &secrets, |k| secret_scope::get_secret(k, None).ok().flatten())` without touching process env.
   - **Prompt**: Load persona from `profile_home.join("SOUL.md")` and inject as initial `system` message in `build_messages_with_content`.
   - **Tools**: Attach profile-permitted tools to `NativeAgentClient::with_tools()`.
   - **Prompt Caching Preservation**: Cache `NativeAgentClient` in an LRU map keyed by `(profile, model, api_key_hash, base_url, soul_hash)`. Reusing the client keeps the system prompt prefix and tool schemas byte-identical across turns.
