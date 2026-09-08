//! Hermes gateway (Rust rewrite) — entry point.
//!
//! This is the strangler-fig seam: it stands up the long-lived network process
//! that the Python `gateway/run.py` owns today. Platform adapters and the
//! agent RPC boundary are ported in behind this skeleton one at a time.

mod agent;
mod agent_cache_pressure;
mod api_server_run_idempotency;
mod atomic_file;
mod audio_process;
mod auth_store;
mod authz;
mod automatic_compression;
mod bot_mode;
mod browser_control_artifacts;
mod browser_control_broker;
mod cache_paths;
mod cgroup_cleanup;
mod channel_directory;
mod chat_message_projection;
mod cli_agent;
mod code_skew;
mod coding_context;
mod coding_project_facts;
mod coding_prompt;
mod command_catalog;
mod compression_prompt;
mod compression_redact;
mod config;
mod config_env_overrides;
mod config_file;
mod config_gateway;
mod config_loader;
mod config_schema;
mod config_types;
mod context_files;
mod control_socket;
mod conversation_agent;
mod conversation_prompt;
mod credential_persistence;
mod credential_pool;
mod credential_sources;
mod custom_provider_config;
mod custom_request_config;
mod cwd_placeholder;
mod dead_targets;
mod delegation_policy;
mod delivery;
mod delivery_ledger;
mod discord;
mod disk_status;
mod dispatch;
mod display_config;
mod drain_control;
mod durable_turn_lease;
mod environment_probe;
mod environment_prompt;
mod extension_host;
mod file_read_safety;
mod gemini_thinking;
mod git_probe;
mod health;
mod hooks;
mod hosted_room_execution_policy;
mod hosted_room_links;
mod hosted_room_peer;
mod hosted_room_policy_checkpoint;
mod hosted_room_replicas;
mod hosted_rooms;
mod hosted_rooms_log;
mod http_client_limits;
mod image_references;
mod image_routing;
mod inbound_media;
mod inbound_text_context;
mod install_identity;
mod kanban_watchers;
mod lifecycle_ledger;
mod local_probe;
mod managed_capabilities;
mod managed_catalog;
mod media;
mod media_context;
mod media_policy;
mod media_repair;
mod memory_monitor;
mod memory_snapshot;
mod memory_status;
mod message;
mod message_repair;
mod message_timestamps;
mod mime_types;
mod mirror;
mod models_dev;
mod native_agent;
mod native_image_content;
mod native_tools;
mod ogg_opus_duration;
mod pairing;
mod partial_compress;
mod pending_messages;
mod pending_stt;
mod platform;
mod platform_base_types;
mod platform_helpers;
mod plugin_prompt;
mod profile_name;
mod profile_routing;
mod prompt_cache;
mod prompt_footer;
mod provider_registry;
mod provider_usage;
mod python_literal;
mod python_value;
mod qqbot_common;
mod qqbot_crypto;
mod qqbot_keyboards;
mod qqbot_onboard;
mod readiness;
mod reasoning_effort;
mod reasoning_replay;
mod relay_auth;
mod relay_command_manifest;
mod relay_descriptor;
mod relay_transport;
mod response_filters;
mod restart;
mod restart_loop_guard;
mod retry_utils;
mod rich_sent_store;
mod runtime_clock;
mod runtime_cwd;
mod runtime_footer;
mod scale_to_zero;
mod secret_scope;
mod session;
mod session_admission;
mod session_commands;
mod session_db;
mod session_db_recovery;
mod session_entry;
mod session_image_routing;
mod session_registry;
mod session_reset;
mod session_routing;
mod session_stall;
mod session_state;
mod session_store;
mod shutdown_flush;
mod shutdown_forensics;
mod shutdown_watchdog;
mod signal_format;
mod signal_rate_limit;
mod skill_discovery;
mod skill_loader;
mod skill_yaml;
mod skills_guard;
mod skills_index;
mod slack;
mod slack_blocks;
mod slash;
mod slash_access;
mod slash_confirm;
mod status;
mod status_phrases;
mod sticker_cache;
mod stream_consumer;
mod system_prompt;
mod systemd_notify;
mod telegram;
mod think_scrubber;
mod threat_patterns;
mod tool_arguments;
mod tool_backend_selection;
mod tool_credentials;
mod tool_name_repair;
mod tool_pairing;
mod tool_result;
mod tool_result_prune;
mod toolset_resolution;
mod transcription_enrichment;
mod transcription_http;
mod turn_lease;
mod turn_limit;
mod visible_response;
mod vision_enrichment;
mod wake;
mod webhook_filters;
mod whatsapp_common;
mod whatsapp_identity;
mod yuanbao_proto;
mod yuanbao_sticker;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::agent::{AgentClient, SubprocessAgentClient};
use crate::config::Config;
use crate::dispatch::Dispatcher;
use crate::health::{healthz, readyz, status, AppState};
use crate::message::{get_display_config, get_search, post_message};
use crate::native_agent::NativeAgentClient;
use crate::platform::PlatformAdapter;
use tokio_util::sync::CancellationToken;

struct NativeConversationState {
    system_prompt: String,
    tools: Vec<Arc<dyn crate::native_tools::Tool>>,
    plugin_prompt: crate::plugin_prompt::Snapshot,
    extension_host: Option<crate::extension_host::Client>,
    context_length: u64,
}

fn registered_native_tools() -> Vec<Arc<dyn crate::native_tools::Tool>> {
    vec![Arc::new(crate::native_tools::CurrentTimeTool)]
}

fn available_native_tools(config: &Config) -> Vec<Arc<dyn crate::native_tools::Tool>> {
    if config.agent_tools {
        registered_native_tools()
    } else {
        Vec::new()
    }
}

fn extensions_configured(
    config: &serde_json::Value,
    platform: &str,
    install_root: &std::path::Path,
) -> bool {
    config["memory"]["provider"]
        .as_str()
        .is_some_and(|name| !name.trim().is_empty())
        || config["plugins"]["enabled"]
            .as_array()
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.as_str().is_some_and(|name| !name.trim().is_empty()))
            })
        || auto_tool_backend_requested(config, platform, install_root)
}

fn auto_tool_backend_requested(
    config: &serde_json::Value,
    platform: &str,
    install_root: &std::path::Path,
) -> bool {
    let requested: std::collections::HashSet<_> = config["platform_toolsets"][platform]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    if requested.is_empty() {
        return false;
    }
    fn scan(
        path: &std::path::Path,
        depth: usize,
        requested: &std::collections::HashSet<&str>,
    ) -> bool {
        let Ok(entries) = std::fs::read_dir(path) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "plugin.yaml") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(manifest) = serde_yaml_ng::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if manifest["kind"].as_str() == Some("backend")
                    && manifest["provides_tools"]
                        .as_array()
                        .is_some_and(|tools| !tools.is_empty())
                    && manifest["name"]
                        .as_str()
                        .is_some_and(|name| requested.contains(name))
                {
                    return true;
                }
            } else if depth > 0 && path.is_dir() && scan(&path, depth - 1, requested) {
                return true;
            }
        }
        false
    }
    scan(&install_root.join("plugins"), 3, &requested)
}

/// Extension definitions replace an explicitly authorized native override in
/// place, while new names append without perturbing the existing prefix.
fn merge_native_tools(
    mut base: Vec<Arc<dyn crate::native_tools::Tool>>,
    extensions: Vec<Arc<dyn crate::native_tools::Tool>>,
) -> Vec<Arc<dyn crate::native_tools::Tool>> {
    for extension in extensions {
        let name = extension.spec().name;
        if let Some(slot) = base.iter().position(|tool| tool.spec().name == name) {
            base[slot] = extension;
        } else if !name.is_empty() {
            base.push(extension);
        }
    }
    base
}

/// Pick the agent backend. Native (in-Rust LLM chat) requires opt-in
/// (`HERMES_AGENT_NATIVE`), an API key (`HERMES_LLM_API_KEY`), and a resolved
/// model; anything missing falls back to the Python subprocess bridge so the
/// gateway never silently does nothing.
fn build_agent_client(
    config: &Config,
    user_config: &serde_json::Value,
    model: Option<&str>,
) -> Arc<dyn AgentClient> {
    build_agent_client_for_home(
        config,
        user_config,
        model,
        &config_file::hermes_home(),
        None,
    )
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "agent initialization failed; using existing subprocess fallback");
        build_subprocess_agent(config)
    })
}

