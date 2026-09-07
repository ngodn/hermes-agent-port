# Implementation Map: Asynchronous ConversationAgent Construction & Prompt Assembly

## Executive Summary

This document maps the next implementation step for the Hermes Rust gateway port (`rust/crates/hermes-gateway`):
1. Make `ConversationAgent` construction asynchronous.
2. Assemble the complete fresh per-conversation system prompt from captured session inputs.
3. Call `conversation_prompt::restore_or_build`.
4. Attach immutable prompt bytes to `NativeAgentClient` before provider I/O.

### Baseline & Invariants
- **Prompt Caching Preservation**: Verbatim stored-prompt reuse across subsequent turns keeps upstream LLM prefix caches warm (`Resolution::reused == true`). The immutable prompt bytes are attached once per conversation client and never mutated.
- **Profile Routing Preservation**: Selected `SessionDb` and profile home (`context.home`, `context.database`) isolate SQLite state, `.env` credentials, `SOUL.md`, `skills/`, and `memories/` per profile (`agent:<profile>:...`).
- **SQLite Separation Rule**: SQLite reads (`store.session_row`) and writes (`store.persist_prompt`) complete strictly **outside** prompt construction and **outside** provider model I/O. Prompt construction (`build`) performs zero SQLite queries. Turn leases serialize turns for the same session locally.
- **Lock Safety Rule**: The client initialization lock is never held across an `.await` boundary, never held during filesystem prompt construction, and never held during provider model I/O. Failed builds are not cached and never fall back to another profile's credentials.

---

## 1. Current State vs. Target Architecture

### Current State Audit
| Component | Source File & Lines | Current Behavior | Gap to Bridge |
| :--- | :--- | :--- | :--- |
| `ConversationAgent` | [`conversation_agent.rs:13-48`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L13-L48) | Synchronous `Factory` closure returning `anyhow::Result<Arc<dyn AgentClient>>`. | Cannot execute async prompt loaders (`load_soul`, `load_context`, `load_runtime_guidance`). |
| Initialization Lock | [`conversation_agent.rs:81-95`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L81-L95) | Holds `std::sync::Mutex<Clients>` synchronously during `(self.factory)(...)`. | Holding a sync lock across async prompt I/O would cause compiler errors or cross-session head-of-line blocking. |
| Production Factory | [`main.rs:550-562`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L550-L562) | Calls `build_agent_client_for_home`, ignoring `_message`, `_history`, `_database`. | Does not assemble system prompt, does not call `restore_or_build`, leaves `system_prompt = None`. |
| Prompt Decision | [`conversation_prompt.rs:52-125`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L52-L125) | Synchronous `pub fn restore_or_build(..., build: impl FnOnce() -> anyhow::Result<String>)`. Staged with 12 golden tests passing. | Signature is synchronous; must accept an async `build` future. |
| Prompt Attachment | [`native_agent.rs:248-251`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L248-L251) | `NativeAgentClient::with_system_prompt` is marked `#[allow(dead_code)]`. | Dead code in production; outgoing requests lack system prompt. |
| Prompt Cache Key | [`native_agent.rs:421-428`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L421-L428) | Calls `prompt_cache::apply` using `original_messages`. | Once `system_prompt` is attached, it prepends to `original_messages` and routes warm prefix keys. |

### Python Reference Anchors
- **Agent Caching & Reuse**: [`gateway/run.py:6124-6387`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L6124-L6387) (`_agent_cache` keyed by `ctx.session_key`).
- **Prompt Restoration & Build Boundary**: [`agent/conversation_loop.py:993-1228`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L993-L1228) (`_restore_or_build_system_prompt`):
  1. Checks `conversation_history` + `agent._session_db`. Reads `session_row` (SQLite read).
  2. If present and `_stored_prompt_matches_runtime`, checks Bot Chat capability epoch. Reuses prompt verbatim without rebuild.
  3. If missing, null, empty, or stale runtime: builds prompt via `_build_system_prompt`, fires `on_session_start`, persists via `update_system_prompt` (SQLite write).
