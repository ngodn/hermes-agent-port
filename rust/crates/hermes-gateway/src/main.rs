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
mod background_process;
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
mod compression_auxiliary;
mod compression_discovery;
mod compression_handoff;
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
mod dangerous_command;
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
mod foreground_exec;
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
mod main_dropped_stream;
mod main_provider_notices;
mod main_provider_timeouts;
mod main_provider_truncation;
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
mod micro_compaction;
mod mime_types;
mod mirror;
mod models_dev;
mod native_agent;
mod native_image_content;
mod native_process;
mod native_terminal;
mod native_tools;
mod nous_credentials;
mod ogg_opus_duration;
mod ollama_glm_truncation;
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
mod python_fnmatch;
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
mod terminal_guard;
mod think_scrubber;
mod threat_patterns;
mod tool_approval;
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
mod turn_session;
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
    hooks: Option<Arc<crate::hooks::HookRegistry>>,
    platform: String,
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

fn native_local_terminal_eligible(config: &serde_json::Value, platform: &str) -> bool {
    let backend = config["terminal"]["backend"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("local");
    let approval_mode = match config["approvals"].get("mode") {
        None => "smart",
        Some(serde_json::Value::Bool(false)) => "off",
        Some(serde_json::Value::String(mode)) if mode.trim().eq_ignore_ascii_case("off") => "off",
        Some(serde_json::Value::String(mode)) if mode.trim().eq_ignore_ascii_case("smart") => {
            "smart"
        }
        _ => "manual",
    };
    let deny_is_valid = match config["approvals"].get("deny") {
        None | Some(serde_json::Value::Null | serde_json::Value::Array(_)) => true,
        Some(_) => false,
    };
    let manual_push = approval_mode == "manual"
        && matches!(platform, "telegram" | "discord" | "slack")
        && config["security"]["tirith_enabled"].as_bool() == Some(false);
    cfg!(unix) && backend == "local" && deny_is_valid && (approval_mode == "off" || manual_push)
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
        if !name.is_empty() && !base.iter().any(|tool| tool.spec().name == name) {
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

enum CompressionRouteConfig<'a> {
    Primary(&'a compression_auxiliary::Config),
    TaskFallback {
        entry: &'a compression_auxiliary::FallbackChainEntry,
        task: &'a compression_auxiliary::Config,
    },
    MainFallback {
        entry: &'a compression_auxiliary::FallbackChainEntry,
        task: &'a compression_auxiliary::Config,
    },
    BuiltinDiscovery {
        entry: &'a compression_auxiliary::FallbackChainEntry,
        task: &'a compression_auxiliary::Config,
    },
}

impl CompressionRouteConfig<'_> {
    fn provider(&self) -> &str {
        match self {
            Self::Primary(config) => &config.provider,
            Self::TaskFallback { entry, .. }
            | Self::MainFallback { entry, .. }
            | Self::BuiltinDiscovery { entry, .. } => &entry.provider,
        }
    }

    fn is_fallback(&self) -> bool {
        !matches!(self, Self::Primary(_))
    }

    fn model(&self) -> Option<&str> {
        match self {
            Self::Primary(config) => config.model.as_deref(),
            Self::TaskFallback { entry, .. }
            | Self::MainFallback { entry, .. }
            | Self::BuiltinDiscovery { entry, .. } => entry.model.as_deref(),
        }
    }

    fn base_url(&self) -> Option<&str> {
        match self {
            Self::Primary(config) => config.base_url.as_deref(),
            Self::TaskFallback { entry, .. }
            | Self::MainFallback { entry, .. }
            | Self::BuiltinDiscovery { entry, .. } => entry.base_url.as_deref(),
        }
    }

    fn api_mode(&self) -> Option<&str> {
        match self {
            Self::Primary(config) => config.api_mode.as_deref(),
            Self::TaskFallback { entry, .. }
            | Self::MainFallback { entry, .. }
            | Self::BuiltinDiscovery { entry, .. } => entry.api_mode.as_deref(),
        }
    }

    fn timeout(&self) -> std::time::Duration {
        match self {
            Self::Primary(config) => config.timeout,
            Self::TaskFallback { entry, task } => entry.timeout.unwrap_or(task.timeout),
            // Python's `_fallback_entry_timeout` only resolves
            // `auxiliary.<task>.fallback_chain` labels. Top-level main-chain
            // entries retain the task timeout even if they carry this key.
            Self::MainFallback { task, .. } | Self::BuiltinDiscovery { task, .. } => task.timeout,
        }
    }

    fn reasoning_config(
        &self,
        actual_provider: &str,
        actual_model: &str,
    ) -> Option<serde_json::Value> {
        match self {
            Self::Primary(config) => config.reasoning_config.clone(),
            Self::TaskFallback { entry, .. }
                if entry.certifies_route(actual_provider, actual_model) =>
            {
                Some(serde_json::json!({"enabled": false, "effort": "none"}))
            }
            Self::TaskFallback { .. }
            | Self::MainFallback { .. }
            | Self::BuiltinDiscovery { .. } => None,
        }
    }

    fn request_extra_body(
        &self,
        actual_provider: &str,
        actual_model: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        match self {
            Self::Primary(config) => config.extra_body.clone(),
            Self::TaskFallback { entry, task }
            | Self::MainFallback { entry, task }
            | Self::BuiltinDiscovery { entry, task } => {
                let certified = matches!(self, Self::TaskFallback { .. })
                    && entry.certifies_route(actual_provider, actual_model);
                let mut body = task.extra_body.clone();
                if task.claims_fast_lane() && !certified {
                    body.shift_remove("reasoning");
                }
                if certified {
                    body.entry("reasoning")
                        .or_insert_with(|| serde_json::json!({"enabled": false, "effort": "none"}));
                }
                body
            }
        }
    }

    fn direct_api_key(
        &self,
        dotenv: &std::collections::HashMap<String, String>,
        environment: &mut impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        match self {
            Self::Primary(config) => config.direct_api_key(dotenv, environment),
            Self::TaskFallback { entry, .. }
            | Self::MainFallback { entry, .. }
            | Self::BuiltinDiscovery { entry, .. } => entry.direct_api_key(dotenv, environment),
        }
    }

    fn certified_output_cap(&self, actual_provider: &str, actual_model: &str) -> Option<u64> {
        match self {
            Self::Primary(config) => config.certified_output_cap(actual_provider, actual_model),
            Self::TaskFallback { entry, .. } => {
                entry.certified_output_cap(actual_provider, actual_model)
            }
            Self::MainFallback { .. } | Self::BuiltinDiscovery { .. } => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_native_compression_discovery(
    task: &compression_auxiliary::Config,
    user_config: &serde_json::Value,
    main_model: &str,
    main_key: &str,
    main_base_url: &str,
    main_profile: Option<&provider_registry::ProviderProfile>,
    profiles: &provider_registry::ProviderRegistry,
    dotenv: &std::collections::HashMap<String, String>,
    environment: &mut impl FnMut(&str) -> Option<String>,
    home: &std::path::Path,
) -> Vec<(
    NativeAgentClient,
    Option<native_agent::CompressionPoolCredential>,
)> {
    fn secret(
        name: &str,
        dotenv: &std::collections::HashMap<String, String>,
        environment: &mut impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        dotenv
            .get(name)
            .cloned()
            .or_else(|| environment(name))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    let profile_auth = home.join("auth.json");
    let root = config_file::hermes_root();
    let root_auth =
        (home.parent() == Some(root.join("profiles").as_path())).then(|| root.join("auth.json"));
    let pool_runtime = |provider: &str| {
        let locator = credential_pool::PoolLocator::new(
            profile_auth.clone(),
            root_auth.clone(),
            provider,
            credential_pool::pool_strategy(provider, user_config),
        );
        match locator.select_runtime() {
            Ok(Some(runtime)) => Some((locator, runtime)),
            Ok(None) => None,
            Err(error) => {
                tracing::debug!(
                    provider,
                    error = %compression_redact::redact(&error.to_string()),
                    "compression credential pool unavailable"
                );
                None
            }
        }
    };

    type DiscoveryEntry = (
        serde_json::Value,
        Option<native_agent::CompressionPoolCredential>,
    );
    let mut entries: Vec<DiscoveryEntry> = Vec::new();
    let openrouter_model = user_config["auxiliary"]["openrouter_model"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("nvidia/nemotron-3-ultra-550b-a55b:free");
    let free_only = python_value::truthy(&user_config["auxiliary"]["free_only"]);
    let openrouter_free =
        openrouter_model.ends_with(":free") || openrouter_model.starts_with("stealth/");
    if !free_only || openrouter_free {
        let fallback_base_url = "https://openrouter.ai/api/v1".to_owned();
        let pooled = pool_runtime("openrouter");
        let api_key = pooled
            .as_ref()
            .map(|(_, runtime)| runtime.api_key().to_owned())
            .or_else(|| secret("OPENROUTER_API_KEY", dotenv, environment));
        if let Some(api_key) = api_key {
            let base_url = pooled
                .as_ref()
                .and_then(|(_, runtime)| runtime.base_url())
                .unwrap_or(&fallback_base_url)
                .to_owned();
            let binding = pooled.map(|(locator, runtime)| {
                native_agent::CompressionPoolCredential::new(
                    locator,
                    runtime,
                    fallback_base_url.clone(),
                )
            });
            entries.push((
                serde_json::json!({
                    "provider":"openrouter",
                    "model":openrouter_model,
                    "base_url":base_url,
                    "api_key":api_key,
                }),
                binding,
            ));
        }
    }

    let shared_auth = secret("HERMES_SHARED_AUTH_DIR", dotenv, environment)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            root_auth
                .as_ref()
                .and_then(|path| path.parent())
                .unwrap_or(home)
                .join("shared")
        })
        .join("nous_auth.json");
    let portal_override = secret("HERMES_PORTAL_BASE_URL", dotenv, environment)
        .or_else(|| secret("NOUS_PORTAL_BASE_URL", dotenv, environment));
    let inference_override = secret("NOUS_INFERENCE_BASE_URL", dotenv, environment);
    let refresh_timeout = secret("HERMES_NOUS_TIMEOUT_SECONDS", dotenv, environment)
        .and_then(|value| value.parse::<f64>().ok())
        .and_then(|value| std::time::Duration::try_from_secs_f64(value).ok())
        .filter(|value| !value.is_zero())
        .unwrap_or_else(|| std::time::Duration::from_secs(15));
    let nous_locator = nous_credentials::Locator::new(
        profile_auth.clone(),
        root_auth.clone(),
        shared_auth,
        portal_override,
        inference_override,
        refresh_timeout,
    );
    if nous_locator.is_configured() {
        entries.push((
            serde_json::json!({
                "provider":"nous",
                "model":nous_credentials::DEFAULT_MODEL,
                "base_url":nous_credentials::DEFAULT_INFERENCE_URL,
                "api_key":"oauth-resolved-before-request",
            }),
            Some(native_agent::CompressionPoolCredential::new_nous(
                nous_locator,
                nous_credentials::DEFAULT_INFERENCE_URL,
            )),
        ));
    }

    let requested_main = user_config["model"]["provider"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if (requested_main == "custom" || requested_main.starts_with("custom:"))
        && !main_base_url
            .to_lowercase()
            .starts_with("https://chatgpt.com/backend-api/codex")
    {
        entries.push((
            serde_json::json!({
                "provider":requested_main,
                "model":main_model,
                "base_url":main_base_url,
                "api_key":main_key,
            }),
            None,
        ));
    }

    for profile in compression_discovery::ordered_native_profiles(profiles) {
        let fallback_base_url = profile
            .env_vars
            .iter()
            .find(|name| name.ends_with("_URL"))
            .and_then(|name| dotenv.get(name).cloned().or_else(|| environment(name)))
            .map(|value| value.trim().trim_end_matches('/').to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| profile.base_url.clone());
        let pooled = pool_runtime(&profile.name);
        let mut entry = serde_json::json!({
            "provider":profile.name,
            "model":profile.default_aux_model,
        });
        if let Some((_, runtime)) = &pooled {
            entry["api_key"] = serde_json::json!(runtime.api_key());
            if let Some(base_url) = runtime.base_url() {
                entry["base_url"] = serde_json::json!(base_url);
            }
        }
        let binding = pooled.map(|(locator, runtime)| {
            native_agent::CompressionPoolCredential::new(locator, runtime, fallback_base_url)
        });
        entries.push((entry, binding));
    }

    entries
        .iter()
        .filter_map(|(value, binding)| {
            compression_auxiliary::FallbackChainEntry::from_value(value)
                .map(|entry| (entry, binding.clone()))
        })
        .filter_map(|(entry, binding)| {
            match build_native_compression_client(
                CompressionRouteConfig::BuiltinDiscovery {
                    entry: &entry,
                    task,
                },
                user_config,
                main_model,
                main_key,
                main_base_url,
                main_profile,
                profiles,
                dotenv,
                environment,
                home,
            ) {
                Ok(Some(client)) => Some((client, binding.clone())),
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(
                        provider = entry.provider,
                        error = %compression_redact::redact(&error.to_string()),
                        "built-in compression provider is unavailable"
                    );
                    None
                }
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_native_compression_client(
    route: CompressionRouteConfig<'_>,
    user_config: &serde_json::Value,
    main_model: &str,
    main_key: &str,
    main_base_url: &str,
    main_profile: Option<&provider_registry::ProviderProfile>,
    profiles: &provider_registry::ProviderRegistry,
    dotenv: &std::collections::HashMap<String, String>,
    environment: &mut impl FnMut(&str) -> Option<String>,
    home: &std::path::Path,
) -> anyhow::Result<Option<NativeAgentClient>> {
    // Python retains provider-only entries while parsing, then skips them at
    // route resolution because compression fallbacks require both fields.
    if route.is_fallback() && route.model().is_none() {
        return Ok(None);
    }
    let configured_main_provider = user_config["model"]["provider"]
        .as_str()
        .unwrap_or("")
        .trim();
    let inherits_main = matches!(route.provider(), "auto" | "main");
    let requested_provider = if inherits_main {
        main_profile
            .map(|profile| profile.name.as_str())
            .filter(|provider| !provider.is_empty())
            .unwrap_or(configured_main_provider)
    } else {
        route.provider()
    };
    let auxiliary_profile = if inherits_main {
        main_profile.cloned()
    } else {
        profiles
            .get(requested_provider)
            .map(|profile| profile.read().unwrap().clone())
    };
    let named = custom_provider_config::named(
        user_config,
        requested_provider,
        auxiliary_profile
            .as_ref()
            .map(|profile| profile.name.as_str()),
        |name| {
            environment(name)
                .or_else(|| dotenv.get(name).cloned())
                .unwrap_or_default()
        },
    );

    let same_as_main = inherits_main
        || requested_provider.eq_ignore_ascii_case(configured_main_provider)
        || main_profile.is_some_and(|profile| {
            profile.name.eq_ignore_ascii_case(requested_provider)
                || profile
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(requested_provider))
        });
    let profile_base_url = auxiliary_profile.as_ref().and_then(|profile| {
        profile
            .env_vars
            .iter()
            .find(|name| name.ends_with("_URL"))
            .and_then(|name| dotenv.get(name).cloned().or_else(|| environment(name)))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| (!profile.base_url.is_empty()).then(|| profile.base_url.clone()))
    });
    let base_url = route
        .base_url()
        .map(str::to_owned)
        .or_else(|| {
            named
                .as_ref()
                .and_then(|entry| entry["base_url"].as_str())
                .map(str::to_owned)
        })
        .or(profile_base_url)
        .or_else(|| same_as_main.then(|| main_base_url.to_owned()))
        .ok_or_else(|| anyhow::anyhow!("auxiliary compression route has no endpoint"))?;

    let model = route
        .model()
        .map(str::to_owned)
        .or_else(|| inherits_main.then(|| main_model.to_owned()))
        .or_else(|| {
            auxiliary_profile
                .as_ref()
                .map(|profile| profile.default_aux_model.trim())
                .filter(|model| !model.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            named
                .as_ref()
                .and_then(|entry| entry["model"].as_str())
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| main_model.to_owned());

    let api_mode = route
        .api_mode()
        .map(str::to_owned)
        .or_else(|| {
            named
                .as_ref()
                .and_then(|entry| entry["api_mode"].as_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            auxiliary_profile
                .as_ref()
                .map(|profile| profile.api_mode.clone())
        })
        .unwrap_or_else(|| "chat_completions".into());
    anyhow::ensure!(
        api_mode == "chat_completions",
        "auxiliary compression provider requires unsupported native API mode {api_mode}"
    );

    let direct_key = route.direct_api_key(dotenv, environment);
    let named_key = named
        .as_ref()
        .and_then(|entry| entry["api_key"].as_str())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned);
    let profile_key = auxiliary_profile.as_ref().and_then(|profile| {
        config_file::resolve_profile_api_key(profile, dotenv, |name| environment(name))
    });
    let api_key = direct_key
        .or(named_key)
        .or_else(|| same_as_main.then(|| main_key.to_owned()))
        .or(profile_key)
        .or_else(|| {
            config_file::resolve_provider_api_key_with_env(&base_url, dotenv, |name| {
                environment(name)
            })
        })
        .or_else(|| {
            crate::local_probe::is_local_endpoint(&base_url).then(|| "no-key-required".into())
        })
        .ok_or_else(|| anyhow::anyhow!("no API key resolved for auxiliary compression route"))?;

    let actual_provider = auxiliary_profile
        .as_ref()
        .map(|profile| profile.name.as_str())
        .or_else(|| (!requested_provider.is_empty()).then_some(requested_provider))
        .unwrap_or("custom");
    let reasoning_config = route.reasoning_config(actual_provider, &model);
    let mut client = NativeAgentClient::new(&model, api_key, &base_url)?
        .with_provider_identity(actual_provider)
        .with_reasoning_config(reasoning_config.clone())
        .with_summary_request_policy(
            route.timeout(),
            route.certified_output_cap(actual_provider, &model),
        );
    if let Some(context_length) = models_dev::ModelsDev::new(home.to_path_buf(), user_config)
        .cached_context_window(actual_provider, &model, user_config)
    {
        client = client.with_context_length(context_length);
    }
    if let Some(profile) = &auxiliary_profile {
        client = client.with_provider_profile(profile)?;
    }
    if actual_provider.eq_ignore_ascii_case("openrouter") {
        client = client.with_provider_default_headers(
            serde_json::json!({
                "HTTP-Referer":"https://hermes-agent.nousresearch.com",
                "X-Title":"Hermes Agent",
                "X-OpenRouter-Categories":"productivity,cli-agent",
            })
            .as_object()
            .unwrap(),
        )?;
    }
    client = client.with_extra_headers(&custom_provider_config::extra_headers(
        user_config,
        &base_url,
    ))?;

    let mut extra_body = named
        .as_ref()
        .and_then(|entry| entry["extra_body"].as_object())
        .cloned()
        .unwrap_or_default();
    extra_body.extend(route.request_extra_body(actual_provider, &model));
    if let Some(reasoning) = reasoning_config {
        extra_body.entry("reasoning").or_insert(reasoning);
    }
    if !extra_body.is_empty() {
        client = client.with_request_overrides(serde_json::Map::from_iter([(
            "extra_body".into(),
            serde_json::Value::Object(extra_body),
        )]));
    }
    Ok(Some(client))
}

/// Build one frozen ordinary main-turn fallback route. This intentionally
/// shares provider/profile request shaping with the main client, but excludes
/// auxiliary summary policy and rejects transports that are not yet native.
#[allow(clippy::too_many_arguments)]
fn build_native_main_fallback_client(
    entry: &compression_auxiliary::FallbackChainEntry,
    user_config: &serde_json::Value,
    profiles: &provider_registry::ProviderRegistry,
    dotenv: &std::collections::HashMap<String, String>,
    environment: &mut impl FnMut(&str) -> Option<String>,
    home: &std::path::Path,
) -> anyhow::Result<Option<NativeAgentClient>> {
    let Some(model) = entry.model.as_deref() else {
        return Ok(None);
    };
    let requested_provider = entry.provider.trim();
    let profile = profiles
        .get(requested_provider)
        .map(|profile| profile.read().unwrap().clone());
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
    let api_mode = entry
        .api_mode
        .clone()
        .or_else(|| {
            named
                .as_ref()
                .and_then(|value| value["api_mode"].as_str())
                .map(str::to_owned)
        })
        .or_else(|| profile.as_ref().map(|profile| profile.api_mode.clone()))
        .unwrap_or_else(|| "chat_completions".into());
    if api_mode != "chat_completions" {
        return Ok(None);
    }

    let profile_base_url = profile.as_ref().and_then(|profile| {
        profile
            .env_vars
            .iter()
            .find(|name| name.ends_with("_URL"))
            .and_then(|name| dotenv.get(name).cloned().or_else(|| environment(name)))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| (!profile.base_url.is_empty()).then(|| profile.base_url.clone()))
    });
    let fallback_base_url = entry
        .base_url
        .clone()
        .or_else(|| {
            named
                .as_ref()
                .and_then(|value| value["base_url"].as_str())
                .map(str::to_owned)
        })
        .or(profile_base_url)
        .ok_or_else(|| anyhow::anyhow!("main fallback route has no endpoint"))?;

    let direct_key = entry.direct_api_key(dotenv, &mut *environment);
    let named_key = named
        .as_ref()
        .and_then(|value| value["api_key"].as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let static_key = direct_key.or(named_key);
    let actual_provider = profile
        .as_ref()
        .map(|profile| profile.name.as_str())
        .filter(|provider| !provider.is_empty())
        .unwrap_or(requested_provider);
    let pool_provider = actual_provider.trim().to_lowercase();
    let pool_supported = !matches!(
        pool_provider.as_str(),
        "" | "auto" | "custom" | "anthropic" | "openai-codex" | "xai-oauth" | "nous"
    ) && !pool_provider.starts_with("custom:");
    let profile_auth = home.join("auth.json");
    let root = config_file::hermes_root();
    let root_auth =
        (home.parent() == Some(root.join("profiles").as_path())).then(|| root.join("auth.json"));
    let pool_runtime = (static_key.is_none() && pool_supported)
        .then(|| {
            let locator = credential_pool::PoolLocator::new(
                profile_auth,
                root_auth,
                &pool_provider,
                credential_pool::pool_strategy(&pool_provider, user_config),
            );
            match locator.select_runtime() {
                Ok(Some(runtime)) => Some((locator, runtime)),
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(
                        provider = pool_provider,
                        error = %compression_redact::redact(&error.to_string()),
                        "main fallback credential pool unavailable"
                    );
                    None
                }
            }
        })
        .flatten();
    let api_key = static_key
        .or_else(|| {
            pool_runtime
                .as_ref()
                .map(|(_, runtime)| runtime.api_key().to_owned())
        })
        .or_else(|| {
            profile.as_ref().and_then(|profile| {
                config_file::resolve_profile_api_key(profile, dotenv, &mut *environment)
            })
        })
        .or_else(|| {
            config_file::resolve_provider_api_key_with_env(
                &fallback_base_url,
                dotenv,
                &mut *environment,
            )
        })
        .or_else(|| {
            local_probe::is_local_endpoint(&fallback_base_url).then(|| "no-key-required".into())
        })
        .ok_or_else(|| anyhow::anyhow!("no API key resolved for main fallback route"))?;
    let base_url = pool_runtime
        .as_ref()
        .and_then(|(_, runtime)| runtime.base_url())
        .unwrap_or(&fallback_base_url)
        .to_owned();

    let reasoning_config = reasoning_effort::resolve_config(user_config, model);
    let main_timeouts =
        main_provider_timeouts::Policy::resolve(user_config, actual_provider, model, |name| {
            environment(name).or_else(|| dotenv.get(name).cloned())
        });
    let mut client = NativeAgentClient::new(model, api_key, &base_url)?
        .with_provider_identity(actual_provider)
        .with_main_timeouts(main_timeouts)
        .with_reasoning_config(reasoning_config)
        .with_reasoning_echo(
            entry.reasoning_echo || reasoning_replay::needs_echo(actual_provider, model, &base_url),
        )
        .with_output_cap(native_agent::resolve_output_cap(
            &user_config["model"]["max_tokens"],
            environment("HERMES_MAX_TOKENS").as_deref(),
            named
                .as_ref()
                .and_then(|value| value.get("max_output_tokens")),
        ));
    if let Some(context_length) = models_dev::ModelsDev::new(home.to_path_buf(), user_config)
        .cached_context_window(actual_provider, model, user_config)
    {
        client = client.with_context_length(context_length);
    }
    if let Some(profile) = &profile {
        client = client.with_provider_profile(profile)?;
    }
    if actual_provider.eq_ignore_ascii_case("openrouter") {
        client = client.with_provider_default_headers(
            serde_json::json!({
                "HTTP-Referer":"https://hermes-agent.nousresearch.com",
                "X-Title":"Hermes Agent",
                "X-OpenRouter-Categories":"productivity,cli-agent",
            })
            .as_object()
            .unwrap(),
        )?;
    }
    client = client.with_extra_headers(&custom_provider_config::extra_headers(
        user_config,
        &base_url,
    ))?;
    let named_overrides = named
        .as_ref()
        .and_then(|value| value["extra_body"].as_object())
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
        client = client.with_request_overrides(serde_json::Map::from_iter([(
            "extra_body".into(),
            serde_json::Value::Object(extra),
        )]));
    }
    if let Some((locator, runtime)) = pool_runtime {
        client = client.with_main_pool(native_agent::MainPoolCredential::new(
            locator,
            runtime,
            fallback_base_url,
            custom_provider_config::extra_header_routes(user_config),
        )?);
    }
    Ok(Some(client))
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
    build_agent_client_for_home_with_discovery(
        config,
        user_config,
        model,
        home,
        conversation,
        Arc::new(compression_discovery::Health::default()),
    )
}

fn build_agent_client_for_home_with_discovery(
    config: &Config,
    user_config: &serde_json::Value,
    model: Option<&str>,
    home: &std::path::Path,
    conversation: Option<NativeConversationState>,
    compression_health: Arc<compression_discovery::Health>,
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
        let mut environment = |name: &str| secret_scope::get_secret(name, None).ok().flatten();
        let profiles = provider_registry::ProviderRegistry::default();
        profiles.register_bundled_base_profiles(env!("CARGO_PKG_VERSION"));
        profiles.register_upstage();
        profiles.register_nebius();
        profiles.register_vercel();
        if let Some(requested) = user_config["model"]["provider"]
            .as_str()
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
        {
            if let Some(block) = user_config["providers"].get(requested) {
                anyhow::ensure!(
                    custom_provider_config::enabled(block),
                    "provider {requested:?} is disabled in config (providers.{requested}.enabled: false)"
                );
            }
        }
        let profile = user_config
            .get("model")
            .and_then(|model| model.get("provider"))
            .and_then(serde_json::Value::as_str)
            .and_then(|name| profiles.get(name))
            .map(|profile| profile.read().unwrap().clone());

        // Explicit endpoints win, then a registered base-profile endpoint.
        // Generic configurations retain the OpenRouter default.
        let fallback_base_url = config
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
        let configured_provider = user_config["model"]["provider"]
            .as_str()
            .map(str::trim)
            .filter(|provider| !provider.is_empty());
        let provider_identity = profile
            .as_ref()
            .map(|profile| profile.name.as_str())
            .or(configured_provider)
            .unwrap_or_else(|| {
                if local_probe::urlparse_hostname(&fallback_base_url).ends_with("openrouter.ai") {
                    "openrouter"
                } else {
                    "custom"
                }
            });

        // Explicit runtime overrides stay outside durable-pool routing. For a
        // normal provider selection, the profile-scoped stored pool precedes
        // dotenv and process credentials, matching Python runtime resolution.
        let explicit_runtime = config.llm_api_key.is_some() || config.llm_base_url.is_some();
        let pool_provider = provider_identity.trim().to_lowercase();
        let pool_supported = !matches!(
            pool_provider.as_str(),
            "" | "auto" | "custom" | "anthropic" | "openai-codex" | "xai-oauth" | "nous"
        ) && !pool_provider.starts_with("custom:");
        let profile_auth = home.join("auth.json");
        let root = config_file::hermes_root();
        let root_auth = (home.parent() == Some(root.join("profiles").as_path()))
            .then(|| root.join("auth.json"));
        let main_pool_runtime = (!explicit_runtime && pool_supported)
            .then(|| {
                let locator = credential_pool::PoolLocator::new(
                    profile_auth,
                    root_auth,
                    &pool_provider,
                    credential_pool::pool_strategy(&pool_provider, user_config),
                );
                match locator.select_runtime() {
                    Ok(Some(runtime)) => Some((locator, runtime)),
                    Ok(None) => None,
                    Err(error) => {
                        tracing::debug!(
                            provider = pool_provider,
                            error = %compression_redact::redact(&error.to_string()),
                            "main credential pool unavailable"
                        );
                        None
                    }
                }
            })
            .flatten();
        let key = config
            .llm_api_key
            .clone()
            .or_else(|| {
                main_pool_runtime
                    .as_ref()
                    .map(|(_, runtime)| runtime.api_key().to_owned())
            })
            .or_else(|| match &profile {
                Some(profile) => {
                    config_file::resolve_profile_api_key(profile, &dotenv, &mut environment)
                }
                None => config_file::resolve_provider_api_key_with_env(
                    &fallback_base_url,
                    &dotenv,
                    &mut environment,
                ),
            });
        let base_url = main_pool_runtime
            .as_ref()
            .and_then(|(_, runtime)| runtime.base_url())
            .unwrap_or(&fallback_base_url)
            .to_owned();

        match (key, model) {
            (Some(key), Some(model)) => match NativeAgentClient::new(model, &key, base_url.clone())
                .and_then(|client| {
                    let limit = turn_limit::gateway(
                        user_config,
                        environment("HERMES_MAX_ITERATIONS").as_deref(),
                    )?;
                    let main_timeouts = main_provider_timeouts::Policy::resolve(
                        user_config,
                        provider_identity,
                        model,
                        |name| environment(name).or_else(|| dotenv.get(name).cloned()),
                    );
                    let client = client
                        .with_provider_identity(provider_identity)
                        .with_main_timeouts(main_timeouts)
                        .with_run_budget_seconds(&user_config["agent"]["run_budget_seconds"])
                        .with_turn_limit(limit)
                        .with_main_retry_attempts(native_agent::main_retry_attempts(
                            &user_config["agent"]["api_max_retries"],
                        ))
                        .with_empty_response_guard(&user_config["agent"]["empty_response_guard"])
                        .with_max_concurrent_children(delegation_policy::max_children(
                            user_config,
                            environment("DELEGATION_MAX_CONCURRENT_CHILDREN").as_deref(),
                        ));
                    let client = match &profile {
                        Some(profile) => client.with_provider_profile(profile)?,
                        None => client,
                    };
                    let client = if provider_identity.eq_ignore_ascii_case("openrouter") {
                        client.with_provider_default_headers(
                            serde_json::json!({
                                "HTTP-Referer":"https://hermes-agent.nousresearch.com",
                                "X-Title":"Hermes Agent",
                                "X-OpenRouter-Categories":"productivity,cli-agent",
                            })
                            .as_object()
                            .unwrap(),
                        )?
                    } else {
                        client
                    };
                    client.with_extra_headers(&custom_provider_config::extra_headers(
                        user_config,
                        &base_url,
                    ))
                }) {
                Ok(mut c) => {
                    if let Some((locator, runtime)) = main_pool_runtime.clone() {
                        c = c.with_main_pool(native_agent::MainPoolCredential::new(
                            locator,
                            runtime,
                            fallback_base_url.clone(),
                            custom_provider_config::extra_header_routes(user_config),
                        )?);
                    }
                    let compression_policy = compression_auxiliary::Config::from_value(user_config);
                    let compression_auto = compression_policy.provider == "auto";
                    c = c.with_summary_request_policy(compression_policy.timeout, None);
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
                    let primary_identity = c.backend_identity();
                    let mut main_fallbacks = Vec::new();
                    for (index, entry) in compression_auxiliary::main_fallback_chain(user_config)
                        .iter()
                        .enumerate()
                    {
                        match build_native_main_fallback_client(
                            entry,
                            user_config,
                            &profiles,
                            &dotenv,
                            &mut environment,
                            home,
                        ) {
                            Ok(Some(fallback))
                                if compression_auxiliary::should_skip_candidate(
                                    &fallback.backend_identity(),
                                    &primary_identity,
                                    compression_auxiliary::FailureScope::Model,
                                ) =>
                            {
                                tracing::debug!(
                                    route_index = index,
                                    provider = entry.provider,
                                    "main fallback resolves to the primary backend; skipping"
                                );
                            }
                            Ok(Some(fallback)) => main_fallbacks.push(fallback),
                            Ok(None) => tracing::debug!(
                                route_index = index,
                                provider = entry.provider,
                                "main fallback requires an unsupported native transport; skipping"
                            ),
                            Err(error) => {
                                let error = compression_redact::redact(&error.to_string());
                                tracing::warn!(
                                    %error,
                                    route_index = index,
                                    provider = entry.provider,
                                    "main fallback route unavailable; trying the next entry"
                                );
                            }
                        }
                    }
                    if !main_fallbacks.is_empty() {
                        c = c.with_main_fallback_routes(main_fallbacks);
                    }
                    let mut primary_compression = None;
                    let mut primary_compression_unavailable = false;
                    let mut compression_fallbacks = Vec::new();
                    let mut main_compression_fallbacks = Vec::new();
                    let mut compression_discovery = None;
                    if compression_policy.needs_separate_client {
                        match build_native_compression_client(
                            CompressionRouteConfig::Primary(&compression_policy),
                            user_config,
                            model,
                            &key,
                            &base_url,
                            profile.as_ref(),
                            &profiles,
                            &dotenv,
                            &mut environment,
                            home,
                        ) {
                            Ok(Some(auxiliary)) => primary_compression = Some(auxiliary),
                            Ok(None) => primary_compression_unavailable = true,
                            Err(error) => {
                                primary_compression_unavailable = true;
                                let error = compression_redact::redact(&error.to_string());
                                tracing::warn!(
                                    %error,
                                    "primary auxiliary compression route unavailable; continuing to configured fallbacks"
                                );
                            }
                        }
                    }
                    for (index, entry) in compression_policy.fallback_chain.iter().enumerate() {
                        match build_native_compression_client(
                            CompressionRouteConfig::TaskFallback {
                                entry,
                                task: &compression_policy,
                            },
                            user_config,
                            model,
                            &key,
                            &base_url,
                            profile.as_ref(),
                            &profiles,
                            &dotenv,
                            &mut environment,
                            home,
                        ) {
                            Ok(Some(fallback)) => compression_fallbacks.push(fallback),
                            Ok(None) => tracing::debug!(
                                route_index = index,
                                "compression fallback entry has no resolvable model; skipping"
                            ),
                            Err(error) => {
                                let error = compression_redact::redact(&error.to_string());
                                tracing::warn!(
                                    %error,
                                    route_index = index,
                                    provider = entry.provider,
                                    "compression fallback route unavailable; trying the next entry"
                                );
                            }
                        }
                    }
                    if compression_auto {
                        for (index, entry) in
                            compression_auxiliary::main_fallback_chain(user_config)
                                .iter()
                                .enumerate()
                        {
                            if compression_auxiliary::should_skip_main_fallback_provider(
                                &entry.provider,
                                requested_provider,
                                requested_provider,
                            ) {
                                continue;
                            }
                            match build_native_compression_client(
                                CompressionRouteConfig::MainFallback {
                                    entry,
                                    task: &compression_policy,
                                },
                                user_config,
                                model,
                                &key,
                                &base_url,
                                profile.as_ref(),
                                &profiles,
                                &dotenv,
                                &mut environment,
                                home,
                            ) {
                                Ok(Some(fallback)) => main_compression_fallbacks.push(fallback),
                                Ok(None) => tracing::debug!(
                                    route_index = index,
                                    "main compression fallback has no resolvable model; skipping"
                                ),
                                Err(error) => {
                                    let error = compression_redact::redact(&error.to_string());
                                    tracing::warn!(
                                        %error,
                                        route_index = index,
                                        provider = entry.provider,
                                        "main compression fallback unavailable; trying the next entry"
                                    );
                                }
                            }
                        }
                        let candidates = build_native_compression_discovery(
                            &compression_policy,
                            user_config,
                            model,
                            &key,
                            &base_url,
                            profile.as_ref(),
                            &profiles,
                            &dotenv,
                            &mut environment,
                            home,
                        );
                        compression_discovery =
                            native_agent::CompressionDiscovery::new_with_pool_credentials(
                                home,
                                candidates,
                                compression_health.clone(),
                            );
                    }
                    if primary_compression.is_some()
                        || !compression_fallbacks.is_empty()
                        || !main_compression_fallbacks.is_empty()
                        || compression_discovery.is_some()
                    {
                        let unavailable_primary = primary_compression_unavailable.then(|| {
                            compression_auxiliary::BackendIdentity::new(
                                &compression_policy.provider,
                                compression_policy.model.as_deref().unwrap_or(model),
                                compression_policy.base_url.as_deref().unwrap_or(""),
                            )
                        });
                        c = c.with_compression_routes(
                            primary_compression,
                            compression_fallbacks,
                            main_compression_fallbacks,
                            compression_discovery,
                            compression_auto,
                            unavailable_primary,
                        );
                    }
                    c = c.with_automatic_compression_policy(
                        automatic_compression::AutomaticCompressionPolicy::from_value(user_config),
                    );
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
                            .with_hooks(state.hooks, state.platform)
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

struct ConversationRuntime<'a> {
    database: Option<&'a session_db::SessionDb>,
    process_registry: Arc<background_process::Registry>,
    tool_approvals: Arc<tool_approval::ApprovalBroker>,
    compression_health: Arc<compression_discovery::Health>,
}

async fn build_conversation_client(
    config: &Config,
    initializer: &conversation_prompt::Initializer,
    home: &std::path::Path,
    message: &hermes_core::Message,
    history: &[session_db::HistoryMessage],
    runtime: ConversationRuntime<'_>,
) -> anyhow::Result<Arc<dyn AgentClient>> {
    let ConversationRuntime {
        database,
        process_registry,
        tool_approvals,
        compression_health,
    } = runtime;
    let profile_secrets = secret_scope::current_secret_scope().as_deref().cloned();
    anyhow::ensure!(
        !secret_scope::is_multiplex_active() || profile_secrets.is_some(),
        "native profile construction requires a secret scope"
    );
    let selected = config_file::load_config_from(&home.join("config.yaml"));
    let profile_env = profile_secrets
        .clone()
        .unwrap_or_else(|| config_file::load_dotenv(&home.join(".env")));
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
    let mut registered_base_tools = registered_native_tools();
    if native_local_terminal_eligible(&selected, &platform) {
        let route = database
            .and_then(|database| database.gateway_route_for_session(&session_id).ok())
            .flatten();
        let terminal_database = database.and_then(|database| {
            session_db::SessionDb::open_shared(database.database_path().to_path_buf())
                .map_err(|error| {
                    tracing::warn!(%error, %session_id, "Native terminal DB handle unavailable");
                    error
                })
                .ok()
        });
        let default_timeout = selected["terminal"]["timeout"].as_u64().unwrap_or(180);
        registered_base_tools.push(Arc::new(crate::native_terminal::TerminalTool::new(
            crate::native_terminal::TerminalConfig {
                cwd: runtime_cwd.clone().into(),
                profile_env: &profile_env,
                profile_home: home,
                session_identity: &gateway_session_key,
                default_timeout,
                database: terminal_database,
                route,
                process_registry: process_registry.clone(),
                approval_config: selected.clone(),
                tool_approvals,
                approval_prompt_capable: matches!(
                    platform.as_str(),
                    "telegram" | "discord" | "slack"
                ),
            },
        )));
        registered_base_tools.push(Arc::new(crate::native_process::ProcessTool::new(
            process_registry,
            crate::background_process::Owner::new(home, &gateway_session_key),
            default_timeout,
        )));
    }
    let base_fresh_tools = if config.agent_tools {
        registered_base_tools.clone()
    } else {
        Vec::new()
    };
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
            gateway_session_key: Some(gateway_session_key.clone()),
            native_tool_names: native_tools::tool_names(&registered_base_tools),
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
    let registered_tools = merge_native_tools(registered_base_tools, registered_extension_tools);
    let base_fresh_tool_names = native_tools::tool_names(&base_fresh_tools);
    let mut fresh_tools = merge_native_tools(base_fresh_tools.clone(), available_extension_tools);
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
        fresh_tools = base_fresh_tools.clone();
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
    let mut hooks = hooks::HookRegistry::new().with_runtime(hooks::HookRuntime {
        profile_home: home.to_path_buf(),
        profile_env,
        python: config.agent_python.clone(),
        repo_root: config.agent_cwd.clone(),
    });
    hooks.discover_and_load_from(&home.join("hooks"));
    let hooks = if hooks.loaded_hooks().is_empty() {
        None
    } else {
        tracing::info!(count = hooks.loaded_hooks().len(), %session_id, "Native lifecycle hooks activated");
        Some(Arc::new(hooks))
    };
    build_agent_client_for_home_with_discovery(
        config,
        &selected,
        Some(&model),
        home,
        Some(NativeConversationState {
            system_prompt: resolution.prompt,
            tools,
            plugin_prompt,
            extension_host: extension.map(|(client, _)| client),
            hooks,
            platform,
            context_length,
        }),
        compression_health,
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
    .with_slash_confirmations(state.slash_confirmations.clone())
    .with_tool_approvals(state.tool_approvals.clone());
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
    let background_processes = Arc::new(background_process::Registry::new());
    let tool_approvals = Arc::new(tool_approval::ApprovalBroker::new());
    let compression_health = Arc::new(compression_discovery::Health::default());

    // Choose the agent backend. Native (in-Rust LLM) is opt-in and needs a key +
    // a model; otherwise fall back to the Python subprocess bridge (default).
    let agent = build_agent_client(&config, &user_config, configured_model.as_deref());
    let mut conversation_cache = None;
    let agent: Arc<dyn AgentClient> =
        if config.agent_native && config.agent_cli.is_none() && !agent.manages_history() {
            let captured = config.clone();
            let captured_processes = background_processes.clone();
            let captured_approvals = tool_approvals.clone();
            let captured_compression_health = compression_health.clone();
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
                    let process_registry = captured_processes.clone();
                    let approvals = captured_approvals.clone();
                    let compression_health = captured_compression_health.clone();
                    Box::pin(async move {
                        build_conversation_client(
                            &captured,
                            &prompt_initializer,
                            &home,
                            &message,
                            &history,
                            ConversationRuntime {
                                database,
                                process_registry,
                                tool_approvals: approvals,
                                compression_health,
                            },
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
    state.tool_approvals = tool_approvals;
    if !state.agent.manages_history() {
        let home = config_file::hermes_home();
        let root = config_file::hermes_root();
        let freshness = session_reset::configured_freshness_seconds(
            &state.user_config,
            std::env::var("HERMES_AUTO_CONTINUE_FRESHNESS")
                .ok()
                .as_deref(),
        );
        let process_probe = background_processes.clone();
        let store = tokio::task::spawn_blocking(move || {
            let profile = profile_name::active_profile_name(&home, &root)
                .unwrap_or_else(|_| "default".into());
            let gateway_config = config_loader::load_gateway_config_from(&home);
            let max_age = session_reset::process_age_limit(&gateway_config.default_reset_policy)?
                .map(std::time::Duration::try_from_secs_f64)
                .transpose()
                .map_err(|error| anyhow::anyhow!("invalid background process age: {error}"))?;
            let process_home = home.clone();
            session_store::SessionStore::open(gateway_config, root, home, profile, move |key| {
                Ok(process_probe.has_active_for_session(&process_home, key, max_age))
            })
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
    let cancelled_approvals = state.tool_approvals.shutdown();
    if cancelled_approvals > 0 {
        tracing::info!(
            cancelled_approvals,
            "cancelled pending tool approvals on shutdown"
        );
    }
    if let Some(cache) = conversation_cache {
        cache.shutdown(std::time::Duration::from_secs(45)).await;
    }
    background_processes.shutdown().await;
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
    fn fallback_fast_lane_controls_require_candidate_certification() {
        let task = compression_auxiliary::Config::from_value(&serde_json::json!({
            "auxiliary":{"compression":{
                "provider":"custom", "model":"primary-model",
                "reasoning_effort":false, "max_output_tokens":777,
                "extra_body":{"reasoning":{"enabled":false}, "route_marker":"task"},
                "fallback_chain":[
                    {"provider":"openrouter", "model":"fallback-model"}
                ]
            }}
        }));
        let route = CompressionRouteConfig::TaskFallback {
            entry: &task.fallback_chain[0],
            task: &task,
        };

        assert!(route
            .reasoning_config("openrouter", "fallback-model")
            .is_none());
        assert_eq!(
            route.request_extra_body("openrouter", "fallback-model"),
            serde_json::Map::from_iter([("route_marker".into(), serde_json::json!("task"))])
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_terminal_requires_supported_local_approval_surface() {
        assert!(native_local_terminal_eligible(
            &json!({
                "terminal":{"backend":"local"},
                "approvals":{"mode":"off","deny":[]}
            }),
            "cli"
        ));
        assert!(native_local_terminal_eligible(
            &json!({
                "terminal":{"backend":"local"},
                "approvals":{"mode":" OFF ","deny":["git push*"]}
            }),
            "telegram"
        ));
        assert!(native_local_terminal_eligible(
            &json!({
                "terminal":{"backend":"local"},
                "approvals":{"mode":false,"deny":null}
            }),
            "cli"
        ));
        assert!(native_local_terminal_eligible(
            &json!({
                "terminal":{"backend":"local"},
                "approvals":{"mode":"manual","deny":[]},
                "security":{"tirith_enabled":false}
            }),
            "telegram"
        ));
        assert!(native_local_terminal_eligible(
            &json!({
                "terminal":{"backend":"local"},
                "approvals":{"mode":"manual","deny":[]},
                "security":{"tirith_enabled":false}
            }),
            "discord"
        ));
        assert!(native_local_terminal_eligible(
            &json!({
                "terminal":{"backend":"local"},
                "approvals":{"mode":true,"deny":[]},
                "security":{"tirith_enabled":false}
            }),
            "slack"
        ));
        for (config, platform) in [
            (json!({}), "telegram"),
            (json!({"security":{"tirith_enabled":false}}), "telegram"),
            (
                json!({"approvals":{"mode":"manual"},"security":{"tirith_enabled":false}}),
                "cli",
            ),
            (json!({"approvals":{"mode":"manual"}}), "telegram"),
            (
                json!({"terminal":{"backend":"docker"},"approvals":{"mode":"off"}}),
                "telegram",
            ),
            (json!({"approvals":{"mode":"smart"}}), "telegram"),
            (
                json!({"approvals":{"mode":"off","deny":"invalid"}}),
                "telegram",
            ),
        ] {
            assert!(
                !native_local_terminal_eligible(&config, platform),
                "{config}"
            );
        }
    }

    #[test]
    fn native_tool_names_cannot_be_replaced_by_extension_collisions() {
        let merged = merge_native_tools(registered_native_tools(), registered_native_tools());
        assert_eq!(native_tools::tool_names(&merged), ["current_time"]);
    }

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
    async fn native_agent_applies_configured_main_retry_budget() {
        use axum::{
            extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn serve(app: Router) -> (String, Server) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let primary_calls = Arc::new(AtomicUsize::new(0));
        let primary = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({"error":{"message":"upstream failed"}})),
                    )
                        .into_response()
                }),
            )
            .with_state(primary_calls.clone());
        let fallback = Router::new().route(
            "/chat/completions",
            post(|| async {
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"configured\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                )
            }),
        );
        let (primary_url, _primary_server) = serve(primary).await;
        let (fallback_url, _fallback_server) = serve(fallback).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = TempHome(std::env::temp_dir().join(format!(
            "hermes-main-retry-config-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&home.0).unwrap();
        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(primary_url);
        let user_config = json!({
            "agent":{"api_max_retries":1},
            "model":{"provider":"openrouter"},
            "fallback_providers":[{
                "provider":"custom", "model":"fallback-model",
                "base_url":fallback_url, "api_key":"fallback-key"
            }]
        });
        let agent = build_agent_client_for_home(
            &config,
            &user_config,
            Some("primary-model"),
            &home.0,
            Some(NativeConversationState {
                system_prompt: "stable\n\nModel: primary-model\nProvider: openrouter".into(),
                tools: Vec::new(),
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        agent.run_turn(&message, &[], sender).await.unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }

        assert_eq!(reply, "configured");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_agent_applies_model_request_timeout_before_main_fallback() {
        use axum::{body::Body, extract::State, response::Response, routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn serve(app: Router) -> (String, Server) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let primary_calls = Arc::new(AtomicUsize::new(0));
        let primary = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"late primary\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        ))
                        .unwrap()
                }),
            )
            .with_state(primary_calls.clone());
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let fallback = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"fallback after timeout\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        ))
                        .unwrap()
                }),
            )
            .with_state(fallback_calls.clone());
        let (primary_url, _primary_server) = serve(primary).await;
        let (fallback_url, _fallback_server) = serve(fallback).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = TempHome(std::env::temp_dir().join(format!(
            "hermes-main-request-timeout-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&home.0).unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(primary_url);
        let user_config = json!({
            "agent":{"api_max_retries":1},
            "model":{"provider":"openrouter"},
            "providers":{"openrouter":{"models":{
                "primary-model":{"timeout_seconds":0.05}
            }}},
            "fallback_providers":[{
                "provider":"custom", "model":"fallback-model",
                "base_url":fallback_url, "api_key":"fallback-key"
            }]
        });
        let agent = build_agent_client_for_home(
            &config,
            &user_config,
            Some("primary-model"),
            &home.0,
            Some(NativeConversationState {
                system_prompt: "stable\n\nModel: primary-model\nProvider: openrouter".into(),
                tools: Vec::new(),
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            agent.run_turn(&message, &[], sender),
        )
        .await
        .expect("configured request timeout must end the hung primary")
        .unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }

        assert_eq!(reply, "fallback after timeout");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 3);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_agent_applies_buffered_stale_timeout_while_reading_response_body() {
        use axum::{body::Body, extract::State, response::Response, routing::post, Json, Router};
        use futures_util::StreamExt;
        use std::convert::Infallible;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn serve(app: Router) -> (String, Server) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let primary_calls = Arc::new(AtomicUsize::new(0));
        let primary = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from_stream(
                            futures_util::stream::once(async {
                                Ok::<_, Infallible>(axum::body::Bytes::from_static(
                                    b"{\"choices\":[",
                                ))
                            })
                            .chain(futures_util::stream::pending()),
                        ))
                        .unwrap()
                }),
            )
            .with_state(primary_calls.clone());
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let fallback = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"choices":[{"message":{
                        "role":"assistant", "content":"fallback after buffered stall"
                    }}]}))
                }),
            )
            .with_state(fallback_calls.clone());
        let (primary_url, _primary_server) = serve(primary).await;
        let (fallback_url, _fallback_server) = serve(fallback).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = TempHome(std::env::temp_dir().join(format!(
            "hermes-main-buffered-stall-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&home.0).unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(primary_url);
        config.agent_tools = true;
        let user_config = json!({
            "agent":{"api_max_retries":1},
            "model":{"provider":"openrouter"},
            "providers":{"openrouter":{"models":{
                "primary-model":{"timeout_seconds":2, "stale_timeout_seconds":0.05}
            }}},
            "fallback_providers":[{
                "provider":"custom", "model":"fallback-model",
                "base_url":fallback_url, "api_key":"fallback-key"
            }]
        });
        let agent = build_agent_client_for_home(
            &config,
            &user_config,
            Some("primary-model"),
            &home.0,
            Some(NativeConversationState {
                system_prompt: "stable\n\nModel: primary-model\nProvider: openrouter".into(),
                tools: vec![Arc::new(native_tools::CurrentTimeTool)],
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);

        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            agent.run_turn(&message, &[], sender),
        )
        .await
        .expect("buffered stale timeout must cover response-body reads")
        .unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }

        assert_eq!(reply, "fallback after buffered stall");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn configured_run_budget_caps_the_native_buffered_stale_deadline() {
        use axum::{body::Body, extract::State, response::Response, routing::post, Json, Router};
        use std::convert::Infallible;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn serve(app: Router) -> (String, Server) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let primary_calls = Arc::new(AtomicUsize::new(0));
        let primary = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from_stream(futures_util::stream::pending::<
                            std::result::Result<axum::body::Bytes, Infallible>,
                        >()))
                        .unwrap()
                }),
            )
            .with_state(primary_calls.clone());
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let fallback = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"choices":[{"message":{
                        "role":"assistant", "content":"fallback after budget cap"
                    }}]}))
                }),
            )
            .with_state(fallback_calls.clone());
        let (primary_url, _primary_server) = serve(primary).await;
        let (fallback_url, _fallback_server) = serve(fallback).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = TempHome(std::env::temp_dir().join(format!(
            "hermes-main-run-budget-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&home.0).unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(primary_url);
        config.agent_tools = true;
        let user_config = json!({
            "agent":{"api_max_retries":1, "run_budget_seconds":120},
            "model":{"provider":"openrouter"},
            "fallback_providers":[{
                "provider":"custom", "model":"fallback-model",
                "base_url":fallback_url, "api_key":"fallback-key"
            }]
        });
        let agent = build_agent_client_for_home(
            &config,
            &user_config,
            Some("deepseek/deepseek-r1"),
            &home.0,
            Some(NativeConversationState {
                system_prompt: "stable\n\nModel: deepseek/deepseek-r1\nProvider: openrouter".into(),
                tools: vec![Arc::new(native_tools::CurrentTimeTool)],
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        tokio::time::timeout(
            std::time::Duration::from_secs(100),
            agent.run_turn(&message, &[], sender),
        )
        .await
        .expect("configured run budget must cap the 600-second reasoning floor")
        .unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }

        assert_eq!(reply, "fallback after budget cap");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_agent_retries_configured_previsible_stream_stall_then_falls_back() {
        use axum::{body::Body, extract::State, response::Response, routing::post, Router};
        use std::convert::Infallible;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn serve(app: Router) -> (String, Server) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let primary_calls = Arc::new(AtomicUsize::new(0));
        let primary = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(futures_util::stream::pending::<
                            Result<axum::body::Bytes, Infallible>,
                        >()))
                        .unwrap()
                }),
            )
            .with_state(primary_calls.clone());
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let fallback = Router::new()
            .route(
                "/chat/completions",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"fallback after stall\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        ))
                        .unwrap()
                }),
            )
            .with_state(fallback_calls.clone());
        let (primary_url, _primary_server) = serve(primary).await;
        let (fallback_url, _fallback_server) = serve(fallback).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = TempHome(std::env::temp_dir().join(format!(
            "hermes-main-stream-stall-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&home.0).unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(primary_url);
        let user_config = json!({
            "agent":{"api_max_retries":1},
            "model":{"provider":"openrouter"},
            "providers":{"openrouter":{"models":{
                "primary-model":{"stale_timeout_seconds":0.05}
            }}},
            "fallback_providers":[{
                "provider":"custom", "model":"fallback-model",
                "base_url":fallback_url, "api_key":"fallback-key"
            }]
        });
        let agent = build_agent_client_for_home(
            &config,
            &user_config,
            Some("primary-model"),
            &home.0,
            Some(NativeConversationState {
                system_prompt: "stable\n\nModel: primary-model\nProvider: openrouter".into(),
                tools: Vec::new(),
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            agent.run_turn(&message, &[], sender),
        )
        .await
        .expect("configured stale deadline must end each silent stream")
        .unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }

        assert_eq!(reply, "fallback after stall");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 3);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_main_provider_fallback_preserves_static_prompt_prefix() {
        use axum::{
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::post,
            Json, Router,
        };

        type Captures = Arc<std::sync::Mutex<Vec<(HeaderMap, Value)>>>;
        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        async fn primary(captures: Captures) -> (String, Server) {
            let app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let captures = captures.clone();
                    async move {
                        captures.lock().unwrap().push((headers, body));
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({"error":{"message":"rate limit"}})),
                        )
                            .into_response()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        async fn fallback(captures: Captures) -> (String, Server) {
            let app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let captures = captures.clone();
                    async move {
                        captures.lock().unwrap().push((headers, body));
                        (
                            [("content-type", "text/event-stream")],
                            "data: {\"choices\":[{\"delta\":{\"content\":\"rescued\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let primary_requests: Captures = Default::default();
        let fallback_requests: Captures = Default::default();
        let (primary_url, _primary_server) = primary(primary_requests.clone()).await;
        let (fallback_url, _fallback_server) = fallback(fallback_requests.clone()).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home =
            TempHome(std::env::temp_dir().join(format!("hermes-main-provider-fallback-{nonce}")));
        std::fs::create_dir_all(&home.0).unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(primary_url);
        let user_config = json!({
            "model":{"provider":"openrouter"},
            "fallback_providers":[{
                "provider":"custom",
                "model":"fallback-model",
                "base_url":fallback_url,
                "api_key":"fallback-key"
            }]
        });
        let primary_prompt = "stable cached prefix\n\nModel: primary-model\nProvider: openrouter";
        let agent = build_agent_client_for_home(
            &config,
            &user_config,
            Some("primary-model"),
            &home.0,
            Some(NativeConversationState {
                system_prompt: primary_prompt.into(),
                tools: Vec::new(),
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        message.resolved_session_id = Some("fallback-session".into());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        agent.run_turn(&message, &[], sender).await.unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }
        assert_eq!(reply, "rescued");

        let preflight = agent
            .compression_preflight(agent::TurnContext::default(), &message, &[])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preflight.model, "fallback-model");

        message.text = "still there?".into();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        agent.run_turn(&message, &[], sender).await.unwrap();
        let mut second_reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                second_reply.push_str(&text);
            }
        }
        assert_eq!(second_reply, "rescued");

        let primary_requests = primary_requests.lock().unwrap();
        let fallback_requests = fallback_requests.lock().unwrap();
        assert_eq!(primary_requests.len(), 1);
        assert_eq!(fallback_requests.len(), 2);
        assert_eq!(primary_requests[0].0["authorization"], "Bearer primary-key");
        assert_eq!(
            fallback_requests[0].0["authorization"],
            "Bearer fallback-key"
        );
        assert_eq!(primary_requests[0].1["model"], "primary-model");
        assert_eq!(fallback_requests[0].1["model"], "fallback-model");
        assert_eq!(
            primary_requests[0].1["messages"][0]["content"],
            primary_prompt
        );
        assert_eq!(
            fallback_requests[0].1["messages"][0]["content"],
            "stable cached prefix\n\nModel: fallback-model\nProvider: custom"
        );
    }

    #[tokio::test]
    async fn native_main_provider_fallback_sticks_across_tool_rounds() {
        use axum::{
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::post,
            Json, Router,
        };

        type Captures = Arc<std::sync::Mutex<Vec<(HeaderMap, Value)>>>;
        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        async fn serve_primary(captures: Captures) -> (String, Server) {
            let app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let captures = captures.clone();
                    async move {
                        let index = {
                            let mut requests = captures.lock().unwrap();
                            let index = requests.len();
                            requests.push((headers, body));
                            index
                        };
                        if index == 0 {
                            Json(json!({"choices":[{"message":{
                                "role":"assistant",
                                "content":null,
                                "tool_calls":[{"id":"primary-time","type":"function","function":{
                                    "name":"current_time", "arguments":"{}"
                                }}]
                            }}]}))
                            .into_response()
                        } else {
                            (
                                StatusCode::TOO_MANY_REQUESTS,
                                Json(json!({"error":{"message":"rate limit"}})),
                            )
                                .into_response()
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }
        async fn serve_fallback(captures: Captures) -> (String, Server) {
            let app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let captures = captures.clone();
                    async move {
                        let index = {
                            let mut requests = captures.lock().unwrap();
                            let index = requests.len();
                            requests.push((headers, body));
                            index
                        };
                        if index == 0 {
                            Json(json!({"choices":[{"message":{
                                "role":"assistant",
                                "content":null,
                                "tool_calls":[{"id":"fallback-time","type":"function","function":{
                                    "name":"current_time", "arguments":"{}"
                                }}]
                            }}]}))
                        } else {
                            Json(json!({"choices":[{"message":{
                                "role":"assistant", "content":"tool fallback complete"
                            }}]}))
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let primary_requests: Captures = Default::default();
        let fallback_requests: Captures = Default::default();
        let (primary_url, _primary_server) = serve_primary(primary_requests.clone()).await;
        let (fallback_url, _fallback_server) = serve_fallback(fallback_requests.clone()).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("hermes-tool-provider-fallback-{nonce}"));
        std::fs::create_dir_all(&home).unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(primary_url);
        config.agent_tools = true;
        let selected = json!({
            "model":{"provider":"openrouter"},
            "fallback_model":{
                "provider":"custom",
                "model":"fallback-model",
                "base_url":fallback_url,
                "api_key":"fallback-key"
            }
        });
        let client = build_agent_client_for_home(
            &config,
            &selected,
            Some("primary-model"),
            &home,
            Some(NativeConversationState {
                system_prompt: "stable prefix\n\nModel: primary-model\nProvider: openrouter".into(),
                tools: vec![Arc::new(native_tools::CurrentTimeTool)],
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"time"
        }))
        .unwrap();
        message.resolved_session_id = Some("tool-fallback-session".into());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        client.run_turn(&message, &[], sender).await.unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }
        assert_eq!(reply, "tool fallback complete");

        let primary_requests = primary_requests.lock().unwrap();
        let fallback_requests = fallback_requests.lock().unwrap();
        assert_eq!(primary_requests.len(), 2);
        assert_eq!(fallback_requests.len(), 2);
        assert_eq!(
            fallback_requests[0].0["authorization"],
            "Bearer fallback-key"
        );
        assert_eq!(fallback_requests[0].1["model"], "fallback-model");
        assert_eq!(
            fallback_requests[0].1["tools"],
            primary_requests[1].1["tools"]
        );
        assert!(fallback_requests[0].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["tool_call_id"] == "primary-time"));
        assert!(fallback_requests[1].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["tool_call_id"] == "fallback-time"));
        assert_eq!(
            fallback_requests[0].1["messages"][0]["content"],
            "stable prefix\n\nModel: fallback-model\nProvider: custom"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn native_auth_fallback_restores_primary_on_the_next_turn() {
        use axum::{
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::post,
            Json, Router,
        };

        type Captures = Arc<std::sync::Mutex<Vec<(HeaderMap, Value)>>>;
        let requests: Captures = Default::default();
        let captured = requests.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let captured = captured.clone();
                async move {
                    let authorization = headers["authorization"].clone();
                    let primary_attempt = {
                        let mut requests = captured.lock().unwrap();
                        let primary_attempt = requests
                            .iter()
                            .filter(|(headers, _)| {
                                headers["authorization"] == "Bearer primary-key"
                            })
                            .count();
                        requests.push((headers, body));
                        primary_attempt
                    };
                    if authorization == "Bearer primary-key" && primary_attempt == 0 {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error":{"message":"invalid key"}})),
                        )
                            .into_response()
                    } else {
                        let text = if authorization == "Bearer primary-key" {
                            "primary restored"
                        } else {
                            "fallback response"
                        };
                        (
                            [("content-type", "text/event-stream")],
                            format!(
                                "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
                                serde_json::to_string(text).unwrap()
                            ),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("hermes-auth-fallback-{nonce}"));
        std::fs::create_dir_all(&home).unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("primary-key".into());
        config.llm_base_url = Some(base_url.clone());
        let selected = json!({
            "model":{"provider":"openrouter"},
            "fallback_providers":[{
                "provider":"custom",
                "model":"fallback-model",
                "base_url":base_url,
                "api_key":"fallback-key"
            }]
        });
        let client = build_agent_client_for_home(
            &config,
            &selected,
            Some("primary-model"),
            &home,
            Some(NativeConversationState {
                system_prompt: "stable prefix\n\nModel: primary-model\nProvider: openrouter".into(),
                tools: Vec::new(),
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"first"
        }))
        .unwrap();
        message.resolved_session_id = Some("auth-fallback-session".into());

        for (text, expected) in [
            ("first", "fallback response"),
            ("second", "primary restored"),
        ] {
            message.text = text.into();
            let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
            client.run_turn(&message, &[], sender).await.unwrap();
            let mut reply = String::new();
            while let Some(event) = receiver.recv().await {
                if let hermes_core::StreamEvent::MessageChunk { text } = event {
                    reply.push_str(&text);
                }
            }
            assert_eq!(reply, expected);
        }

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0["authorization"], "Bearer primary-key");
        assert_eq!(requests[1].0["authorization"], "Bearer fallback-key");
        assert_eq!(requests[2].0["authorization"], "Bearer primary-key");
        assert_eq!(requests[2].1["model"], "primary-model");
        assert_eq!(
            requests[2].1["messages"][0]["content"],
            "stable prefix\n\nModel: primary-model\nProvider: openrouter"
        );
        server.abort();
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn compression_routes_preserve_explicit_and_auto_fallback_order() {
        use axum::{http::HeaderMap, routing::post, Json, Router};

        type Captures = Arc<std::sync::Mutex<Vec<(HeaderMap, Value)>>>;
        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn serve(captures: Captures, responses: Vec<Value>) -> (String, Server) {
            let responses = Arc::new(responses);
            let app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let captures = captures.clone();
                    let responses = responses.clone();
                    async move {
                        let index = {
                            let mut captures = captures.lock().unwrap();
                            let index = captures.len();
                            captures.push((headers, body));
                            index
                        };
                        let response = responses
                            .get(index)
                            .or_else(|| responses.last())
                            .expect("test response")
                            .clone();
                        Json(response)
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let auxiliary_requests: Captures = Default::default();
        let fallback_requests: Captures = Default::default();
        let main_fallback_requests: Captures = Default::default();
        let main_requests: Captures = Default::default();
        let discovery_requests: Captures = Default::default();
        let (auxiliary_url, _auxiliary_server) = serve(
            auxiliary_requests.clone(),
            vec![
                json!({
                    "choices":[{"finish_reason":"length","message":{"content":"partial"}}],
                    "usage":{"prompt_tokens":21,"completion_tokens":7}
                }),
                json!({
                    "choices":[{"finish_reason":"stop","message":{"content":"auxiliary summary"}}],
                    "usage":{"prompt_tokens":22,"completion_tokens":9}
                }),
            ],
        )
        .await;
        let (fallback_url, _fallback_server) = serve(
            fallback_requests.clone(),
            vec![json!({
                "choices":[{"finish_reason":"stop","message":{"content":"chain summary"}}],
                "usage":{"prompt_tokens":29,"completion_tokens":6}
            })],
        )
        .await;
        let (main_fallback_url, _main_fallback_server) = serve(
            main_fallback_requests.clone(),
            vec![json!({
                "choices":[{"finish_reason":"stop","message":{"content":"main-chain summary"}}],
                "usage":{"prompt_tokens":31,"completion_tokens":7}
            })],
        )
        .await;
        let (main_url, _main_server) = serve(
            main_requests.clone(),
            vec![
                json!({
                    "choices":[{"finish_reason":"length","message":{"content":"partial"}}],
                    "usage":{"prompt_tokens":34,"completion_tokens":8}
                }),
                json!({
                    "choices":[{"finish_reason":"length","message":{"content":"partial again"}}],
                    "usage":{"prompt_tokens":35,"completion_tokens":8}
                }),
                json!({
                    "choices":[{"finish_reason":"length","message":{"content":"discovery handoff"}}],
                    "usage":{"prompt_tokens":36,"completion_tokens":8}
                }),
            ],
        )
        .await;
        let (discovery_url, _discovery_server) = serve(
            discovery_requests.clone(),
            vec![json!({
                "choices":[{"finish_reason":"stop","message":{"content":"discovered summary"}}],
                "usage":{"prompt_tokens":41,"completion_tokens":5}
            })],
        )
        .await;

        let mut config = native_config();
        config.llm_base_url = Some(main_url);
        config.llm_api_key = Some("main-key".into());
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home =
            TempHome(std::env::temp_dir().join(format!("hermes-compression-aux-route-{nonce}")));
        std::fs::create_dir_all(&home.0).unwrap();
        std::fs::write(
            home.0.join(".env"),
            format!(
                "AUX_COMPRESSION_KEY=aux-key\nGMI_API_KEY=gmi-key\nGMI_BASE_URL={discovery_url}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            home.0.join("auth.json"),
            serde_json::to_vec(&json!({
                "credential_pool":{"gmi":[{
                    "id":"gmi-pool",
                    "auth_type":"api_key",
                    "source":"manual",
                    "priority":0,
                    "access_token":"gmi-pool-key",
                    "base_url":discovery_url,
                }]}
            }))
            .unwrap(),
        )
        .unwrap();
        let user_config = json!({
            "model":{"provider":"custom"},
            "auxiliary":{"compression":{
                "provider":"openrouter",
                "model":"aux-model",
                "base_url":auxiliary_url,
                "key_env":"AUX_COMPRESSION_KEY",
                "reasoning_effort":false,
                "max_output_tokens":777,
                "extra_body":{"route_marker":"auxiliary-only"},
                "fallback_chain":[
                    {"model":"missing-provider"},
                    {"provider":"openrouter"},
                    {"provider":"openrouter", "model":"fallback-model",
                     "base_url":fallback_url.clone(), "api_key":"fallback-key",
                     "timeout":45, "reasoning_effort":false,
                     "max_output_tokens":333}
                ]
            }}
        });
        let agent =
            build_agent_client_for_home(&config, &user_config, Some("main-model"), &home.0, None)
                .unwrap();
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"next"
        }))
        .unwrap();
        message.resolved_session_id = Some("compression-route-session".into());
        let history = [session_db::CompressionHistoryMessage {
            id: 1,
            message: session_db::HistoryMessage {
                role: "user".into(),
                content: "preserve this context".into(),
                api_content: None,
            },
            tool_call_id: None,
            tool_calls: None,
            tool_name: None,
            effect_disposition: None,
            finish_reason: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            codex_reasoning_items: None,
            codex_message_items: None,
            display_kind: None,
            display_metadata: None,
            timestamp: 0.0,
            compressed_summary: false,
        }];
        let summary = agent
            .summarize_context(agent::TurnContext::default(), &message, &history, None)
            .await
            .unwrap();
        assert_eq!(summary.as_deref(), Some("chain summary"));
        let summary = agent
            .summarize_context(agent::TurnContext::default(), &message, &history, None)
            .await
            .unwrap();
        assert_eq!(summary.as_deref(), Some("auxiliary summary"));

        {
            let auxiliary_guard = auxiliary_requests.lock().unwrap();
            assert_eq!(auxiliary_guard.len(), 2);
            let (headers, body) = &auxiliary_guard[0];
            assert_eq!(headers["authorization"], "Bearer aux-key");
            assert_eq!(
                headers["http-referer"],
                "https://hermes-agent.nousresearch.com"
            );
            assert_eq!(headers["x-title"], "Hermes Agent");
            assert_eq!(headers["x-openrouter-categories"], "productivity,cli-agent");
            assert_eq!(body["model"], "aux-model");
            assert_eq!(body["max_tokens"], 777);
            assert_eq!(body["reasoning"], json!({"enabled":false}));
            assert_eq!(body["route_marker"], "auxiliary-only");
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
            assert!(body.get("tools").is_none());

            let fallback_guard = fallback_requests.lock().unwrap();
            assert_eq!(fallback_guard.len(), 1);
            let (headers, body) = &fallback_guard[0];
            assert_eq!(headers["authorization"], "Bearer fallback-key");
            assert_eq!(body["model"], "fallback-model");
            assert_eq!(body["max_tokens"], 333);
            assert_eq!(body["reasoning"], json!({"enabled":false,"effort":"none"}));
            assert_eq!(body["route_marker"], "auxiliary-only");
            assert!(body.get("tools").is_none());
        }
        assert!(main_requests.lock().unwrap().is_empty());

        let inherited_config = json!({
            "model":{"provider":"custom"},
            "auxiliary":{"compression":{
                "provider":"auto",
                "fallback_chain":[
                    {"provider":"openrouter", "model":"fallback-model",
                     "base_url":fallback_url, "api_key":"fallback-key"}
                ]
            }}
        });
        let inherited = build_agent_client_for_home(
            &config,
            &inherited_config,
            Some("main-model"),
            &home.0,
            None,
        )
        .unwrap();
        let summary = inherited
            .summarize_context(agent::TurnContext::default(), &message, &history, None)
            .await
            .unwrap();
        assert_eq!(summary.as_deref(), Some("chain summary"));
        assert_eq!(main_requests.lock().unwrap().len(), 1);
        assert_eq!(fallback_requests.lock().unwrap().len(), 2);

        let main_chain_config = json!({
            "model":{"provider":"openrouter"},
            "fallback_providers":[
                {"provider":"openrouter", "model":"duplicate-main"},
                {"provider":"custom", "model":"main-fallback-model",
                 "base_url":main_fallback_url, "api_key":"main-fallback-key",
                 "timeout":1, "reasoning_effort":false,
                 "max_output_tokens":111}
            ],
            "auxiliary":{"compression":{
                "provider":"auto",
                "extra_body":{"route_marker":"auto-task"}
            }}
        });
        let main_chain = build_agent_client_for_home(
            &config,
            &main_chain_config,
            Some("main-model"),
            &home.0,
            None,
        )
        .unwrap();
        let summary = main_chain
            .summarize_context(agent::TurnContext::default(), &message, &history, None)
            .await
            .unwrap();
        assert_eq!(
            summary.as_deref(),
            Some("main-chain summary"),
            "main requests: {:?}, fallback requests: {:?}",
            main_requests
                .lock()
                .unwrap()
                .iter()
                .map(|(_, body)| body["model"].clone())
                .collect::<Vec<_>>(),
            main_fallback_requests.lock().unwrap().len()
        );
        assert_eq!(main_requests.lock().unwrap().len(), 2);
        assert_eq!(main_requests.lock().unwrap()[1].1["model"], "main-model");
        {
            let main_fallback_guard = main_fallback_requests.lock().unwrap();
            assert_eq!(main_fallback_guard.len(), 1);
            let (headers, body) = &main_fallback_guard[0];
            assert_eq!(headers["authorization"], "Bearer main-fallback-key");
            assert_eq!(body["model"], "main-fallback-model");
            assert_eq!(body["route_marker"], "auto-task");
            assert!(body.get("max_tokens").is_none());
            assert!(body.get("max_completion_tokens").is_none());
            assert!(body.get("reasoning").is_none());
            assert!(body.get("tools").is_none());
        }

        let discovery_config = json!({
            "model":{"provider":"openrouter"},
            "auxiliary":{"compression":{
                "provider":"auto",
                "extra_body":{"route_marker":"discovery-task"}
            }}
        });
        let discovered = build_agent_client_for_home(
            &config,
            &discovery_config,
            Some("main-model"),
            &home.0,
            None,
        )
        .unwrap();
        let summary = discovered
            .summarize_context(agent::TurnContext::default(), &message, &history, None)
            .await
            .unwrap();
        assert_eq!(summary.as_deref(), Some("discovered summary"));
        let main_guard = main_requests.lock().unwrap();
        let discovery_guard = discovery_requests.lock().unwrap();
        assert_eq!(main_guard.len(), 3);
        assert_eq!(discovery_guard.len(), 1);
        let (headers, body) = &discovery_guard[0];
        assert_eq!(headers["authorization"], "Bearer gmi-pool-key");
        assert_eq!(body["model"], "google/gemini-3.1-flash-lite-preview");
        assert_eq!(body["route_marker"], "discovery-task");
        assert_eq!(body["messages"], main_guard[2].1["messages"]);
        assert!(body.get("tools").is_none());
    }

    #[tokio::test]
    async fn auto_compression_discovers_nous_oauth_before_static_profiles() {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        use base64::Engine as _;

        type Captures = Arc<std::sync::Mutex<Vec<(HeaderMap, Value)>>>;
        let requests: Captures = Default::default();
        let captured = requests.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                captured.lock().unwrap().push((headers.clone(), body));
                async move {
                    if headers["authorization"] == "Bearer main-key" {
                        Json(json!({
                            "choices":[{"finish_reason":"length","message":{"content":"partial"}}]
                        }))
                    } else {
                        Json(json!({
                            "choices":[{"finish_reason":"stop","message":{"content":"nous summary"}}]
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home =
            TempHome(std::env::temp_dir().join(format!("hermes-nous-auto-discovery-{nonce}")));
        std::fs::create_dir_all(&home.0).unwrap();
        std::fs::write(
            home.0.join(".env"),
            format!(
                "NOUS_INFERENCE_BASE_URL={base_url}\nGMI_API_KEY=gmi-key\nGMI_BASE_URL={base_url}\n"
            ),
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({"exp":now + 3600,"scope":"inference:invoke"})).unwrap(),
        );
        let token = format!("{header}.{payload}.signature");
        std::fs::write(
            home.0.join("auth.json"),
            serde_json::to_vec(&json!({"providers":{"nous":{
                "access_token":token,
                "refresh_token":"refresh",
                "scope":"inference:invoke",
                "inference_base_url":"https://inference-api.nousresearch.com/v1"
            }}}))
            .unwrap(),
        )
        .unwrap();

        let mut config = native_config();
        config.llm_api_key = Some("main-key".into());
        config.llm_base_url = Some(base_url);
        let user_config = json!({
            "model":{"provider":"custom"},
            "auxiliary":{"compression":{"provider":"auto"}}
        });
        let agent =
            build_agent_client_for_home(&config, &user_config, Some("main-model"), &home.0, None)
                .unwrap();
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"next"
        }))
        .unwrap();
        message.resolved_session_id = Some("nous-auto-session".into());
        let history = [session_db::CompressionHistoryMessage {
            id: 1,
            message: session_db::HistoryMessage {
                role: "user".into(),
                content: "preserve this context".into(),
                api_content: None,
            },
            tool_call_id: None,
            tool_calls: None,
            tool_name: None,
            effect_disposition: None,
            finish_reason: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            codex_reasoning_items: None,
            codex_message_items: None,
            display_kind: None,
            display_metadata: None,
            timestamp: 0.0,
            compressed_summary: false,
        }];

        let summary = agent
            .summarize_context(agent::TurnContext::default(), &message, &history, None)
            .await
            .unwrap();
        assert_eq!(summary.as_deref(), Some("nous summary"));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0["authorization"], "Bearer main-key");
        assert_eq!(requests[1].0["authorization"], format!("Bearer {token}"));
        assert_eq!(requests[1].1["model"], nous_credentials::DEFAULT_MODEL);
        assert_eq!(requests[1].1["messages"], requests[0].1["messages"]);
        assert_eq!(requests[1].1["tags"][0], "product=hermes-agent");
        assert!(requests[1].1.get("tools").is_none());
        drop(requests);
        server.abort();
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
        let hook = home.0.join("hooks/compression-proof");
        std::fs::create_dir_all(&hook).unwrap();
        std::fs::write(
            hook.join("HOOK.yaml"),
            "name: compression-proof\nevents:\n  - session:compress\n",
        )
        .unwrap();
        std::fs::write(
            hook.join("handler.sh"),
            r#"payload=$(cat)
printf '{"event":"%s","home":"%s","context":%s}' "$1" "$HERMES_HOME" "$payload" > "$HERMES_HOME/native-hook.tmp"
mv "$HERMES_HOME/native-hook.tmp" "$HERMES_HOME/native-hook.json"
"#,
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
                ConversationRuntime {
                    database: Some(&database),
                    process_registry: Arc::new(background_process::Registry::new()),
                    tool_approvals: Arc::new(tool_approval::ApprovalBroker::new()),
                    compression_health: Arc::new(compression_discovery::Health::default()),
                },
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
        run(first.clone()).await;
        first
            .notify_compression_boundary(
                agent::TurnContext::from_database(Some(&database)),
                "session-one",
                "session-one",
                true,
            )
            .await
            .unwrap();
        let hook_record = home.0.join("native-hook.json");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !hook_record.is_file() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("production conversation hook did not finish");
        let observed: Value =
            serde_json::from_str(&std::fs::read_to_string(hook_record).unwrap()).unwrap();
        assert_eq!(observed["event"], "session:compress");
        assert_eq!(observed["home"], home.0.to_string_lossy().as_ref());
        assert_eq!(
            observed["context"],
            json!({
                "platform": "cli",
                "session_id": "session-one",
                "old_session_id": "",
                "in_place": true,
                "compression_count": 1,
            })
        );

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
                ConversationRuntime {
                    database: Some(&database),
                    process_registry: Arc::new(background_process::Registry::new()),
                    tool_approvals: Arc::new(tool_approval::ApprovalBroker::new()),
                    compression_health: Arc::new(compression_discovery::Health::default()),
                },
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

    #[cfg(unix)]
    #[tokio::test]
    async fn native_terminal_runs_in_live_tool_loop_with_frozen_schema_and_durable_cwd() {
        use axum::{extract::State, routing::post, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        type ModelState = (Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<Value>>>);
        async fn model(
            State((calls, requests)): State<ModelState>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            requests.lock().unwrap().push(body.clone());
            match call {
                0 => Json(
                    json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"terminal-1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"bash -c 'true' && cd child && export LIVE_NATIVE=yes && printf first\"}"}}]}}]}),
                ),
                1 => {
                    assert!(body.to_string().contains("first"));
                    Json(
                        json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"terminal-2","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"printf '%s:%s' \\\"$PWD\\\" \\\"$LIVE_NATIVE\\\"\"}"}}]}}]}),
                    )
                }
                2 => {
                    assert!(body.to_string().contains("child:yes"));
                    Json(
                        json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"terminal-background","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"sleep 0.1; printf background-complete\",\"background\":true}"}}]}}]}),
                    )
                }
                3 => {
                    let content = &body["messages"].as_array().unwrap().last().unwrap()["content"];
                    let session_id = content["session_id"]
                        .as_str()
                        .expect("background session id");
                    Json(
                        json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"process-wait","type":"function","function":{"name":"process_manage","arguments":serde_json::to_string(&json!({"action":"wait","session_id":session_id,"timeout":5})).unwrap()}}]}}]}),
                    )
                }
                4 => {
                    assert!(body.to_string().contains("background-complete"));
                    Json(json!({"choices":[{"message":{"role":"assistant","content":"done"}}]}))
                }
                _ => panic!("unexpected model request"),
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
            TempDir(std::env::temp_dir().join(format!("hermes-live-native-terminal-{nonce}")));
        std::fs::create_dir_all(home.0.join("child")).unwrap();
        std::fs::write(
            home.0.join("config.yaml"),
            "model:\n  default: fixture-model\n  provider: openrouter\nterminal:\n  backend: local\n  timeout: 5\napprovals:\n  mode: manual\n  timeout: 5\n  deny:\n    - 'git push*'\nsecurity:\n  tirith_enabled: false\n",
        )
        .unwrap();
        let database = session_db::SessionDb::open_shared(home.0.join("state.db")).unwrap();
        database
            .create_session(
                "terminal-session",
                &session_db::SessionCreate {
                    peer: session_db::GatewayPeer {
                        source: "telegram",
                        session_key: Some("terminal-route"),
                        ..Default::default()
                    },
                    cwd: home.0.to_str(),
                    ..Default::default()
                },
            )
            .unwrap();
        database
            .save_gateway_routing_entry(
                "profile",
                "terminal-route",
                r#"{"session_id":"terminal-session"}"#,
            )
            .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/chat/completions", post(model))
            .with_state((calls.clone(), requests.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut config = native_config();
        config.agent_model = Some("fixture-model".into());
        config.llm_base_url = Some(base_url);
        config.agent_cwd = home.0.clone();
        config.agent_tools = true;
        let mut message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"telegram", "channel_id":"chat", "sender_id":"user", "text":"run"
        }))
        .unwrap();
        message.resolved_session_id = Some("terminal-session".into());
        let initializer =
            conversation_prompt::Initializer::capture(home.0.clone(), home.0.clone()).unwrap();
        let approvals = Arc::new(tool_approval::ApprovalBroker::new());
        let client = secret_scope::with_secret_scope(
            Some(Default::default()),
            build_conversation_client(
                &config,
                &initializer,
                &home.0,
                &message,
                &[],
                ConversationRuntime {
                    database: Some(&database),
                    process_registry: Arc::new(background_process::Registry::new()),
                    tool_approvals: approvals.clone(),
                    compression_health: Arc::new(compression_discovery::Health::default()),
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            database.get_session("terminal-session").unwrap().unwrap()["tool_names"],
            r#"["current_time","terminal","process_manage"]"#
        );
        let (sender, mut receiver) = tokio::sync::mpsc::channel(32);
        let run_client = client.clone();
        let run_message = message.clone();
        let turn = tokio::spawn(async move {
            run_client
                .run_turn_with_context(
                    agent::TurnContext::from_database(None).with_route_key(Some("terminal-route")),
                    &run_message,
                    &[],
                    sender,
                )
                .await
        });
        let mut approval_prompts = 0;
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::ApprovalRequest { .. } = event {
                approval_prompts += 1;
                assert!(matches!(
                    approvals.resolve_text("terminal-route", "/approve session", |_| true),
                    tool_approval::ResolveOutcome::Resolved { .. }
                ));
            }
        }
        turn.await.unwrap().unwrap();
        assert_eq!(approval_prompts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(
            database.get_session("terminal-session").unwrap().unwrap()["cwd"],
            home.0.join("child").to_string_lossy().as_ref()
        );
        let requests = requests.lock().unwrap();
        let schemas: Vec<_> = requests
            .iter()
            .map(|request| request["tools"].clone())
            .collect();
        assert!(schemas[0]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "terminal"));
        assert!(schemas[0]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "process_manage"));
        assert!(schemas.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(requests[1].to_string().contains(
            "Command required approval (shell command via -c/-lc flag) and was approved by the user."
        ));
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
                ConversationRuntime {
                    database: Some(&database),
                    process_registry: Arc::new(background_process::Registry::new()),
                    tool_approvals: Arc::new(tool_approval::ApprovalBroker::new()),
                    compression_health: Arc::new(compression_discovery::Health::default()),
                },
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
                ConversationRuntime {
                    database: Some(&database),
                    process_registry: Arc::new(background_process::Registry::new()),
                    tool_approvals: Arc::new(tool_approval::ApprovalBroker::new()),
                    compression_health: Arc::new(compression_discovery::Health::default()),
                },
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
                        ([("content-type", "text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").into_response()
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

    #[tokio::test]
    async fn native_main_pool_recovers_streaming_and_tool_requests() {
        use axum::{
            body::Bytes,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::post,
            Json, Router,
        };

        type Captures = Arc<std::sync::Mutex<Vec<(HeaderMap, Vec<u8>)>>>;
        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        for (tools, failure_status, usage_limit) in [
            (false, StatusCode::UNAUTHORIZED, false),
            (true, StatusCode::UNAUTHORIZED, false),
            (false, StatusCode::TOO_MANY_REQUESTS, false),
            (true, StatusCode::TOO_MANY_REQUESTS, false),
            (false, StatusCode::TOO_MANY_REQUESTS, true),
            (true, StatusCode::TOO_MANY_REQUESTS, true),
        ] {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let home = TempHome(std::env::temp_dir().join(format!(
                "hermes-main-pool-{}-{tools}-{}-{usage_limit}-{nonce}",
                std::process::id(),
                failure_status.as_u16()
            )));
            std::fs::create_dir_all(&home.0).unwrap();
            let auth_path = home.0.join("auth.json");
            let first_requests: Captures = Default::default();
            let captured = first_requests.clone();
            let first_app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, body: Bytes| {
                    captured.lock().unwrap().push((headers, body.to_vec()));
                    async move {
                        (
                            failure_status,
                            Json(if failure_status == StatusCode::UNAUTHORIZED {
                                json!({"error":{"message":"invalid api key"}})
                            } else if usage_limit {
                                json!({"error":{"message":"Usage limit reached. Try again in 1 hour."}})
                            } else {
                                json!({"error":{"message":"rate limit exceeded"}})
                            }),
                        )
                    }
                }),
            );
            let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let first_url = format!("http://{}", first_listener.local_addr().unwrap());
            let _first_server = Server(tokio::spawn(async move {
                axum::serve(first_listener, first_app).await.unwrap();
            }));

            let second_requests: Captures = Default::default();
            let persisted_before_retry = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let captured = second_requests.clone();
            let persisted = persisted_before_retry.clone();
            let persisted_path = auth_path.clone();
            let second_app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, body: Bytes| {
                    let captured = captured.clone();
                    let persisted = persisted.clone();
                    let persisted_path = persisted_path.clone();
                    async move {
                        let auth: Value = serde_json::from_slice(
                            &std::fs::read(persisted_path).expect("persisted auth store"),
                        )
                        .unwrap();
                        persisted.store(
                            auth["credential_pool"]["gmi"][0]["last_status"] == "exhausted",
                            std::sync::atomic::Ordering::Release,
                        );
                        let parsed: Value = serde_json::from_slice(&body).unwrap();
                        let streaming = parsed["stream"] == true;
                        let index = {
                            let mut captured = captured.lock().unwrap();
                            let index = captured.len();
                            captured.push((headers, body.to_vec()));
                            index
                        };
                        if streaming {
                            (
                                [("content-type", "text/event-stream")],
                                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                            )
                                .into_response()
                        } else if tools && index == 0 {
                            Json(json!({"choices":[{"message":{
                                "role":"assistant", "content":null,
                                "tool_calls":[{"id":"clock","type":"function","function":{
                                    "name":"current_time","arguments":"{}"
                                }}]
                            }}]}))
                            .into_response()
                        } else {
                            Json(json!({
                                "choices":[{"message":{"role":"assistant","content":"ok"}}]
                            }))
                            .into_response()
                        }
                    }
                }),
            );
            let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let second_url = format!("http://{}", second_listener.local_addr().unwrap());
            let _second_server = Server(tokio::spawn(async move {
                axum::serve(second_listener, second_app).await.unwrap();
            }));

            std::fs::write(
                &auth_path,
                serde_json::to_vec(&json!({
                    "credential_pool":{"gmi":[
                        {"id":"failed","auth_type":"api_key","source":"manual",
                         "priority":0,"access_token":"pool-key-one","base_url":first_url},
                        {"id":"healthy","auth_type":"api_key","source":"manual",
                         "priority":1,"access_token":"pool-key-two","base_url":second_url}
                    ]}
                }))
                .unwrap(),
            )
            .unwrap();
            std::fs::write(home.0.join(".env"), "GMI_API_KEY=environment-key\n").unwrap();

            let mut config = native_config();
            config.agent_tools = tools;
            config.llm_api_key = None;
            config.llm_base_url = None;
            let user_config = json!({
                "model":{"provider":"gmi","base_url":first_url},
                "custom_providers":[
                    {"name":"first-route","base_url":first_url,
                     "extra_headers":{"X-Route-Token":"first-only"}},
                    {"name":"second-route","base_url":second_url,
                     "extra_headers":{"X-Route-Token":"second-only"}}
                ]
            });
            let agent = build_agent_client_for_home(
                &config,
                &user_config,
                Some("fixture-model"),
                &home.0,
                None,
            )
            .unwrap();
            let message: hermes_core::Message = serde_json::from_value(json!({
                "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
            }))
            .unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
            agent.run_turn(&message, &[], tx).await.unwrap();
            while rx.recv().await.is_some() {}

            assert!(persisted_before_retry.load(std::sync::atomic::Ordering::Acquire));
            let first = first_requests.lock().unwrap();
            let second = second_requests.lock().unwrap();
            let expected_first = if failure_status == StatusCode::TOO_MANY_REQUESTS && !usage_limit
            {
                2
            } else {
                1
            };
            assert_eq!(first.len(), expected_first);
            assert_eq!(second.len(), if tools { 2 } else { 1 });
            for (headers, body) in first.iter() {
                assert_eq!(headers["authorization"], "Bearer pool-key-one");
                assert_eq!(headers["x-route-token"], "first-only");
                assert_eq!(body, &second[0].1);
            }
            for (headers, _) in second.iter() {
                assert_eq!(headers["authorization"], "Bearer pool-key-two");
                assert_eq!(headers["x-route-token"], "second-only");
            }
            if tools {
                let first_body: Value = serde_json::from_slice(&second[0].1).unwrap();
                let next_body: Value = serde_json::from_slice(&second[1].1).unwrap();
                assert_eq!(next_body["tools"], first_body["tools"]);
                assert_eq!(next_body["messages"][0], first_body["messages"][0]);
            }
        }
    }

    #[tokio::test]
    async fn native_main_pool_exhausts_before_cross_provider_fallback() {
        use axum::{
            body::Bytes,
            http::{HeaderMap, StatusCode},
            routing::post,
            Json, Router,
        };

        type Captures = Arc<std::sync::Mutex<Vec<(HeaderMap, Vec<u8>)>>>;
        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        struct TempHome(std::path::PathBuf);
        impl Drop for TempHome {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn failing(captures: Captures) -> (String, Server) {
            let app = Router::new().route(
                "/chat/completions",
                post(move |headers: HeaderMap, body: Bytes| {
                    captures.lock().unwrap().push((headers, body.to_vec()));
                    async move {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({"error":{"message":"Usage limit reached"}})),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            (base_url, server)
        }

        let first_requests: Captures = Default::default();
        let second_requests: Captures = Default::default();
        let fallback_requests: Captures = Default::default();
        let (first_url, _first_server) = failing(first_requests.clone()).await;
        let (second_url, _second_server) = failing(second_requests.clone()).await;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = TempHome(std::env::temp_dir().join(format!(
            "hermes-main-pool-fallback-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&home.0).unwrap();
        let auth_path = home.0.join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::to_vec(&json!({
                "credential_pool":{"gmi":[
                    {"id":"first","auth_type":"api_key","source":"manual",
                     "priority":0,"access_token":"pool-key-one","base_url":first_url},
                    {"id":"second","auth_type":"api_key","source":"manual",
                     "priority":1,"access_token":"pool-key-two","base_url":second_url}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(home.0.join(".env"), "GMI_API_KEY=environment-key\n").unwrap();

        let persisted_before_fallback = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = fallback_requests.clone();
        let persisted = persisted_before_fallback.clone();
        let persisted_path = auth_path.clone();
        let fallback_app = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = captured.clone();
                let persisted = persisted.clone();
                let persisted_path = persisted_path.clone();
                async move {
                    let auth: Value = serde_json::from_slice(
                        &std::fs::read(persisted_path).expect("persisted auth store"),
                    )
                    .unwrap();
                    persisted.store(
                        auth["credential_pool"]["gmi"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|entry| entry["last_status"] == "exhausted"),
                        std::sync::atomic::Ordering::Release,
                    );
                    captured.lock().unwrap().push((headers, body.to_vec()));
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"choices\":[{\"delta\":{\"content\":\"pool fallback\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                }
            }),
        );
        let fallback_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fallback_url = format!("http://{}", fallback_listener.local_addr().unwrap());
        let _fallback_server = Server(tokio::spawn(async move {
            axum::serve(fallback_listener, fallback_app).await.unwrap();
        }));

        let mut config = native_config();
        config.llm_api_key = None;
        config.llm_base_url = None;
        let selected = json!({
            "model":{"provider":"gmi","base_url":first_url},
            "fallback_providers":[{
                "provider":"custom", "model":"fallback-model",
                "base_url":fallback_url, "api_key":"fallback-key"
            }]
        });
        let client = build_agent_client_for_home(
            &config,
            &selected,
            Some("primary-model"),
            &home.0,
            Some(NativeConversationState {
                system_prompt: "stable\n\nModel: primary-model\nProvider: gmi".into(),
                tools: Vec::new(),
                plugin_prompt: Default::default(),
                extension_host: None,
                hooks: None,
                platform: "cli".into(),
                context_length: 256_000,
            }),
        )
        .unwrap();
        let message: hermes_core::Message = serde_json::from_value(json!({
            "platform":"cli", "channel_id":"chat", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        client.run_turn(&message, &[], sender).await.unwrap();
        let mut reply = String::new();
        while let Some(event) = receiver.recv().await {
            if let hermes_core::StreamEvent::MessageChunk { text } = event {
                reply.push_str(&text);
            }
        }
        assert_eq!(reply, "pool fallback");
        assert!(persisted_before_fallback.load(std::sync::atomic::Ordering::Acquire));

        let first = first_requests.lock().unwrap();
        let second = second_requests.lock().unwrap();
        let fallback = fallback_requests.lock().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(fallback.len(), 1);
        assert_eq!(first[0].0["authorization"], "Bearer pool-key-one");
        assert_eq!(second[0].0["authorization"], "Bearer pool-key-two");
        assert_eq!(fallback[0].0["authorization"], "Bearer fallback-key");
        assert_eq!(first[0].1, second[0].1);
        let primary_body: Value = serde_json::from_slice(&first[0].1).unwrap();
        let fallback_body: Value = serde_json::from_slice(&fallback[0].1).unwrap();
        assert_eq!(
            &primary_body["messages"].as_array().unwrap()[1..],
            &fallback_body["messages"].as_array().unwrap()[1..]
        );
        assert_eq!(fallback_body["model"], "fallback-model");
    }
}