/// Build against the selected conversation profile without changing ambient
/// HERMES_HOME. Each native client captures one credentials/config snapshot;
/// concurrent profiles must never reread another profile's .env mid-build.
fn build_agent_client_for_home(
    config: &Config,
    user_config: &serde_json::Value,
    model: Option<&str>,
    home: &std::path::Path,
    conversation: Option<NativeConversationState>,
) -> anyhow::Result<Arc<dyn AgentClient>> {
    // Highest precedence: a CLI backend (Claude Code / Antigravity / any print-
    // mode LLM CLI). Turns run via that CLI, no Python and no HTTP key needed.
    if let Some(program) = config.agent_cli.clone() {
        let extra = config
            .agent_cli_args
            .as_deref()
            .map(cli_agent::split_extra_args)
            .unwrap_or_default();
        // Prompt flag: unset -> default "-p"; set-empty -> positional prompt.
        let prompt_flag = match config.agent_cli_prompt_flag.as_deref() {
            None => Some("-p".to_string()),
            Some("") => None,
            Some(f) => Some(f.to_string()),
        };
        tracing::info!(program, "using CLI-backend agent client");
        return Ok(Arc::new(cli_agent::CliAgentClient::new(
            program,
            extra,
            prompt_flag,
        )));
    }

    if config.agent_native {
        let scope = secret_scope::current_secret_scope();
        anyhow::ensure!(
            !secret_scope::is_multiplex_active() || scope.is_some(),
            "native profile construction requires a secret scope"
        );
        // Prepared scopes include hydrated external sources and are authoritative
        // over a second file read. Missing multiplexed keys must stay missing.
        let dotenv = scope
            .as_deref()
            .cloned()
            .unwrap_or_else(|| config_file::load_dotenv(&home.join(".env")));
        let environment = |name: &str| secret_scope::get_secret(name, None).ok().flatten();
        let profiles = provider_registry::ProviderRegistry::default();
        profiles.register_bundled_base_profiles(env!("CARGO_PKG_VERSION"));
        profiles.register_upstage();
        profiles.register_nebius();
        profiles.register_vercel();
        let profile = user_config
            .get("model")
            .and_then(|model| model.get("provider"))
            .and_then(serde_json::Value::as_str)
            .and_then(|name| profiles.get(name))
            .map(|profile| profile.read().unwrap().clone());

        // Explicit endpoints win, then a registered base-profile endpoint.
        // Generic configurations retain the OpenRouter default.
        let base_url = config
            .llm_base_url
            .clone()
            .or_else(|| {
                user_config
                    .get("model")
                    .and_then(|m| m.get("base_url"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .or_else(|| {
                profile.as_ref().and_then(|profile| {
                    profile
                        .env_vars
                        .iter()
                        .find(|name| name.ends_with("_URL"))
                        .and_then(|name| environment(name))
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                })
            })
            .or_else(|| profile.as_ref().map(|profile| profile.base_url.clone()))
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());

        // Explicit native credentials win. Registered profiles use their own
        // declared key names; generic configurations retain the legacy lookup.
        let key = config.llm_api_key.clone().or_else(|| match &profile {
            Some(profile) => config_file::resolve_profile_api_key(profile, &dotenv, environment),
            None => config_file::resolve_provider_api_key_with_env(&base_url, &dotenv, environment),
        });

        match (key, model) {
            (Some(key), Some(model)) => match NativeAgentClient::new(model, key, base_url.clone())
                .and_then(|client| {
                    let limit = turn_limit::gateway(
                        user_config,
                        environment("HERMES_MAX_ITERATIONS").as_deref(),
                    )?;
                    let client = client.with_turn_limit(limit).with_max_concurrent_children(
                        delegation_policy::max_children(
                            user_config,
                            environment("DELEGATION_MAX_CONCURRENT_CHILDREN").as_deref(),
                        ),
                    );
                    let client = match &profile {
                        Some(profile) => client.with_provider_profile(profile)?,
                        None => client,
                    };
                    client.with_extra_headers(&custom_provider_config::extra_headers(
                        user_config,
                        &base_url,
                    ))
                }) {
                Ok(mut c) => {
                    c = c.with_reasoning_config(reasoning_effort::resolve_config(
                        user_config,
                        model,
                    ));
                    c = c.with_reasoning_echo(
                        python_value::truthy(&user_config["model"]["reasoning_echo"])
                            || reasoning_replay::needs_echo(
                                user_config["model"]["provider"].as_str().unwrap_or(""),
                                model,
                                &base_url,
                            ),
                    );
                    let requested_provider =
                        user_config["model"]["provider"].as_str().unwrap_or("");
                    let named = custom_provider_config::named(
                        user_config,
                        requested_provider,
                        profile.as_ref().map(|profile| profile.name.as_str()),
                        |name| {
                            environment(name)
                                .or_else(|| dotenv.get(name).cloned())
                                .unwrap_or_default()
                        },
                    );
                    let named_overrides = named
                        .as_ref()
                        .and_then(|entry| entry["extra_body"].as_object())
                        .filter(|body| !body.is_empty())
                        .cloned();
                    if let Some(extra) = named_overrides.or_else(|| {
                        custom_request_config::select_extra_body(
                            requested_provider,
                            model,
                            &base_url,
                            &custom_provider_config::compatible(user_config),
                        )
                    }) {
                        c = c.with_request_overrides(serde_json::Map::from_iter([(
                            "extra_body".into(),
                            serde_json::Value::Object(extra),
                        )]));
                    }
                    // A saved provider supplies the fallback cap. Global and
                    // environment limits retain the gateway's precedence.
                    c = c.with_output_cap(native_agent::resolve_output_cap(
                        &user_config["model"]["max_tokens"],
                        environment("HERMES_MAX_TOKENS").as_deref(),
                        named
                            .as_ref()
                            .and_then(|entry| entry.get("max_output_tokens")),
                    ));
                    let tools = conversation
                        .as_ref()
                        .map(|state| state.tools.clone())
                        .unwrap_or_else(|| available_native_tools(config));
                    if !tools.is_empty() {
                        c = c.with_tools(tools);
                        tracing::info!(
                            model,
                            base_url,
                            "using native agent client (tools enabled)"
                        );
                    } else {
                        tracing::info!(model, base_url, "using native agent client");
                    }
                    if let Some(state) = conversation {
                        c = c
                            .with_system_prompt(state.system_prompt)
                            .with_plugin_prompt_snapshot(state.plugin_prompt)
                            .with_extension_host(state.extension_host)
                            .with_context_length(state.context_length);
                    }
                    return Ok(Arc::new(c));
                }
                Err(err) => {
                    return Err(anyhow::anyhow!("native agent initialization failed: {err}"));
                }
            },
            (None, _) => anyhow::bail!("no API key resolved for native provider"),
            (_, None) => anyhow::bail!("no model resolved for native provider"),
        }
    }
    Ok(build_subprocess_agent(config))
}

async fn build_conversation_client(
    config: &Config,
    initializer: &conversation_prompt::Initializer,
    home: &std::path::Path,
    message: &hermes_core::Message,
    history: &[session_db::HistoryMessage],
    database: Option<&session_db::SessionDb>,
) -> anyhow::Result<Arc<dyn AgentClient>> {
    let profile_secrets = secret_scope::current_secret_scope().as_deref().cloned();
    anyhow::ensure!(
        !secret_scope::is_multiplex_active() || profile_secrets.is_some(),
        "native profile construction requires a secret scope"
    );
    let selected = config_file::load_config_from(&home.join("config.yaml"));
    let model = config
        .agent_model
        .clone()
        .or_else(|| {
            selected
                .get("model")
                .and_then(|value| value.get("default").or_else(|| value.get("model")))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| anyhow::anyhow!("no model resolved for native provider"))?;
    let provider = selected["model"]["provider"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("openrouter")
        .to_owned();
    let platform = format!("{:?}", message.platform).to_lowercase();
    let session_id = session_db::message_session_id(message);
    let metadata_row = database.and_then(|database| {
        database
            .get_session(&session_id)
            .map_err(|error| {
                tracing::warn!(%error, %session_id, "Session metadata read failed before prompt construction");
                error
            })
            .ok()
            .flatten()
    });
    let session_cwd = metadata_row
        .as_ref()
        .and_then(|row| row.get("cwd"))
        .and_then(serde_json::Value::as_str);
    let gateway_session_key = metadata_row
        .as_ref()
        .and_then(|row| row.get("session_key"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(&session_id)
        .to_owned();
    let runtime_cwd = initializer.runtime_cwd(home, session_cwd)?;
    let mut extension = if extensions_configured(&selected, &platform, &config.agent_cwd) {
        let params = extension_host::InitializeParams {
            home: home.to_string_lossy().into_owned(),
            session_id: session_id.clone(),
            model: model.clone(),
            provider: provider.clone(),
            platform: platform.clone(),
            profile_name: initializer.profile_name(home),
            cwd: runtime_cwd.clone(),
            session_title: metadata_row
                .as_ref()
                .and_then(|row| row.get("display_name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            user_id: (!message.sender_id.is_empty()).then(|| message.sender_id.clone()),
            user_id_alt: None,
            user_name: None,
            chat_id: (!message.channel_id.is_empty()).then(|| message.channel_id.clone()),
            chat_name: None,
            chat_type: message.chat_type.clone(),
            thread_id: message.thread_id.clone(),
            gateway_session_key: Some(gateway_session_key),
            native_tool_names: native_tools::tool_names(&registered_native_tools()),
            profile_secrets: profile_secrets.clone(),
        };
        match extension_host::Client::spawn(&config.agent_python, &config.agent_cwd, home, params)
            .await
        {
            Ok((client, initialized)) => {
                if let Some(provider) = initialized.active_memory_provider.as_deref() {
                    tracing::info!(provider, %session_id, "Native external-memory provider activated");
                    if !initialized.memory_exposed {
                        tracing::info!(provider, %session_id, "External-memory prompt and tools are gated off for this session");
                    }
                }
                Some((client, initialized))
            }
            Err(error) => {
                tracing::warn!(%error, %session_id, "Native extension host unavailable; continuing without extensions");
                None
            }
        }
    } else {
        None
    };
    let registered_extension_tools = extension
        .as_ref()
        .map(|(client, initialized)| initialized.registered_tools(client))
        .unwrap_or_default();
    // HERMES_AGENT_TOOLS is the legacy opt-in for Rust's built-in tools only.
    // Explicitly configured plugin and memory tools follow their own Python
    // toolset gates and must stay executable whenever they are advertised.
    let available_extension_tools = extension
        .as_ref()
        .map(|(client, initialized)| initialized.available_tools(client))
        .unwrap_or_default();
    let registered_tools =
        merge_native_tools(registered_native_tools(), registered_extension_tools);
    let base_fresh_tools = available_native_tools(config);
    let base_fresh_tool_names = native_tools::tool_names(&base_fresh_tools);
    let mut fresh_tools = merge_native_tools(base_fresh_tools, available_extension_tools);
    let fresh_tool_names = native_tools::tool_names(&fresh_tools);
    let extension_snapshot_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bot = initializer.bot_inputs(home, Some(&selected));
    let resolution = conversation_prompt::restore_or_build(
        database.map(|database| database as &dyn conversation_prompt::PromptStore),
        &session_id,
        &conversation_prompt::RestoreInputs {
            has_history: !history.is_empty(),
            runtime: system_prompt::PromptRuntime {
                model: &model,
                provider: &provider,
                platform: &platform,
                cwd: &runtime_cwd,
            },
            tool_names: &fresh_tool_names,
            capability_stale: false,
            legacy_bot_upgrade: false,
            bot: Some(bot),
        },
        |snapshot| async {
            let extension_snapshot = match extension.as_ref() {
                Some((client, _)) => match client.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        extension_snapshot_failed.store(true, std::sync::atomic::Ordering::Release);
                        tracing::warn!(%error, %session_id, "Native extension prompt snapshot failed; omitting extensions");
                        extension_host::PromptSnapshot::default()
                    }
                },
                None => extension_host::PromptSnapshot::default(),
            };
            let prompt_tools = if extension_snapshot_failed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                &base_fresh_tool_names
            } else {
                &fresh_tool_names
            };
            initializer.build_fresh(conversation_prompt::FreshPromptInputs {
                home,
                config: &selected,
                model: &model,
                provider: &provider,
                platform: &platform,
                session_id: &session_id,
                tools: prompt_tools,
                external_memory: extension_snapshot.memory_prompt,
                plugin_sections: extension_snapshot.plugin_sections,
                snapshot,
            }).await
        },
    )
    .await?;
    if !resolution.reused && extension_snapshot_failed.load(std::sync::atomic::Ordering::Acquire) {
        fresh_tools = available_native_tools(config);
        extension = None;
        if let Some(database) = database {
            if let Err(error) =
                database.update_session_tool_names(&session_id, Some(&base_fresh_tool_names))
            {
                tracing::debug!(%error, %session_id, "Session DB extension-failure tool persist skipped");
            }
        }
    }
    let mut plugin_prompt = plugin_prompt::Snapshot::default();
    plugin_prompt.restore(&resolution.prompt);
    let tools = if resolution.restore_frozen_sections {
        match resolution.saved_tool_names.as_deref() {
            Some(saved) => {
                let (tools, changed) =
                    native_tools::restore_tool_prefix(saved, fresh_tools, &registered_tools);
                if changed {
                    if let Some(database) = database {
                        let names = native_tools::tool_names(&tools);
                        if let Err(error) =
                            database.update_session_tool_names(&session_id, Some(&names))
                        {
                            tracing::debug!(%error, %session_id, "Session DB restored tool-prefix persist skipped");
                        }
                    }
                }
                tools
            }
            None => fresh_tools,
        }
    } else {
        fresh_tools
    };
    let context_length = models_dev::ModelsDev::new(home.to_path_buf(), &selected)
        .context_window(&provider, &model, &selected, true)
        .await
        .unwrap_or(256_000);
    build_agent_client_for_home(
        config,
        &selected,
        Some(&model),
        home,
        Some(NativeConversationState {
            system_prompt: resolution.prompt,
            tools,
            plugin_prompt,
            extension_host: extension.map(|(client, _)| client),
            context_length,
        }),
    )
}

fn build_subprocess_agent(config: &Config) -> Arc<dyn AgentClient> {
    let mut agent =
        SubprocessAgentClient::new(config.agent_python.clone(), config.agent_cwd.clone());
    if let Some(model) = &config.agent_model {
        agent = agent.with_model(model.clone());
    }
    tracing::info!("using subprocess agent bridge");
    Arc::new(agent)
}

/// Start one platform's push path: the adapter's inbound loop feeding a
/// Dispatcher that runs turns and delivers replies through the same adapter.
/// Both halves stop when `shutdown` is cancelled, so a SIGTERM/SIGINT tears the
/// push paths down instead of leaving them running into process teardown.
fn start_push_path(
    platform: hermes_core::Platform,
    adapter: Arc<dyn PlatformAdapter>,
    state: &AppState,
    shutdown: CancellationToken,
) {
    let mut dispatcher = Dispatcher::new(
        state.agent.clone(),
        state.user_config.clone(),
        state.session_db.clone(),
    )
    .with_turn_leases(
        state.turn_leases.clone(),
        state.route_leases.clone(),
        state.turn_generation.clone(),
    )
    .with_slash_confirmations(state.slash_confirmations.clone());
    if let Some((store, freshness)) = &state.session_store {
        dispatcher = dispatcher.with_session_store(store.clone(), *freshness);
    }
    // Install the inbound-audio transcription backend when STT is configured, so
    // a downloaded voice note is transcribed before its turn. None (no key
    // resolvable) leaves audio untranscribed rather than failing the turn.
    if let Some(backend) = transcription_http::build_gateway_transcription(&state.user_config) {
        dispatcher = dispatcher.with_transcription(backend);
    }
    dispatcher.register_adapter(platform, adapter.clone());
    let dispatcher = Arc::new(dispatcher);

    let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<hermes_core::Message>(128);

    let adapter_shutdown = shutdown.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = adapter_shutdown.cancelled() => {
                tracing::info!(?platform, "adapter stopping on shutdown");
            }
            r = adapter.run(inbound_tx) => {
                if let Err(err) = r {
                    tracing::error!(?platform, %err, "adapter loop exited");
                }
            }
        }
    });

    let disp_run = dispatcher.run(inbound_rx);
    tokio::spawn(async move {
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = disp_run => {}
        }
    });
    tracing::info!(?platform, "push path started");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hermes_gateway=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env()?;

    // Singleton / status lifecycle is opt-in (`HERMES_GATEWAY_SINGLETON=1`). It
    // takes the profile's runtime flock and owns gateway_state.json, so it is
    // OFF by default: during the strangler migration the Python gateway still
    // owns those, and only the operator flips this at cutover.
    let singleton = matches!(
        std::env::var("HERMES_GATEWAY_SINGLETON")
            .unwrap_or_default()
            .trim(),
        "1" | "true" | "yes" | "on"
    );

    if singleton {
        if let Some(storm) = status::record_start_and_check_storm(5, 120.0, 300.0) {
            tracing::warn!(
                count = storm.count,
                backoff_s = storm.backoff_s,
                "respawn storm detected; backing off before continuing"
            );
            tokio::time::sleep(std::time::Duration::from_secs_f64(storm.backoff_s)).await;
        }
        if !status::acquire_gateway_runtime_lock() {
            tracing::error!("another gateway already holds this profile's runtime lock; exiting");
            return Ok(());
        }
        status::write_pid_file();
        status::write_runtime_status(&status::StatusUpdate {
            gateway_state: Some(serde_json::json!("starting")),
            clear_profile_platforms: true,
            ..Default::default()
        });
        // Now that we own the status file, publish session-store health into it.
        session_db_recovery::set_health_sink(|aggregate| {
            status::write_runtime_status(&status::StatusUpdate {
                session_store: Some(serde_json::json!({ "status": aggregate })),
                ..Default::default()
            });
        });
        // Lifecycle sentinel: report any unclean previous death, claim this life.
        lifecycle_ledger::record_startup(None);
    }
    // Gateway process start time (wall epoch) for heartbeat PID-reuse detection.
    let boot_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    // Load the user config (config.yaml) once at startup; consumers read it
    // from shared state. Absent/broken config degrades to defaults.
    let user_config = Arc::new(config_file::load_config());
    if user_config
        .as_object()
        .map(|m| m.is_empty())
        .unwrap_or(true)
    {
        tracing::info!(path = %config_file::config_path().display(), "no user config found; using defaults");
    } else {
        tracing::info!(path = %config_file::config_path().display(), "loaded user config");
    }

    // Resolve the configured model: the explicit override wins, else config.yaml's
    // model.default / model.model.
    let configured_model = config.agent_model.clone().or_else(|| {
        user_config
            .get("model")
            .and_then(|m| m.get("default").or_else(|| m.get("model")))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });

    // Choose the agent backend. Native (in-Rust LLM) is opt-in and needs a key +
    // a model; otherwise fall back to the Python subprocess bridge (default).
    let agent = build_agent_client(&config, &user_config, configured_model.as_deref());
    let mut conversation_cache = None;
    let agent: Arc<dyn AgentClient> =
        if config.agent_native && config.agent_cli.is_none() && !agent.manages_history() {
            let captured = config.clone();
            let prompt_initializer = Arc::new(conversation_prompt::Initializer::capture(
                config_file::hermes_root(),
                config.agent_cwd.clone(),
            )?);
            let cache = Arc::new(conversation_agent::ConversationAgent::new(
                agent,
                move |home, message, history, database| {
                    let home = home.to_owned();
                    let captured = captured.clone();
                    let message = message.clone();
                    let history = history.to_vec();
                    let prompt_initializer = prompt_initializer.clone();
                    Box::pin(async move {
                        build_conversation_client(
                            &captured,
                            &prompt_initializer,
                            &home,
                            &message,
                            &history,
                            database,
                        )
                        .await
                    })
                },
                agent_cache_pressure::resolve_agent_cache_bounds(&user_config),
            ));
            conversation_cache = Some(cache.clone());
            cache
        } else {
            agent
        };

    // Conversation-history store. Backends that manage their own history (the
    // Python bridge) ignore it; native/CLI backends use it for multi-turn.
    let session_db =
        match session_db::SessionDb::open_shared(config_file::hermes_home().join("state.db")) {
            Ok(db) => Some(db),
            Err(err) => {
                tracing::warn!(%err, "session store unavailable; turns will be stateless");
                None
            }
        };

    let mut state = AppState::new(agent, user_config, configured_model, session_db);
    if !state.agent.manages_history() {
        let home = config_file::hermes_home();
        let root = config_file::hermes_root();
        let freshness = session_reset::configured_freshness_seconds(
            &state.user_config,
            std::env::var("HERMES_AUTO_CONTINUE_FRESHNESS")
                .ok()
                .as_deref(),
        );
        let store = tokio::task::spawn_blocking(move || {
            let profile = profile_name::active_profile_name(&home, &root)
                .unwrap_or_else(|_| "default".into());
            let gateway_config = config_loader::load_gateway_config_from(&home);
            // The native tool runtime has no process registry yet. This is
            // Python's missing-registry case; wire its liveness probe when
            // background process execution becomes available.
            session_store::SessionStore::open(gateway_config, root, home, profile, |_| Ok(false))
        })
        .await??;
        state.session_store = Some((Arc::new(store), freshness));
    }

    // Recover any messages a prior gateway life flushed to disk before it died
    // (data-loss guard, #72680). Only when we own the profile (singleton), so
    // it doesn't race the Python gateway during migration.
    if singleton {
        if let Some(db) = state.session_db.as_deref() {
            let n = shutdown_flush::recover_pending_to_db(db, None);
            if n > 0 {
                tracing::info!(
                    recovered = n,
                    "recovered flushed pending messages at startup"
                );
            }
        }
    }

    // One shutdown token, cancelled on SIGINT/SIGTERM, drives both the push
    // paths and the HTTP server's graceful shutdown.
    let shutdown = CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            wait_for_signal().await;
            tracing::info!("shutdown signal received, draining");
            shutdown.cancel();
        }
    });

    // Local control socket: an owned identify/status surface for tooling.
    // Best-effort; stops with the shutdown token.
    tokio::spawn(control_socket::serve(
        config_file::hermes_home(),
        shutdown.clone(),
    ));

    // Periodic RSS logging (leak detection). Passive logging only, so it runs
    // regardless of the singleton flag; stops with the shutdown token.
    memory_monitor::start_memory_monitoring(std::time::Duration::from_secs(300), shutdown.clone());
    if let Some(cache) = &conversation_cache {
        cache.start_maintenance(
            shutdown.clone(),
            state.session_store.as_ref().map(|(store, _)| store.clone()),
        );
    }

    // Loop-liveness heartbeat: every 30s write state/gateway.heartbeat with a
    // memory sample, so an unclean death leaves pre-death telemetry and
    // /api/status shows current memory pressure. Only when we own the profile.
    if singleton {
        let hb_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = hb_shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        lifecycle_ledger::write_loop_heartbeat(None, Some(boot_epoch), None, None);
                    }
                }
            }
        });
    }

    // Push paths: for each configured platform, start the adapter's inbound
    // loop feeding a Dispatcher that runs turns and delivers replies. All share
    // the same AgentClient as /message.
    if let Some(token) = config.telegram_token.clone() {
        match telegram::TelegramAdapter::new(token) {
            Ok(tg) => start_push_path(
                hermes_core::Platform::Telegram,
                Arc::new(tg.with_audio_cache(config_file::hermes_home(), &state.user_config)),
                &state,
                shutdown.clone(),
            ),
            Err(err) => tracing::error!(%err, "telegram adapter init failed"),
        }
    }
    if let Some(token) = config.discord_token.clone() {
        match discord::DiscordAdapter::new(token) {
            Ok(dc) => start_push_path(
                hermes_core::Platform::Discord,
                Arc::new(dc.with_audio_cache(config_file::hermes_home(), &state.user_config)),
                &state,
                shutdown.clone(),
            ),
            Err(err) => tracing::error!(%err, "discord adapter init failed"),
        }
    }
    match (
        config.slack_app_token.clone(),
        config.slack_bot_token.clone(),
    ) {
        (Some(app), Some(bot)) => match slack::SlackAdapter::new(app, bot) {
            Ok(sl) => start_push_path(
                hermes_core::Platform::Slack,
                Arc::new(sl.with_audio_cache(config_file::hermes_home(), &state.user_config)),
                &state,
                shutdown.clone(),
            ),
            Err(err) => tracing::error!(%err, "slack adapter init failed"),
        },
        (Some(_), None) | (None, Some(_)) => {
            tracing::warn!(
                "slack needs both HERMES_SLACK_APP_TOKEN and HERMES_SLACK_BOT_TOKEN; skipping"
            )
        }
        (None, None) => {}
    }

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/status", get(status))
        .route("/message", post(post_message))
        .route("/display/:platform", get(get_display_config))
        .route("/search", get(get_search))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(addr = %config.bind, "hermes-gateway listening");

    // Startup work (adapter registration, DB recovery, ...) happens here as it
    // is ported. Once complete the gateway flips readiness on.
    state.mark_ready();
    if singleton {
        status::write_runtime_status(&status::StatusUpdate {
            gateway_state: Some(serde_json::json!("running")),
            ..Default::default()
        });
    }

    let server_shutdown = shutdown.clone();
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
        .await;
    shutdown.cancel();
    if let Some(cache) = conversation_cache {
        cache.shutdown(std::time::Duration::from_secs(45)).await;
    }
    server_result?;

    // Graceful shutdown finished: record it and release the singleton claims.
    if singleton {
        status::write_runtime_status(&status::StatusUpdate {
            gateway_state: Some(serde_json::json!("stopped")),
            exit_reason: Some(serde_json::json!("shutdown_signal")),
            ..Default::default()
        });
        lifecycle_ledger::mark_exited(Some(0), "graceful_shutdown", None);
        status::release_gateway_runtime_lock();
        status::remove_pid_file();
    }

    Ok(())
}