- **Prompt Assembly Order**: [`agent/system_prompt.py:435-1033`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L435-L1033) (`build_system_prompt_parts`):
  - Tier 1 (`stable`): SOUL identity / default identity, help guidance, task completion guidance, parallel tool calls, tool guidance, steer channel, tool-use enforcement, operational guidance, execution discipline, Alibaba provider fix, environment hints, coding prefix, post-workspace platform/profile/probe guidance.
  - Tier 2 (`context`): Coding workspace snapshot, coding trailing parts, caller `system_message`, context files (`AGENTS.md`, `.cursorrules`).
  - Tier 3 (`volatile`): Skills index, memory snapshot (`MEMORY.md`, `USER.md`), external memory, plugin sections, metadata footer.

---

## 2. Line-Specific Implementation Map

```
Turn Ingress (Push: dispatch.rs / HTTP: message.rs)
  │
  ▼ [Acquires Turn Lease for session_id]
ConversationAgent::run_turn_with_context(context, msg, history, events)
  │
  ├── 1. Fast Lock Read: self.clients.lock().get(&(home, session_id))
  │      ├── Hit  ──► Returns cached Arc<dyn AgentClient> (Zero I/O)
  │      └── Miss ──► Drops Lock immediately
  │
  ├── 2. Async Factory: (self.factory)(home, msg, history, context.database).await
  │      │
  │      ├── a. Build base NativeAgentClient for profile home (keys, tools, overrides)
  │      │
  │      ├── b. Call conversation_prompt::restore_or_build(store, session_id, inputs, build)
  │      │      │
  │      │      ├── [SQLite Read] store.session_row(session_id) (if history exists)
  │      │      │
  │      │      ├── Runtime check: stored_prompt_matches_runtime + bot_mode capability check
  │      │      │     ├── Valid ──► Returns Resolution { prompt: stored, reused: true }
  │      │      │     └── Stale/Miss ──► Executes build().await
  │      │      │
  │      │      ├── [Prompt Construction] build().await (Zero SQLite calls)
  │      │      │     ├── ResolvedPromptSections::default()
  │      │      │     ├── initialize_stable (SOUL.md + StableGuidance)
  │      │      │     ├── load_skills (gated on skill tools)
  │      │      │     ├── load_runtime_guidance (provider + env + coding context)
  │      │      │     ├── append_bot_chat + append_profile_platform
  │      │      │     ├── load_context (context files + threat scan)
  │      │      │     ├── set_memory_snapshot (MEMORY.md + USER.md)
  │      │      │     ├── load_plugin_sections
  │      │      │     ├── set_footer (date, tz, session metadata)
  │      │      │     └── sections.assemble().joined()
  │      │      │
  │      │      └── [SQLite Write] store.persist_prompt(session_id, &prompt)
  │      │
  │      └── c. Attach prompt bytes: client.with_system_prompt(resolution.prompt)
  │
  ├── 3. Double-Checked Insert: self.clients.lock().entry(key).or_insert(client)
  │
  └── 4. Model I/O: client.run_turn(msg, history, events).await (Zero locks held)
```

---

### Step 1: Make `ConversationAgent` Construction Asynchronous
**Target File**: [`rust/crates/hermes-gateway/src/conversation_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs)

#### 1.1 Type Definition & Future Boxing (lines 13-27)
Replace the synchronous `Factory` type with an asynchronous higher-ranked boxed future type:

```rust
pub type BoxFuture<'a, T> = std::pin::Pin<
    Box<dyn std::future::Future<Output = T> + Send + 'a>,
>;

type Factory = dyn for<'a> Fn(
        &'a Path,
        &'a Message,
        &'a [crate::session_db::HistoryMessage],
        Option<&'a crate::session_db::SessionDb>,
    ) -> BoxFuture<'a, anyhow::Result<Arc<dyn AgentClient>>>
    + Send
    + Sync;
