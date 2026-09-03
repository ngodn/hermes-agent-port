//! The `/message` route: a minimal request/response entrypoint.
//!
//! This is the first runnable end-to-end path: accept one message, run it
//! through the [`AgentClient`], accumulate the streamed reply, and return it.
//! It doubles as the "local" adapter (an HTTP caller instead of a chat
//! platform). Push-based platform adapters (Telegram et al.) drive the async
//! [`Dispatcher`](crate::dispatch) instead; both share the same AgentClient.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use hermes_core::{Message, Platform, StreamEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::display_config::ResolvedDisplayConfig;
use crate::health::AppState;

#[derive(Debug, Deserialize)]
pub struct MessageRequest {
    /// Opaque conversation id. Defaults to "local" when omitted.
    #[serde(default = "default_channel")]
    pub channel_id: String,
    /// Opaque sender id. Defaults to "local" when omitted.
    #[serde(default = "default_sender")]
    pub sender_id: String,
    pub text: String,
}

fn default_channel() -> String {
    "local".into()
}
fn default_sender() -> String {
    "local".into()
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub reply: String,
}

/// Resolve the effective display config for a platform from the loaded user
/// config. Introspection endpoint: lets an operator verify how per-platform
/// display settings resolve against the built-in tiered defaults.
pub async fn get_display_config(
    State(state): State<AppState>,
    Path(platform): Path<String>,
) -> Json<ResolvedDisplayConfig> {
    Json(ResolvedDisplayConfig::resolve(&state.user_config, &platform))
}

/// Run one turn synchronously and return the assembled reply.
pub async fn post_message(
    State(state): State<AppState>,
    Json(req): Json<MessageRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<serde_json::Value>)> {
    let msg = Message {
        platform: Platform::Cli,
        channel_id: req.channel_id,
        sender_id: req.sender_id,
        text: req.text,
        chat_type: Some("dm".to_string()),
    };

    // Slash-command gating, same policy as the push path.
    if let crate::slash::SlashDecision::Denied { command } =
        crate::slash::evaluate(&state.user_config, &msg)
    {
        return Ok(Json(MessageResponse {
            reply: crate::slash::denial_text(&command),
        }));
    }

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
    let agent = state.agent.clone();
    let msg_for_agent = msg.clone();
    let turn = tokio::spawn(async move { agent.run_turn(&msg_for_agent, tx).await });

    // Assemble text + commentary into one reply, same rule as the Dispatcher.
    let mut reply = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::MessageChunk { text } => reply.push_str(&text),
            StreamEvent::Commentary { text } => {
                if !reply.is_empty() {
                    reply.push_str("\n\n");
                }
                reply.push_str(&text);
            }
            StreamEvent::MessageStop { final_: true } => break,
            StreamEvent::MessageStop { final_: false } => reply.push_str("\n\n"),
            StreamEvent::ToolCallChunk { .. }
            | StreamEvent::ToolCallFinished { .. }
            | StreamEvent::LongToolHint { .. }
            | StreamEvent::GatewayNotice { .. } => {}
        }
    }

    // Intentional-silence markers suppress delivery: return an empty reply
    // rather than echoing "NO_REPLY" to the caller.
    if crate::response_filters::is_intentional_silence_response(&reply) {
        reply.clear();
    }

    match turn.await {
        Ok(Ok(())) => Ok(Json(MessageResponse { reply })),
        Ok(Err(err)) => {
            warn!(%err, "agent turn failed");
            Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": err.to_string() })),
            ))
        }
        Err(err) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("agent task panicked: {err}") })),
        )),
    }
}