/// Resolve on the first SIGINT / SIGTERM. The caller cancels the shutdown
/// token, which drains the push paths and the HTTP server together (mirroring
/// the Python gateway's `shutdown_flush` / `drain_control` intent).
async fn wait_for_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn configured_auto_tool_backend_opens_extension_host_without_default_overhead() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        assert!(extensions_configured(
            &json!({"platform_toolsets":{"cli":["spotify"]}}),
            "cli",
            &repo,
        ));
        assert!(!extensions_configured(&json!({}), "cli", &repo));
        assert!(!extensions_configured(
            &json!({"platform_toolsets":{"telegram":["spotify"]}}),
            "cli",
            &repo,
        ));
    }
    use serde_json::{json, Value};

    fn native_config() -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            agent_python: "python3".into(),
            agent_cwd: ".".into(),
            agent_model: None,
            telegram_token: None,
            discord_token: None,
            slack_app_token: None,
            slack_bot_token: None,
            agent_native: true,
            llm_api_key: Some("fixture-key".into()),
            llm_base_url: None,
            agent_cli: None,
            agent_cli_args: None,
            agent_cli_prompt_flag: None,
            agent_tools: false,
        }
    }

    #[tokio::test]
    async fn conversation_prompt_is_persisted_before_model_io_and_reused_verbatim() {
        use axum::{extract::State, routing::post, Json, Router};

        type ModelState = (
            Arc<session_db::SessionDb>,
            Arc<std::sync::Mutex<Vec<Value>>>,
        );
        async fn model(
            State((database, requests)): State<ModelState>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let row = database.get_session("session-one").unwrap().unwrap();
            assert!(row["system_prompt"]
                .as_str()
                .is_some_and(|text| !text.is_empty()));
            assert_eq!(row["tool_names"], r#"["current_time"]"#);
            requests.lock().unwrap().push(body);
            Json(json!({"choices":[{"message":{"role":"assistant","content":"ok"}}]}))
        }

        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home =
            TempDir(std::env::temp_dir().join(format!("hermes-conversation-prompt-{nonce}")));
        std::fs::create_dir_all(&home.0).unwrap();
        std::fs::write(home.0.join("SOUL.md"), "Frozen test identity").unwrap();
        std::fs::write(
            home.0.join("config.yaml"),
            "model:\n  default: fixture-model\n  provider: openrouter\n",
        )
        .unwrap();
        let database = session_db::SessionDb::open_shared(home.0.join("state.db")).unwrap();
        database
            .create_session(
                "session-one",
                &session_db::SessionCreate {
                    peer: session_db::GatewayPeer {
                        source: "cli",
                        session_key: Some("route-one"),
                        chat_id: Some("chat"),
                        chat_type: Some("dm"),
                        ..Default::default()
                    },
                    cwd: home.0.to_str(),
                    ..Default::default()
                },
            )
            .unwrap();
        database
            .append_message("session-one", "user", "earlier")
            .unwrap();
        let history = database.load_history("session-one", 0).unwrap();

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/chat/completions", post(model))
            .with_state((database.clone(), requests.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut config = native_config();
        config.agent_model = Some("fixture-model".into());
        config.llm_base_url = Some(base_url);
        config.agent_cwd = home.0.clone();
        config.agent_tools = true;
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"next"
        }))
        .unwrap();
        message.resolved_session_id = Some("session-one".into());

        let initializer =
            conversation_prompt::Initializer::capture(home.0.clone(), home.0.clone()).unwrap();
        let first = secret_scope::with_secret_scope(
            Some(Default::default()),
            build_conversation_client(
                &config,
                &initializer,
                &home.0,
                &message,
                &history,
                Some(&database),
            ),
        )
        .await
        .unwrap();
        let stored = database.get_session("session-one").unwrap().unwrap()["system_prompt"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(stored.contains("Frozen test identity"));
        assert!(stored.contains("Model: fixture-model"));
        assert!(stored.contains("Provider: openrouter"));
        assert!(stored.contains("Platform: cli"));

        let run = |client: Arc<dyn AgentClient>| {
            let message = message.clone();
            let history = history.clone();
            async move {
                let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
                client.run_turn(&message, &history, sender).await.unwrap();
                while receiver.recv().await.is_some() {}
            }
        };
        run(first).await;

        // A new process-level initializer must restore the stored bytes without
        // consulting changed prompt sources for this continuing conversation.
        std::fs::write(home.0.join("SOUL.md"), "Changed identity must not appear").unwrap();
        // A fresh process now resolves no available tools. The saved session
        // prefix must retain current_time from the registered native catalog.
        config.agent_tools = false;
        let second_initializer =
            conversation_prompt::Initializer::capture(home.0.clone(), home.0.clone()).unwrap();
        let second = secret_scope::with_secret_scope(
            Some(Default::default()),
            build_conversation_client(
                &config,
                &second_initializer,
                &home.0,
                &message,
                &history,
                Some(&database),
            ),
        )
        .await
        .unwrap();
        run(second).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request["messages"][0]["role"], "system");
            assert_eq!(request["messages"][0]["content"], stored);
            assert!(!request["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Changed identity"));
            assert_eq!(request["tools"][0]["function"]["name"], "current_time");
        }
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        server.abort();
    }

    #[tokio::test]
    async fn configured_python_extension_reaches_native_prompt_and_tool_loop() {
        use axum::{extract::State, routing::post, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        type ModelState = (Arc<session_db::SessionDb>, Arc<AtomicUsize>);
        async fn model(
            State((database, calls)): State<ModelState>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let row = database.get_session("extension-session").unwrap().unwrap();
            let prompt = row["system_prompt"].as_str().unwrap();
            assert!(prompt.contains("## Plugin Context: fixture.rules"));
            assert!(prompt.contains("Plugin session extension-session"));
            assert!(prompt.contains("# Fixture Memory"));
            assert_eq!(row["tool_names"], r#"["fixture_plugin_tool"]"#);
            let history = database.load_history("extension-session", 0).unwrap();
            let current_user = history
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .expect("current user row");
            assert!(
                current_user
                    .api_content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("<memory-context>"),
                "history was {history:?}"
            );
            assert!(current_user
                .api_content
                .as_deref()
                .unwrap_or_default()
                .contains("recalled for continue database migration"));
            let call = calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                assert_eq!(body["tools"][0]["function"]["strict"], true);
                assert!(body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "user"
                        && message["content"].as_str().is_some_and(
                            |text| text.contains("recalled for continue database migration")
                        )));
                Json(
                    json!({"choices":[{"message":{"role":"assistant","content":null,
                    "tool_calls":[{"id":"extension-call","type":"function","function":{
                        "name":"fixture_plugin_tool","arguments":"{\"value\":\"hello\"}"
                    }}]}}]}),
                )
            } else {
                let result = body["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(result["name"], "fixture_plugin_tool");
                assert_eq!(
                    result["content"],
                    json!([{"type":"text","text":"plugin:extension-session:hello"}])
                );
                Json(json!({"choices":[{"message":{"role":"assistant","content":"done"}}]}))
            }
        }

        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home =
            TempDir(std::env::temp_dir().join(format!("hermes-native-extension-live-{nonce}")));
        let plugin = home.0.join("plugins/fixture-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            home.0.join("config.yaml"),
            "model:\n  default: fixture-model\n  provider: openrouter\nmemory:\n  provider: fixture-plugin\nplugins:\n  enabled: [fixture-plugin]\nplatform_toolsets:\n  cli: [fixture, memory]\n",
        )
        .unwrap();
        std::fs::write(
            plugin.join("plugin.yaml"),
            "name: fixture-plugin\nversion: 1.0.0\nkind: standalone\n",
        )
        .unwrap();
        std::fs::write(
            plugin.join("__init__.py"),
            r##"import json
import os
import sqlite3
from pathlib import Path
from agent.memory_provider import MemoryProvider

class FixtureMemory(MemoryProvider):
    name = "fixture-plugin"
    def initialize(self, session_id, **kwargs):
        self.session_id = session_id
        self.home = kwargs["hermes_home"]
    def is_available(self): return True
    def system_prompt_block(self): return "# Fixture Memory"
    def get_tool_schemas(self): return []
    def prefetch(self, query, **kwargs):
        Path(self.home, "memory-prefetch").write_text(query)
        return "recalled for " + query
    def sync_turn(self, user_content, assistant_content, **kwargs):
        with sqlite3.connect(Path(self.home, "state.db")) as connection:
            durable = connection.execute(
                "SELECT role, content FROM messages WHERE session_id=? ORDER BY id DESC LIMIT 1",
                (self.session_id,),
            ).fetchone()
        Path(self.home, "memory-sync").write_text(json.dumps(
            {"user": user_content, "assistant": assistant_content,
             "messages": kwargs.get("messages"), "durable": durable}, sort_keys=True
        ))
    def queue_prefetch(self, query, **kwargs):
        Path(self.home, "memory-queue-prefetch").write_text(query)
    def shutdown(self):
        Path(self.home, "memory-shutdown").write_text(self.session_id)

def fixture_tool(args, **kwargs):
    return {"_multimodal": True, "content": [{"type": "text", "text":
            "plugin:" + kwargs.get("session_id", "") + ":" + str(args.get("value", ""))}]}

def prompt_section(info):
    count = Path(os.environ["HERMES_HOME"], "prompt-callback-count")
    with count.open("a", encoding="utf-8") as handle:
        handle.write("called\n")
    return "Plugin session " + info["session_id"]

def register(ctx):
    ctx.register_memory_provider(FixtureMemory())
    ctx.register_system_prompt_section(
        "fixture.rules", prompt_section
    )
    ctx.register_tool(
        name="fixture_plugin_tool", toolset="fixture",
        schema={"description":"fixture","parameters":{"type":"object"},"strict":True},
        handler=fixture_tool,
    )
"##,
        )
        .unwrap();

        let database = session_db::SessionDb::open_shared(home.0.join("state.db")).unwrap();
        database
            .create_session(
                "extension-session",
                &session_db::SessionCreate {
                    peer: session_db::GatewayPeer {
                        source: "cli",
                        session_key: Some("route-extension"),
                        chat_id: Some("chat"),
                        chat_type: Some("dm"),
                        ..Default::default()
                    },
                    cwd: home.0.to_str(),
                    ..Default::default()
                },
            )
            .unwrap();
        database
            .append_message("extension-session", "user", "earlier")
            .unwrap();
        database
            .append_message("extension-session", "assistant", "earlier answer")
            .unwrap();
        let history = database.load_history("extension-session", 0).unwrap();
        database
            .append_message("extension-session", "user", "continue database migration")
            .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/chat/completions", post(model))
            .with_state((database.clone(), calls.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let mut config = native_config();
        config.agent_python = repo.join(".venv/bin/python").to_string_lossy().into_owned();
        config.agent_cwd = repo.clone();
        config.agent_model = Some("fixture-model".into());
        config.llm_base_url = Some(base_url);
        config.agent_tools = false;
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"continue database migration"
        }))
        .unwrap();
        message.resolved_session_id = Some("extension-session".into());
        let initializer =
            conversation_prompt::Initializer::capture(home.0.clone(), repo.clone()).unwrap();
        let client = secret_scope::with_secret_scope(
            Some(Default::default()),
            build_conversation_client(
                &config,
                &initializer,
                &home.0,
                &message,
                &history,
                Some(&database),
            ),
        )
        .await
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        client
            .run_turn_with_context(
                agent::TurnContext::from_database(Some(&database)),
                &message,
                &history,
                sender,
            )
            .await
            .unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }
        session_db::end_turn(Some(&database), false, &message, &reply);
        client
            .finalize_turn_after_persist(
                agent::TurnContext::from_database(Some(&database)),
                &message,
                &reply,
                true,
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(client);
        for _ in 0..100 {
            if home.0.join("memory-shutdown").is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(
            std::fs::read_to_string(home.0.join("memory-prefetch")).unwrap(),
            "continue database migration"
        );
        let synced: Value =
            serde_json::from_str(&std::fs::read_to_string(home.0.join("memory-sync")).unwrap())
                .unwrap();
        assert_eq!(synced["assistant"], "done");
        assert_eq!(synced["user"], "continue database migration");
        assert_eq!(synced["durable"], json!(["assistant", "done"]));
        let synced_messages = synced["messages"].as_array().unwrap();
        assert_eq!(synced_messages[0]["content"], "continue database migration");
        assert!(synced_messages[0]["api_content"]
            .as_str()
            .unwrap()
            .contains("recalled for continue database migration"));
        assert_eq!(synced_messages[1]["role"], "assistant");
        assert_eq!(synced_messages[2]["role"], "tool");
        assert_eq!(synced_messages.last().unwrap()["content"], "done");
        assert_eq!(
            std::fs::read_to_string(home.0.join("memory-queue-prefetch")).unwrap(),
            "continue database migration"
        );
        assert_eq!(
            std::fs::read_to_string(home.0.join("memory-shutdown")).unwrap(),
            "extension-session"
        );

        let resumed = secret_scope::with_secret_scope(
            Some(Default::default()),
            build_conversation_client(
                &config,
                &conversation_prompt::Initializer::capture(home.0.clone(), repo).unwrap(),
                &home.0,
                &message,
                &history,
                Some(&database),
            ),
        )
        .await
        .unwrap();
        drop(resumed);
        assert_eq!(
            std::fs::read_to_string(home.0.join("prompt-callback-count")).unwrap(),
            "called\n",
            "stored prompt reuse must not execute plugin prompt callbacks"
        );
        server.abort();
    }

    #[test]
    fn selected_base_profile_reaches_native_stream_and_tool_requests() {
        let _lock = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                for (name, value) in &self.0 {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
        struct TestHome(std::path::PathBuf);
        impl Drop for TestHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = format!("hermes-profile-startup-{}-{nonce}", std::process::id());
        let home = TestHome(std::env::temp_dir().join(directory));
        std::fs::create_dir(&home.0).unwrap();
        let _restore = RestoreEnv(
            [
                "HERMES_HOME",
                "FIREWORKS_API_KEY",
                "HERMES_MAX_TOKENS",
                "OPENROUTER_API_KEY",
            ]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect(),
        );
        std::env::set_var("HERMES_HOME", &home.0);
        std::env::set_var("FIREWORKS_API_KEY", "stale-shell-key");
        std::env::remove_var("HERMES_MAX_TOKENS");
        std::env::set_var("OPENROUTER_API_KEY", "stale-generic-key");
        std::fs::write(
            home.0.join(".env"),
            "FIREWORKS_API_KEY=fixture-key\nOPENROUTER_API_KEY=rotated-generic-key\n",
        )
        .unwrap();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {

            use axum::{http::HeaderMap, response::IntoResponse, routing::post, Json, Router};
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = requests.clone();
            let app = Router::new().route("/chat/completions", post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push((headers, body.clone()));
                    if body["stream"] == true {
                        ([("content-type", "text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n").into_response()
                    } else {
                        Json(json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]})).into_response()
                    }
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            struct Server(tokio::task::JoinHandle<()>);
            impl Drop for Server {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            let mut config = native_config();
            config.llm_base_url = Some(base);
            let user_config = json!({"model": {"provider": "fw", "base_url": "http://127.0.0.1:1"}});
            let message: hermes_core::Message = serde_json::from_value(
                json!({"platform": "cli", "channel_id": "c", "sender_id": "s", "text": "new turn"}),
            )
            .unwrap();
            let history = vec![
                session_db::HistoryMessage {
                    role: "user".into(),
                    content: "earlier".into(),
                    api_content: None,
                },
                session_db::HistoryMessage {
                    role: "assistant".into(),
                    content: "reply".into(),
                    api_content: None,
                },
            ];
            for tools in [false, true] {
                config.agent_tools = tools;
                // Exercise both explicit credentials and real profile-scoped file
                // loading through startup, including saved-key rotation precedence.
                config.llm_api_key = (!tools).then(|| "fixture-key".into());
                let agent = build_agent_client(&config, &user_config, Some("fixture-model"));
                let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                agent.run_turn(&message, &history, tx).await.unwrap();
                let mut saw_text = false;
                while let Some(event) = rx.recv().await {
                    if let hermes_core::StreamEvent::MessageChunk { text } = event {
                        saw_text |= text == "ok";
                    }
                }
                assert!(saw_text);
            }
            {
            let calls = requests.lock().unwrap();
            assert_eq!(calls.len(), 2);
            for (headers, body) in calls.iter() {
                assert_eq!(headers["authorization"], "Bearer fixture-key");
                assert_eq!(
                    headers["user-agent"],
                    format!("HermesAgent/{}", env!("CARGO_PKG_VERSION"))
                );
                assert_eq!(headers["x-title"], "Hermes Agent");
                assert_eq!(
                    body["messages"][0],
                    json!({"role": "user", "content": "earlier"})
                );
                assert_eq!(
                    body["messages"][1],
                    json!({"role": "assistant", "content": "reply"})
                );
                assert_eq!(body["model"], "fixture-model");
            }
            assert_eq!(calls[0].1["stream"], true);
            assert_eq!(calls[1].1["stream"], false);
            assert!(calls[1].1["tools"].is_array());
            }
            // Exercise the custom hook through the same startup and HTTP path,
            // with config resolution occurring before every client is built.
            for tools in [false, true] {
                for (model, agent_config, expected) in [
                    ("solar-pro3", json!({}), json!("medium")),
                    ("solar-pro3", json!({"reasoning_effort": false}), Value::Null),
                    ("solar-pro3", json!({"reasoning_effort": "minimal"}), Value::Null),
                    ("solar-pro3", json!({"reasoning_effort": "ultra"}), json!("high")),
                    ("solar-mini-250127", json!({"reasoning_effort": "high"}), Value::Null),
                    ("vendor/solar-pro3", json!({"reasoning_effort": "high", "reasoning_overrides": {"solar-pro3": "low"}}), json!("low")),
                ] {
                    config.agent_tools = tools;
                    config.llm_api_key = Some("fixture-key".into());
                    let selected = json!({"model": {"provider": "solar"}, "agent": agent_config});
                    let agent = build_agent_client(&config, &selected, Some(model));
                    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                    agent.run_turn(&message, &history, tx).await.unwrap();
                    while rx.recv().await.is_some() {}
                    let calls = requests.lock().unwrap();
                    let body = &calls.last().unwrap().1;
                    assert_eq!(body["reasoning_effort"], expected, "{selected}");
                    assert_eq!(body["stream"], !tools);
                    assert_eq!(body["messages"][0], json!({"role": "user", "content": "earlier"}));
                }
            }

            for tools in [false, true] {
                for (model, setting, expected) in [
                    ("Qwen/Qwen3.5-fast", Value::Null, json!("medium")),
                    ("deepseek-ai/DeepSeek-V4-Pro", json!("ultra"), json!("high")),
                    ("deepseek-ai/DeepSeek-R1", json!("minimal"), json!("low")),
                    ("openai/gpt-oss-120b", json!(false), Value::Null),
                    ("meta-llama/Llama-3.3", json!("high"), Value::Null),
                    ("gpt-oss/llama", json!("high"), Value::Null),
                ] {
                    config.agent_tools = tools;
                    config.llm_api_key = Some("fixture-key".into());
                    let selected = json!({"model": {"provider": "nebius"}, "agent": {"reasoning_effort": setting}});
                    let agent = build_agent_client(&config, &selected, Some(model));
                    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                    agent.run_turn(&message, &history, tx).await.unwrap();
                    while rx.recv().await.is_some() {}
                    let calls = requests.lock().unwrap();
                    let body = &calls.last().unwrap().1;
                    assert_eq!(body["reasoning_effort"], expected, "{selected}");
                    assert_eq!(body["stream"], !tools);
                }
            }
            for tools in [false, true] {
                for (model, raw, parameter, expected) in [
                    ("llama", json!(321), "max_tokens", json!(321)),
                    ("vendor/gpt-5.4", json!(321), "max_completion_tokens", json!(321)),
                    ("gpt-4o", json!("123"), "max_completion_tokens", json!(123)),
                    ("llama", json!("bad"), "max_tokens", Value::Null),
                ] {
                    config.agent_tools = tools;
                    let selected = json!({"model": {"provider": "fw", "max_tokens": raw}});
                    let agent = build_agent_client(&config, &selected, Some(model));
                    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                    agent.run_turn(&message, &history, tx).await.unwrap();
                    while rx.recv().await.is_some() {}
                    let calls = requests.lock().unwrap();
                    let body = &calls.last().unwrap().1;
                    assert_eq!(body[parameter], expected, "{selected}");
                    let other = if parameter == "max_tokens" { "max_completion_tokens" } else { "max_tokens" };
                    assert!(body.get(other).is_none());
                }
            }
            for tools in [false, true] {
                config.agent_tools = tools;
                config.llm_api_key = None;
                let agent = build_agent_client(&config, &json!({}), Some("fixture-model"));
                let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                agent.run_turn(&message, &history, tx).await.unwrap();
                while rx.recv().await.is_some() {}
                let calls = requests.lock().unwrap();
                assert_eq!(calls.last().unwrap().0["authorization"], "Bearer rotated-generic-key");
            }
            for (tools, keyed) in [(false,false),(true,false),(false,true),(true,true)] {
                config.agent_tools = tools;
                config.llm_api_key = Some("fixture-key".into());
                let mut selected = if keyed {
                    json!({"model":{"provider":"lab"},"providers":{"lab":{"api":config.llm_base_url.as_ref().unwrap(),"defaultModel":"fixture-model","extra_body":{"temperature":0.6,"custom_field":{"active":true}}}}})
                } else { json!({"model": {"provider": "custom:lab"}, "custom_providers": [{"name": "lab", "base_url": config.llm_base_url.as_ref().unwrap(), "model": "fixture-model", "extra_body": {"temperature": 0.6, "custom_field": {"active": true}}}]}) };
                let entry = if keyed { &mut selected["providers"]["lab"] } else { &mut selected["custom_providers"][0] };
                entry["extra_headers"] = json!({"X-Route-Token":"header-fixture", "Authorization":"Bearer custom-fixture"});
                let agent = build_agent_client(&config, &selected, Some("fixture-model"));
                let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                agent.run_turn(&message, &history, tx).await.unwrap();
                while rx.recv().await.is_some() {}
                let calls = requests.lock().unwrap();
                let body = &calls.last().unwrap().1;
                assert_eq!(body["temperature"], 0.6);
                assert_eq!(body["custom_field"], json!({"active": true}));
                assert_eq!(calls.last().unwrap().0["x-route-token"], "header-fixture");
                assert_eq!(calls.last().unwrap().0["authorization"], "Bearer custom-fixture");
                assert!(body.get("extra_body").is_none());
                assert_eq!(body["messages"][0], json!({"role": "user", "content": "earlier"}));
            }
            // An explicit endpoint override must not inherit the named entry's
            // proxy credential when the configured route no longer matches.
            for tools in [false, true] {
                config.agent_tools = tools;
                let selected = json!({"model":{"provider":"lab"},"providers":{"lab":{
                    "api":"http://127.0.0.1:1/old-route",
                    "extra_headers":{"X-Route-Token":"must-not-send", "Authorization":"Bearer must-not-send"}
                }}});
                let agent = build_agent_client(&config, &selected, Some("fixture-model"));
                let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                agent.run_turn(&message, &history, tx).await.unwrap();
                while rx.recv().await.is_some() {}
                let calls = requests.lock().unwrap();
                let headers = &calls.last().unwrap().0;
                assert!(!headers.contains_key("x-route-token"));
                assert_eq!(headers["authorization"], "Bearer fixture-key");
            }
            // Follow a named provider's fallback cap through client construction
            // and both HTTP paths, including explicit zero and invalid env input.
            for tools in [false, true] {
                for (global, environment, expected) in [
                    (json!(null), None, 256),
                    (json!(128), None, 128),
                    (json!(0), None, 0),
                    (json!(128), Some("64"), 64),
                    (json!(128), Some("invalid"), 256),
                    (json!("128"), None, 256),
                ] {
                    match environment {
                        Some(value) => std::env::set_var("HERMES_MAX_TOKENS", value),
                        None => std::env::remove_var("HERMES_MAX_TOKENS"),
                    }
                    config.agent_tools = tools;
                    let selected = json!({
                        "model": {"provider": "lab", "max_tokens": global},
                        "providers": {"lab": {
                            "api": config.llm_base_url.as_ref().unwrap(),
                            "max_output_tokens": 256, "max_tokens": 512
                        }}
                    });
                    let agent = build_agent_client(&config, &selected, Some("fixture-model"));
                    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                    agent.run_turn(&message, &history, tx).await.unwrap();
                    while rx.recv().await.is_some() {}
                    let calls = requests.lock().unwrap();
                    assert_eq!(calls.last().unwrap().1["max_tokens"], expected);
                }
            }
            std::env::remove_var("HERMES_MAX_TOKENS");
            for tools in [false, true] {
                for (provider, flag, expected) in [
                    ("fw", json!(true), true), ("fw", json!(false), false),
                    ("fw", json!("enabled"), true), ("kimi-coding", json!(false), true),
                    ("KIMI-CODING", json!(false), false), ("deepseek", json!(false), true),
                ] {
                    config.agent_tools = tools;
                    let selected = json!({"model": {"provider": provider, "reasoning_echo": flag}});
                    let agent = build_agent_client(&config, &selected, Some("fixture-model"));
                    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                    agent.run_turn(&message, &history, tx).await.unwrap();
                    while rx.recv().await.is_some() {}
                    let calls = requests.lock().unwrap();
                    let assistant = &calls.last().unwrap().1["messages"][1];
                    if expected { assert_eq!(assistant["reasoning_content"], " "); }
                    else { assert!(assistant.get("reasoning_content").is_none()); }
                }
            }
            for tools in [false, true] {
                for (model, effort, cap, expected) in [
                    ("google/gemini-3-flash", json!("ultra"), 1024, 65535),
                    ("gemini-3-pro", json!("low"), 2048, 65535),
                    ("gemini-2.5-flash", json!(false), 1024, 1024),
                    ("gemini-3-pro", json!(false), 0, 65535),
                    ("gemini-3-flash", json!("high"), 70000, 70000),
                    ("gemma-3", json!("high"), 1024, 1024),
                    ("openrouter/google/gemini-3-flash", json!("high"), 1024, 1024),
                ] {
                    config.agent_tools = tools;
                    config.llm_api_key = Some("fixture-key".into());
                    let selected = json!({"model": {"provider": "fw", "max_tokens": cap}, "agent": {"reasoning_effort": effort}});
                    let agent = build_agent_client(&config, &selected, Some(model));
                    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                    agent.run_turn(&message, &history, tx).await.unwrap();
                    while rx.recv().await.is_some() {}
                    let calls = requests.lock().unwrap();
                    assert_eq!(calls.last().unwrap().1["max_tokens"], expected, "{selected}");
                }
            }
            // Two routed homes build independent clients without rewriting
            // ambient HERMES_HOME or borrowing its saved credential file.
            let red = home.0.join("profiles/red");
            let blue = home.0.join("profiles/blue");
            for (path, key) in [(&red, "red-profile-key"), (&blue, "blue-profile-key")] {
                std::fs::create_dir_all(path).unwrap();
                std::fs::write(path.join(".env"), format!("FIREWORKS_API_KEY={key}\n")).unwrap();
            }
            config.agent_tools = false;
            config.llm_api_key = None;
            let selected = json!({"model":{"provider":"fw"}});
            let red_agent = build_agent_client_for_home(&config, &selected, Some("red-model"), &red, None).unwrap();
            let blue_agent = build_agent_client_for_home(&config, &selected, Some("blue-model"), &blue, None).unwrap();
            let (red_tx, mut red_rx) = tokio::sync::mpsc::channel(32);
            let (blue_tx, mut blue_rx) = tokio::sync::mpsc::channel(32);
            let (red_result, blue_result) = tokio::join!(
                red_agent.run_turn(&message, &history, red_tx),
                blue_agent.run_turn(&message, &history, blue_tx),
            );
            red_result.unwrap();
            blue_result.unwrap();
            while red_rx.recv().await.is_some() {}
            while blue_rx.recv().await.is_some() {}
            {
            let calls = requests.lock().unwrap();
            let recent = &calls[calls.len()-2..];
            for (model, key) in [("red-model", "Bearer red-profile-key"), ("blue-model", "Bearer blue-profile-key")] {
                let (headers, _) = recent.iter().find(|(_, body)| body["model"] == model).unwrap();
                assert_eq!(headers["authorization"], key);
            }
            assert_eq!(std::env::var_os("HERMES_HOME").unwrap(), home.0.as_os_str());
            }

            struct RestoreMultiplex(bool);
            impl Drop for RestoreMultiplex {
                fn drop(&mut self) { secret_scope::set_multiplex_active(self.0); }
            }
            let _multiplex = RestoreMultiplex(secret_scope::is_multiplex_active());
            secret_scope::set_multiplex_active(true);
            assert!(build_agent_client_for_home(&config, &selected, Some("scoped-model"), &red, None).is_err());
            for provider_config in [&selected, &json!({})] {
                secret_scope::with_secret_scope(Some(Default::default()), async {
                    // Neither a populated .env nor stale process keys may fill
                    // an empty authoritative scope, for registered or generic providers.
                    assert!(build_agent_client_for_home(&config, provider_config, Some("scoped-model"), &red, None).is_err());
                }).await;
            }
            let scoped_agent = secret_scope::with_secret_scope(Some(std::collections::HashMap::from([
                ("FIREWORKS_API_KEY".into(), "hydrated-profile-key".into()),
            ])), async {
                build_agent_client_for_home(&config, &selected, Some("scoped-model"), &red, None).unwrap()
            }).await;
            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
            scoped_agent.run_turn(&message, &history, tx).await.unwrap();
            while rx.recv().await.is_some() {}
            assert_eq!(requests.lock().unwrap().last().unwrap().0["authorization"], "Bearer hydrated-profile-key");
        });
    }

    #[test]
    fn unported_api_mode_and_missing_required_endpoint_use_existing_bridge() {
        let config = native_config();
        for provider in ["xai", "codex", "azure"] {
            let agent = build_agent_client(
                &config,
                &json!({"model": {"provider": provider}}),
                Some("fixture-model"),
            );
            assert!(
                !agent.supports_structured_content(),
                "provider {provider} must not become a generic native chat client"
            );
        }
    }
}
