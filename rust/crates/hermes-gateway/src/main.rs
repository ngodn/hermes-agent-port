//! Hermes gateway (Rust rewrite) — entry point.
//!
//! This is the strangler-fig seam: it stands up the long-lived network process
//! that the Python `gateway/run.py` owns today. Platform adapters and the
//! agent RPC boundary are ported in behind this skeleton one at a time.

mod agent;
mod config;
mod display_config;
mod dispatch;
mod health;
mod message;
mod platform;
mod response_filters;
mod session_stall;
mod slash_access;
mod telegram;
mod whatsapp_identity;
mod session_state;
mod turn_lease;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::agent::SubprocessAgentClient;
use crate::config::Config;
use crate::health::{healthz, readyz, AppState};
use crate::message::post_message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hermes_gateway=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env()?;

    // Strangler step: drive the existing Python agent as a subprocess. Swapped
    // for a native client once run_agent.py is ported.
    let mut agent = SubprocessAgentClient::new(config.agent_python.clone(), config.agent_cwd.clone());
    if let Some(model) = &config.agent_model {
        agent = agent.with_model(model.clone());
    }
    let state = AppState::new(Arc::new(agent));

    // Push path: when a Telegram token is configured, start the adapter's
    // getUpdates loop feeding the Dispatcher, which runs turns and delivers
    // replies back via sendMessage. Shares the same AgentClient as /message.
    if let Some(token) = config.telegram_token.clone() {
        use crate::dispatch::Dispatcher;
        use crate::platform::PlatformAdapter;
        use crate::telegram::TelegramAdapter;
        use hermes_core::{Message, Platform};

        let tg = Arc::new(TelegramAdapter::new(token)?);
        let mut dispatcher = Dispatcher::new(state.agent.clone());
        dispatcher.register_adapter(Platform::Telegram, tg.clone() as Arc<dyn PlatformAdapter>);
        let dispatcher = Arc::new(dispatcher);

        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<Message>(128);
        let tg_run = tg.clone();
        tokio::spawn(async move {
            if let Err(err) = tg_run.run(inbound_tx).await {
                tracing::error!(%err, "telegram adapter loop exited");
            }
        });
        tokio::spawn(dispatcher.run(inbound_rx));
        tracing::info!("telegram push path started");
    }

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/message", post(post_message))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(addr = %config.bind, "hermes-gateway listening");

    // Startup work (adapter registration, DB recovery, ...) happens here as it
    // is ported. Once complete the gateway flips readiness on.
    state.mark_ready();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Wait for SIGINT / SIGTERM so we drain cleanly, mirroring the Python
/// gateway's `shutdown_flush` / `drain_control` behavior.
async fn shutdown_signal() {
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

    tracing::info!("shutdown signal received, draining");
}
