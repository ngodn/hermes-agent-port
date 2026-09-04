//! Hermes gateway (Rust rewrite) — entry point.
//!
//! This is the strangler-fig seam: it stands up the long-lived network process
//! that the Python `gateway/run.py` owns today. Platform adapters and the
//! agent RPC boundary are ported in behind this skeleton one at a time.

mod agent;
mod cli_agent;
mod config;
mod config_file;
mod control_socket;
mod cwd_placeholder;
mod dead_targets;
mod delivery_ledger;
mod discord;
mod dispatch;
mod display_config;
mod drain_control;
mod health;
mod message;
mod native_agent;
mod native_tools;
mod platform;
mod readiness;
mod response_filters;
mod restart_loop_guard;
mod rich_sent_store;
mod session_db;
mod session_stall;
mod session_state;
mod slack;
mod slash;
mod slash_access;
mod sticker_cache;
mod systemd_notify;
mod telegram;
mod turn_lease;
mod whatsapp_identity;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::agent::{AgentClient, SubprocessAgentClient};
use crate::config::Config;
use crate::dispatch::Dispatcher;
use crate::health::{healthz, readyz, AppState};
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
        // base_url: explicit env override, else config's model.base_url, else OpenRouter.
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
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());

        // Key: explicit HERMES_LLM_API_KEY override, else resolve from the
        // process env / $HERMES_HOME/.env by the provider the base_url implies
        // (same source the Python agent uses). The value is never logged.
        let key = config.llm_api_key.clone().or_else(|| {
            let dotenv = config_file::load_dotenv(&config_file::env_path());
            config_file::resolve_provider_api_key(&base_url, &dotenv)
        });

        match (key, model) {
            (Some(key), Some(model)) => match NativeAgentClient::new(model, key, base_url.clone()) {
                Ok(mut c) => {
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

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;

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
