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
mod browser_control_artifacts;
mod browser_control_broker;
mod cache_paths;
mod cgroup_cleanup;
mod channel_directory;
mod chat_message_projection;
mod cli_agent;
mod code_skew;
mod config;
mod config_env_overrides;
mod config_file;
mod config_gateway;
mod config_loader;
mod config_schema;
mod config_types;
mod control_socket;
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
mod file_read_safety;
mod gemini_thinking;
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
mod pending_messages;
mod pending_stt;
mod platform;
mod platform_base_types;
mod platform_helpers;
mod profile_name;
mod profile_routing;
mod prompt_cache;
mod provider_registry;
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
mod runtime_footer;
mod scale_to_zero;
mod secret_scope;
mod session;
mod session_db;
mod session_db_recovery;
mod session_image_routing;
mod session_registry;
mod session_stall;
mod session_state;
mod shutdown_flush;
mod shutdown_forensics;
mod shutdown_watchdog;
mod signal_format;
mod signal_rate_limit;
mod slack;
mod slash;
mod slash_access;
mod status;
mod status_phrases;
mod sticker_cache;
mod stream_consumer;
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

/// Pick the agent backend. Native (in-Rust LLM chat) requires opt-in
/// (`HERMES_AGENT_NATIVE`), an API key (`HERMES_LLM_API_KEY`), and a resolved
/// model; anything missing falls back to the Python subprocess bridge so the
/// gateway never silently does nothing.
fn build_agent_client(
    config: &Config,
    user_config: &serde_json::Value,
    model: Option<&str>,
) -> Arc<dyn AgentClient> {
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
        return Arc::new(cli_agent::CliAgentClient::new(program, extra, prompt_flag));
    }

    if config.agent_native {
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
                        .and_then(|name| std::env::var(name).ok())
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                })
            })
            .or_else(|| profile.as_ref().map(|profile| profile.base_url.clone()))
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());

        // Explicit native credentials win. Registered profiles use their own
        // declared key names; generic configurations retain the legacy lookup.
        let key = config.llm_api_key.clone().or_else(|| {
            let dotenv = config_file::load_dotenv(&config_file::env_path());
            match &profile {
                Some(profile) => config_file::resolve_profile_api_key(profile, &dotenv, |name| {
                    std::env::var(name).ok()
                }),
                None => config_file::resolve_provider_api_key(&base_url, &dotenv),
            }
        });

        match (key, model) {
            (Some(key), Some(model)) => match NativeAgentClient::new(model, key, base_url.clone()).and_then(|client| {
                let limit = turn_limit::gateway(user_config, std::env::var("HERMES_MAX_ITERATIONS").ok().as_deref())?;
                let client = client.with_turn_limit(limit).with_max_concurrent_children(delegation_policy::max_children(user_config, std::env::var("DELEGATION_MAX_CONCURRENT_CHILDREN").ok().as_deref()));
                let client = match &profile { Some(profile) => client.with_provider_profile(profile)?, None => client };
                client.with_extra_headers(&custom_provider_config::extra_headers(user_config, &base_url))
            }) {
                Ok(mut c) => {
                    c = c.with_reasoning_config(reasoning_effort::resolve_config(user_config, model));
                    c = c.with_reasoning_echo(
                        python_value::truthy(&user_config["model"]["reasoning_echo"])
                            || reasoning_replay::needs_echo(user_config["model"]["provider"].as_str().unwrap_or(""), model, &base_url),
                    );
                    let requested_provider = user_config["model"]["provider"].as_str().unwrap_or("");
                    let named = custom_provider_config::named(user_config, requested_provider, profile.as_ref().map(|profile| profile.name.as_str()), |name| {
                        std::env::var(name).ok().or_else(|| config_file::load_dotenv(&config_file::env_path()).remove(name)).unwrap_or_default()
                    });
                    let named_overrides = named.as_ref().and_then(|entry|entry["extra_body"].as_object()).filter(|body|!body.is_empty()).cloned();
                    if let Some(extra) = named_overrides.or_else(|| custom_request_config::select_extra_body(requested_provider, model, &base_url, &custom_provider_config::compatible(user_config))) {
                        c = c.with_request_overrides(serde_json::Map::from_iter([("extra_body".into(), serde_json::Value::Object(extra))]));
                    }
                    // A saved provider supplies the fallback cap. Global and
                    // environment limits retain the gateway's precedence.
                    c = c.with_output_cap(native_agent::resolve_output_cap(&user_config["model"]["max_tokens"], std::env::var("HERMES_MAX_TOKENS").ok().as_deref(), named.as_ref().and_then(|entry| entry.get("max_output_tokens"))));
                    if config.agent_tools {
                        c = c.with_tools(vec![Arc::new(crate::native_tools::CurrentTimeTool)]);
                        tracing::info!(model, base_url, "using native agent client (tools enabled)");
                    } else {
                        tracing::info!(model, base_url, "using native agent client");
                    }
                    return Arc::new(c);
                }
                Err(err) => {
                    tracing::error!(%err, "native agent init failed; falling back to subprocess")
                }
            },
            (None, _) => tracing::warn!(
                base_url,
                "HERMES_AGENT_NATIVE set but no API key found (env or .env) for this provider; falling back to subprocess"
            ),
            (_, None) => tracing::warn!(
                "HERMES_AGENT_NATIVE set but no model resolved; falling back to subprocess"
            ),
        }
    }
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
    );
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

    // Conversation-history store. Backends that manage their own history (the
    // Python bridge) ignore it; native/CLI backends use it for multi-turn.
    let session_db = match session_db::SessionDb::open_default() {
        Ok(db) => Some(Arc::new(db)),
        Err(err) => {
            tracing::warn!(%err, "session store unavailable; turns will be stateless");
            None
        }
    };

    let state = AppState::new(agent, user_config, configured_model, session_db);

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
                Arc::new(tg),
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
                Arc::new(dc),
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
                Arc::new(sl),
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

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;

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
                },
                session_db::HistoryMessage {
                    role: "assistant".into(),
                    content: "reply".into(),
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