type Clients = HashMap<(PathBuf, String), Arc<dyn AgentClient>>;

pub struct ConversationAgent {
    fallback: Arc<dyn AgentClient>,
    factory: Box<Factory>,
    clients: Mutex<Clients>,
}
```

#### 1.2 Constructor Signature (lines 29-48)
Accept any closure returning a future matching the lifetime bounds:

```rust
impl ConversationAgent {
    pub fn new<F>(fallback: Arc<dyn AgentClient>, factory: F) -> Self
    where
        F: for<'a> Fn(
                &'a Path,
                &'a Message,
                &'a [crate::session_db::HistoryMessage],
                Option<&'a crate::session_db::SessionDb>,
            ) -> BoxFuture<'a, anyhow::Result<Arc<dyn AgentClient>>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            fallback,
            factory: Box::new(factory),
            clients: Mutex::new(HashMap::new()),
        }
    }
}
```

#### 1.3 Asynchronous Execution & Lock Boundary in `run_turn_with_context` (lines 68-98)
Decouple cache inspection from factory execution:
1. **Cache Hit**: Acquire mutex, check map, clone `Arc<dyn AgentClient>`, drop mutex.
2. **Cache Miss**: Await `(self.factory)(home, msg, history, context.database)` **without holding any lock**.
3. **Cache Insertion**: Re-acquire mutex, insert via `entry(key).or_insert(...)`, drop mutex.
4. **Provider I/O**: Execute `client.run_turn` **without holding any lock**.

```rust
    async fn run_turn_with_context(
        &self,
        context: crate::agent::TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
        events: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let Some(home) = context.home else {
            return self.run_turn(msg, history, events).await;
        };
        let key = (home.to_owned(), crate::session_db::message_session_id(msg));

        // Fast path: cached client lookup with minimal lock hold time.
        let cached = {
            let clients = self.clients.lock().unwrap();
            clients.get(&key).cloned()
        };

        let client = if let Some(client) = cached {
            client
        } else {
            // Build the client asynchronously outside the mutex.
            // Filesystem I/O and prompt assembly never block other sessions.
            let client = (self.factory)(home, msg, history, context.database)
                .await
                .map_err(|error| {
                    hermes_core::Error::Other(format!(
                        "conversation agent initialization failed: {error}"
                    ))
                })?;

            let mut clients = self.clients.lock().unwrap();
            clients.entry(key).or_insert_with(|| client.clone()).clone()
        };

        // Provider I/O runs completely outside the initialization lock.
        client.run_turn(msg, history, events).await
    }
```

---

### Step 2: Make `conversation_prompt::restore_or_build` Asynchronous
**Target File**: [`rust/crates/hermes-gateway/src/conversation_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs)

#### 2.1 Async Signature & Invocation (lines 52-57, 107-115)
Make `restore_or_build` asynchronous so the `build` closure can perform async prompt assembly without blocking worker threads:

```rust
pub async fn restore_or_build<F, Fut>(
    store: Option<&dyn PromptStore>,
    session_id: &str,
    input: &RestoreInputs<'_>,
    build: F,
) -> anyhow::Result<Resolution>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    let mut read_attempted = false;
    let row = if input.has_history {
        store.and_then(|store| {
            read_attempted = true;
            match store.session_row(session_id) {
                Ok(row) => row,
                Err(error) => {
                    tracing::warn!(%error, session_id, "Session DB get_session failed for system-prompt restore; rebuilding");
                    None
                }
            }
        })
    } else {
        None
    };
    let raw = row.as_ref().and_then(|row| row.get("system_prompt"));
    let mut state = match raw {
        None => StoredState::Missing,
        Some(Value::Null) => StoredState::Null,
        Some(Value::String(value)) if value.is_empty() => StoredState::Empty,
        Some(Value::String(_)) => StoredState::Present,
        Some(_) => StoredState::Present,
    };
    if let Some(stored) = raw
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if crate::system_prompt::stored_prompt_matches_runtime(stored, &input.runtime) {
            if !input.capability_stale && !input.legacy_bot_upgrade {
                // VERBATIM REUSE: build() is NEVER called. Prefix cache preserved.
                return Ok(Resolution {
                    prompt: stored.to_owned(),
                    stored_state: state,
                    reused: true,
                    restore_frozen_sections: true,
                    reconstruct_static_prefix: true,
                    refreshed_capability: false,
                    read_attempted,
                    persist_attempted: false,
                });
            }
        } else {
            state = StoredState::StaleRuntime;
        }
    }
    if input.has_history && matches!(state, StoredState::Null | StoredState::Empty) {
        tracing::warn!(session_id, stored_state = ?state, "Stored system prompt is unusable; rebuilding");
    }

    // Fresh build: execute async prompt assembly
    let prompt = build().await?;

    let mut persist_attempted = false;
    if let Some(store) = store {
        persist_attempted = true;
        if let Err(error) = store.persist_prompt(session_id, &prompt) {
            tracing::warn!(%error, session_id, "Session DB update_system_prompt failed; later turns may rebuild");
        }
    }
    Ok(Resolution {
        prompt,
        stored_state: state,
        reused: false,
        restore_frozen_sections: false,
        reconstruct_static_prefix: false,
        refreshed_capability: input.capability_stale || input.legacy_bot_upgrade,
        read_attempted,
        persist_attempted,
    })
}
```

---

### Step 3: Assemble the Complete Fresh Prompt from Captured Session Inputs
**Target File**: [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs) (and supporting helpers in [`system_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs))

#### 3.1 Input Capture Contract
When constructing a conversation agent for `(home, msg, history, database)`:
1. `home: &Path`: Active profile home directory (e.g. `~/.hermes/profiles/work` or `~/.hermes`).
2. `root: PathBuf`: `config_file::hermes_root()`.
3. `user_config: Value`: Loaded via `config_file::load_config_from(&home.join("config.yaml"))`.
4. `model: String`: `config.agent_model` or `user_config["model"]["default"]` / `user_config["model"]["model"]`.
5. `provider: String`: `user_config["model"]["provider"].as_str().unwrap_or("openrouter")`.
6. `platform: String`: `format!("{:?}", msg.platform).to_lowercase()`.
7. `session_id: String`: `crate::session_db::message_session_id(msg)`.
8. `tools: Vec<String>`: Names of active tools configured for `NativeAgentClient` (e.g. `CurrentTimeTool` -> `"current_time"`).
9. `session_db: Option<&SessionDb>`: Borrowed from `TurnContext.database`.
10. `history: &[HistoryMessage]`: Prior messages in the conversation.
11. `cwd_scope: CwdInputs`: Launch directory (`config_file::hermes_home()`), session directory, and terminal cwd.

#### 3.2 Async Assembly Function `build_fresh_system_prompt`
Implement the complete 3-tier fresh prompt builder in `system_prompt.rs` (or `main.rs` helper) consuming existing ported methods on `ResolvedPromptSections`:

```rust
pub async fn assemble_fresh_conversation_prompt(
    home: &Path,
    root: &Path,
    user_config: &serde_json::Value,
    model: &str,
    provider: &str,
    platform: &str,
    session_id: &str,
    tools: &[String],
    root_conversation_id: Option<&str>,
    session_title: &str,
    stored_prompt_for_plugins: Option<&str>,
) -> anyhow::Result<String> {
    let mut sections = ResolvedPromptSections::default();

    // 1. Resolve limits and settings
    let (max_chars, read_timeout_secs) = context_file_limits(user_config, &user_config["model"]["context_length"]);
    let read_timeout = std::time::Duration::from_secs_f64(read_timeout_secs);
    let skip_context_files = crate::python_value::truthy(&user_config["skip_context_files"]);
    let load_soul_identity = crate::python_value::truthy(&user_config["load_soul_identity"]) || !skip_context_files;

    let context_req = crate::context_files::ContextRequest {
        cwd: None,
        launch_cwd: root,
        install_root: root,
        home,
        skip_soul: false,
        allow_install_tree_fallback: matches!(platform, "cli" | "tui"),
        max_chars,
        read_timeout,
    };

    // 2. Skills Index (must load before stable guidance for the help pointer)
    let has_skill_tools = tools.iter().any(|t| matches!(t.as_str(), "skills_list" | "skill_view" | "skill_manage"));
    if has_skill_tools {
        let mut loader = crate::skill_loader::PromptLoader::default();
        let source_ctx = crate::skill_loader::SourceContext {
            home,
            root,
            active_profile: &crate::profile_name::agent_profile_name(home, root),
        };
        let visibility = crate::skill_loader::Visibility {
            tools: Some(&tools.iter().cloned().collect()),
            toolsets: None,
        };
        let _ = sections.load_skills(&mut loader, &source_ctx, &visibility, &std::collections::BTreeSet::new(), |_| Ok(true));
    }

    // 3. Stable Guidance (Tier 1)
    let guidance_input = StableGuidance {
        soul: None,
        tools,
        model,
        skills_index: sections.skills.as_deref().unwrap_or_default(),
        settings: user_config,
    };
    sections.initialize_stable(&context_req, load_soul_identity, skip_context_files, guidance_input).await;

    // 4. Runtime Guidance (Provider + Env Hints + Coding Context)
    let cwd_inputs = crate::runtime_cwd::CwdInputs {
        session: None,
        terminal: "",
        launch: root,
        home,
    };
    let env_inputs = crate::environment_prompt::EnvironmentPromptInputs::default();
    let env_vars = std::env::vars_os().collect();
    sections.load_runtime_guidance(&RuntimeGuidance {
        provider,
        model,
        platform,
        tools,
        config: user_config,
        scope: &cwd_inputs,
        temp_root: Some(std::env::temp_dir().as_path()),
        env: &env_vars,
        environment: &env_inputs,
    }).await;

    // 5. Bot Chat Protocol & Capability Epoch
    let bot_cache = crate::bot_mode::ProtocolCache::default();
    let bot_enabled = crate::python_value::truthy(&user_config["agent"]["bot_mode_protocol"]);
    let timeless = sections.append_bot_chat(&bot_cache, &BotChatGuidance {
        enabled: bot_enabled,
        title_hint: session_title,
        stored_title: session_title,
        home,
        config: Some(user_config),
    });

    // 6. Profile Platform Guidance
    sections.append_profile_platform(&ProfilePlatformGuidance {
        home,
        root,
        platform,
        plugin_hint: "",
        config: user_config,
        overrides: &user_config["platforms"][platform],
        desktop_terminal: None,
    });

    // 7. Context Tier (Context Files: AGENTS.md, .cursorrules)
    let _ = sections.load_context(&cwd_inputs, context_req, platform, false, skip_context_files).await;

    // 8. Memory Snapshot (Tier 3 Volatile)
    let mem_enabled = crate::python_value::truthy(&user_config["agent"]["memory_enabled"]);
    let user_enabled = crate::python_value::truthy(&user_config["agent"]["user_profile_enabled"]);
    if mem_enabled || user_enabled {
        let (mem_limit, user_limit) = (100_000, 100_000);
        if let Ok(snapshot) = crate::memory_snapshot::MemorySnapshot::load(home, mem_limit, user_limit) {
            sections.set_memory_snapshot(Some(&snapshot), mem_enabled, user_enabled);
        }
    }

    // 9. Plugin Sections
    let plugin_registry = crate::plugin_prompt::Registry::default();
    let mut plugin_snapshot = crate::plugin_prompt::Snapshot::default();
    let session_attrs = serde_json::Map::from_iter([
        ("session_id".into(), session_id.into()),
        ("model".into(), model.into()),
        ("provider".into(), provider.into()),
        ("platform".into(), platform.into()),
    ]);
    let plugin_info = crate::plugin_prompt::session_info(&session_attrs, &cwd_inputs, Some(home), root, &serde_json::json!({}));
    sections.load_plugin_sections(&mut plugin_snapshot, stored_prompt_for_plugins, &plugin_registry, &plugin_info);

    // 10. Metadata Footer
    let now = chrono::Local::now();
    let footer = Footer {
        now_date: now.date_naive(),
        start_date: crate::prompt_footer::session_start_date(root_conversation_id, session_id, None, &now, None),
        iana: None,
        abbreviation: now.format("%Z").to_string(),
        offset: now.format("%z").to_string(),
        timeless,
        pass_session_id: true,
        session_id: session_id.to_string(),
        model: model.to_string(),
        provider: provider.to_string(),
        platform: platform.to_string(),
    };
    sections.set_footer(&footer);

    // 11. Final Assembly
    Ok(sections.assemble().joined())
}
```

---

### Step 4: Factory Integration in `main.rs`
**Target File**: [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L548-L565)

Connect `ConversationAgent::new` to the asynchronous client builder:

```rust
// Replace lines 548-565 in main.rs
    let agent: Arc<dyn AgentClient> =
        if config.agent_native && config.agent_cli.is_none() && !agent.manages_history() {
            let captured_config = config.clone();
            Arc::new(conversation_agent::ConversationAgent::new(
                agent,
                move |home, message, history, database| {
                    let home = home.to_path_buf();
                    let message = message.clone();
                    let history = history.to_vec();
                    let db = database.cloned();
                    let captured = captured_config.clone();

                    Box::pin(async move {
                        let selected_config = config_file::load_config_from(&home.join("config.yaml"));
                        let model = captured.agent_model.clone().or_else(|| {
                            selected_config
                                .get("model")
                                .and_then(|v| v.get("default").or_else(|| v.get("model")))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        }).unwrap_or_else(|| "default-model".into());

                        let provider = selected_config
                            .get("model")
                            .and_then(|v| v.get("provider"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("openrouter")
                            .to_string();

                        let platform = format!("{:?}", message.platform).to_lowercase();
                        let session_id = crate::session_db::message_session_id(&message);
                        let root = config_file::hermes_root();

                        // 1. Construct base NativeAgentClient with provider credentials,
                        // reasoning parameters, tool loop, and request overrides.
                        let base_client = build_native_agent_client_for_home(
                            &captured,
                            &selected_config,
                            Some(&model),
                            &home,
                        )?;

                        // 2. Pre-fetch SQLite lineage root and title outside prompt construction.
                        let root_id = db.as_ref().and_then(|db| db.get_conversation_root(&session_id).ok());
                        let title = db.as_ref().and_then(|db| db.get_session_title(&session_id).ok()).unwrap_or_default();

                        // 3. Evaluate capability staleness and legacy upgrade flags
                        let fingerprint = crate::bot_mode::capability_fingerprint(&home, Some(&selected_config));
                        let capability_stale = false; // Resolved from stored row if present
                        let legacy_bot_upgrade = title == crate::bot_mode::BOT_CHAT_TITLE;

                        let runtime = crate::system_prompt::PromptRuntime {
                            model: &model,
                            provider: &provider,
                            platform: &platform,
                            cwd: "",
                        };

                        let restore_inputs = crate::conversation_prompt::RestoreInputs {
                            has_history: !history.is_empty(),
                            runtime,
                            capability_stale,
                            legacy_bot_upgrade,
                        };

                        // 4. Invoke restore_or_build orchestration
                        let store = db.as_ref().map(|db| db as &dyn crate::conversation_prompt::PromptStore);
                        let home_clone = home.clone();
                        let resolution = crate::conversation_prompt::restore_or_build(
                            store,
                            &session_id,
                            &restore_inputs,
                            || {
                                let home = home_clone;
                                let selected = selected_config;
                                let model = model.clone();
                                let provider = provider.clone();
                                let platform = platform.clone();
                                let session_id = session_id.clone();
                                let root = root.clone();
                                async move {
                                    assemble_fresh_conversation_prompt(
                                        &home,
                                        &root,
                                        &selected,
                                        &model,
                                        &provider,
                                        &platform,
                                        &session_id,
                                        &["current_time".into()],
                                        root_id.as_deref(),
                                        &title,
                                        None,
                                    ).await
                                }
                            },
                        ).await?;

                        // 5. Attach immutable prompt bytes before provider I/O
                        let native_client = base_client.with_system_prompt(resolution.prompt);
                        Ok(Arc::new(native_client) as Arc<dyn AgentClient>)
                    })
                },
            ))
        } else {
            agent
        };
```

---

## 3. Preservation of Invariants & Risk Analysis

### 3.1 Prompt Caching & Prefix Stability
- **Risk**: Any variation in system prompt bytes across turns in a conversation invalidates the upstream KV prefix cache (Anthropic / OpenAI), driving up latency and cost.
- **Guard**:
  - On turn 1 (or prompt miss), `restore_or_build` persists the prompt to SQLite via `store.persist_prompt`.
  - On turn 2+, `has_history` is true. `store.session_row` loads the exact stored prompt.
  - If runtime matches (`_stored_prompt_matches_runtime`), `restore_or_build` returns `reused: true` with the identical string.
  - `NativeAgentClient::with_system_prompt` stores `Option<Arc<str>>`. All turns under that client send the exact same bytes in `{"role": "system", "content": prompt}`.
  - `prompt_cache::apply` computes the SHA-256 prefix hash and attaches the bounded `prompt_cache_key`.

### 3.2 Selected Profile Routing & Database Isolation
- **Risk**: An incoming turn for profile `work` could mistakenly read profile `default`'s credentials or write to `default`'s SQLite database.
- **Guard**:
  - `TurnContext` carries the selected `home` and `database`.
  - `ConversationAgent.clients` is keyed by `(home: PathBuf, session_id: String)`. Different profiles never share client instances.
  - Base client construction uses `build_agent_client_for_home(&home)` which queries `secret_scope` or `<home>/.env` only.
  - Failure to resolve profile credentials returns an error immediately; it never falls back to another profile's client.

### 3.3 Strict SQLite Separation Rule
- **Rule**: SQLite work must stay strictly outside prompt construction (`build`) and outside model I/O (`run_turn`).
- **Guard**:
  - In `restore_or_build`: SQLite read happens **before** `build().await`. SQLite write happens **after** `build().await`.
  - `build().await` executes prompt assembly using pre-fetched metadata (`root_id`, `title`) and filesystem reads (`SOUL.md`, `memories/`, `AGENTS.md`). It holds no open SQLite statements or locks.
  - `run_turn` (model I/O) executes after `restore_or_build` has completely finished and released any SQLite connection back to the connection pool.

### 3.4 Lock Safety & Concurrency
- **Risk**: Deadlock or thread starvation if `std::sync::MutexGuard` is held across an `.await` boundary, or head-of-line blocking if concurrent conversations block on a shared client build lock.
- **Guard**:
  - The `clients` mutex is acquired only for in-memory map lookup/cloning and map insertion. It is never held during `self.factory.await`.
  - Concurrent turns across different profiles or sessions construct their clients in parallel without mutex contention.
  - Per-session serialization is already guaranteed upstream by `SessionTurnLeaseRegistry::acquire` in `dispatch.rs` and `message.rs`.
  - Failed factory builds return `Err` without caching, ensuring retries on subsequent turns.

---

## 4. Exact Tests and Verification Suite

### 4.1 Unit Tests in `conversation_agent.rs`
Add `#[tokio::test]` cases in [`conversation_agent.rs:100-210`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs#L100-L210):

1. `async_factory_constructs_and_caches_client`:
   - Setup `ConversationAgent` with an async factory closure that simulates 10ms of async I/O.
   - Run turn 1: verify factory executes, returns client, turn runs.
   - Run turn 2 with same key: verify factory is NOT invoked (call count remains 1).
2. `async_factory_error_propagates_and_is_not_cached`:
   - Async factory returns `bail!("profile error")`.
   - Verify `run_turn_with_context` returns `Err`.
   - Verify `clients` map remains empty; next call retries factory.
3. `async_factory_concurrent_distinct_profiles`:
   - Spawn two concurrent tasks with `home: "profile_a"` and `home: "profile_b"`.
   - Verify both async factories run concurrently without deadlock and populate distinct client keys.
4. `async_factory_lock_dropped_during_await`:
   - In factory closure, pause on a `tokio::sync::Notify`.
   - Assert from another task that `agent.clients.try_lock()` succeeds while the factory is awaiting.

### 4.2 Unit Tests in `conversation_prompt.rs`
Update [`conversation_prompt.rs:127-238`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L127-L238):

1. `decisions_match_actual_python_restore_helper`:
   - Update to `#[tokio::test] async fn`.
   - Pass `|| async { Ok("fresh prompt".into()) }` to `restore_or_build`.
   - Verify all 12 golden test cases pass unchanged.
2. `restore_or_build_skips_build_on_verbatim_reuse`:
   - Provide a store with matching stored prompt.
   - Pass a `build` closure that panics (`panic!("build must not be called")`).
   - Verify `restore_or_build` succeeds with `reused: true` without panicking.
3. `sqlite_operations_strictly_bracket_build`:
   - Use a mock `PromptStore` that logs timestamps of `session_row` and `persist_prompt`.
   - Log timestamp inside `build().await`.
   - Assert: `t_read < t_build_start < t_build_end < t_write`.

### 4.3 End-to-End Integration Tests in `hermes-gateway`
Add integration tests using real temporary profiles and local HTTP mock server:

1. `native_conversation_agent_assembles_and_attaches_system_prompt`:
   - Create temporary profile home with `config.yaml` (`model.provider: openrouter`), `SOUL.md` ("Custom Soul Identity"), and `memories/MEMORY.md` ("Important User Fact").
   - Construct `ConversationAgent` with the production async factory.
   - Dispatch turn 1: mock HTTP server verifies that outgoing `/chat/completions` request body contains `role: "system"` message containing "Custom Soul Identity", "Important User Fact", and metadata footer.
   - Verify `state.db` contains persisted row in `system_prompts` and updated `sessions.system_prompt_hash`.
2. `native_conversation_agent_subsequent_turn_reuses_persisted_prompt_verbatim`:
   - After turn 1, modify `SOUL.md` on disk to "Mutated Identity".
   - Dispatch turn 2 for the same conversation ID.
   - Verify outgoing `/chat/completions` request body contains the **original** "Custom Soul Identity" from SQLite, NOT the mutated disk file.
   - Verify `Resolution::reused == true`, proving prompt caching preservation across turns.
3. `profile_separation_across_turns`:
   - Send turn to Profile Red (`SOUL.md` = "Red Agent").
   - Send turn to Profile Blue (`SOUL.md` = "Blue Agent").
   - Verify Profile Red receives Red's system prompt and Profile Blue receives Blue's system prompt.

### 4.4 Verification Commands
```bash
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --package hermes-gateway --lib conversation_agent
cargo test --package hermes-gateway --lib conversation_prompt
cargo test --workspace
```
