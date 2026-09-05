//! The `/message` route: a minimal request/response entrypoint.
//!
//! This is the first runnable end-to-end path: accept one message, run it
//! through the [`AgentClient`], accumulate the streamed reply, and return it.
//! It doubles as the "local" adapter (an HTTP caller instead of a chat
//! platform). Push-based platform adapters (Telegram et al.) drive the async
//! [`Dispatcher`](crate::dispatch) instead; both share the same AgentClient.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use hermes_core::{Message, Platform, StreamEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::display_config::ResolvedDisplayConfig;
use crate::health::AppState;

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// FTS5 query expression.
    pub q: String,
    #[serde(default)]
    pub limit: usize,
}

/// Full-text search across past conversation messages.
pub async fn get_search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Json<serde_json::Value> {
    let hits = state
        .session_db
        .as_ref()
        .map(|db| db.search(&params.q, params.limit).unwrap_or_default())
        .unwrap_or_default();
    Json(serde_json::json!({ "query": params.q, "hits": hits }))
}

#[derive(Debug, Deserialize)]
pub struct MessageRequest {
    /// Opaque conversation id. Defaults to "local" when omitted.
    #[serde(default = "default_channel")]
    pub channel_id: String,
    /// Opaque sender id. Defaults to "local" when omitted.
    #[serde(default = "default_sender")]
    pub sender_id: String,
    pub text: String,
    #[serde(default)]
    pub content_parts: Option<Vec<hermes_core::ContentPart>>,
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
    Json(ResolvedDisplayConfig::resolve(
        &state.user_config,
        &platform,
    ))
}

/// Run one turn synchronously and return the assembled reply.
pub async fn post_message(
    State(state): State<AppState>,
    Json(req): Json<MessageRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<serde_json::Value>)> {
    if req.content_parts.is_some() && !state.agent.supports_structured_content() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error":"configured agent backend does not accept structured content"}),
            ),
        ));
    }
    let msg = Message {
        platform: Platform::Cli,
        channel_id: req.channel_id,
        sender_id: req.sender_id,
        text: req.text,
        content_parts: req.content_parts,
        chat_type: Some("dm".to_string()),
    };

    // Slash-command gating + built-ins, same policy as the push path.
    match crate::slash::evaluate(&state.user_config, &msg) {
        crate::slash::SlashDecision::Denied { command } => {
            return Ok(Json(MessageResponse {
                reply: crate::slash::denial_text(&command),
            }));
        }
        crate::slash::SlashDecision::Allowed { command } => {
            if let Some(reply) = crate::slash::handle_builtin(&command, &msg, &state.user_config) {
                return Ok(Json(MessageResponse { reply }));
            }
        }
        crate::slash::SlashDecision::NotSlash => {}
    }

    // Load prior history + record the inbound message for stateless backends.
    let manages = state.agent.manages_history();
    let history = crate::session_db::begin_turn(state.session_db.as_deref(), manages, &msg, "cli");

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
    let agent = state.agent.clone();
    let msg_for_agent = msg.clone();
    let turn = tokio::spawn(async move { agent.run_turn(&msg_for_agent, &history, tx).await });

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

    // Record the assistant reply for stateless backends (before the silence gate).
    crate::session_db::end_turn(state.session_db.as_deref(), manages, &msg, &reply);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_agent::NativeAgentClient;
    use crate::native_tools::{Tool, ToolSpec};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use base64::Engine;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    struct TempHome(std::path::PathBuf);
    impl TempHome {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hermes-content-http-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn serve(router: axum::Router) -> (String, Server) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}"), Server(task))
    }

    async fn model(
        State(calls): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> Response {
        calls.lock().unwrap().push(body.clone());
        if body["stream"] == true {
            return (
                [("content-type", "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"seen\"}}]}\n\ndata: [DONE]\n\n",
            )
                .into_response();
        }
        let had_tool = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool");
        let message = if had_tool {
            json!({"content":"seen"})
        } else {
            json!({"tool_calls":[{"id":"call-1","type":"function","function":{"name":"echo","arguments":"{}"}}]})
        };
        Json(json!({"choices":[{"message":message}]})).into_response()
    }

    struct Echo;
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "test echo".into(),
                parameters: json!({"type":"object"}),
            }
        }
        fn call(&self, _: &Value) -> hermes_core::Result<String> {
            Ok("done".into())
        }
    }

    async fn roundtrip(with_tools: bool) {
        let home = TempHome::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (model_url, _model_server) = serve(
            axum::Router::new()
                .route("/chat/completions", post(model))
                .with_state(calls.clone()),
        )
        .await;
        let mut agent = NativeAgentClient::new("fixture-model", "fixture-key", model_url).unwrap();
        if with_tools {
            agent = agent.with_tools(vec![Arc::new(Echo)]);
        }
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        let state = AppState::new(Arc::new(agent), Arc::new(json!({})), None, Some(db.clone()));
        let (gateway_url, _gateway_server) = serve(
            axum::Router::new()
                .route("/message", post(post_message))
                .with_state(state),
        )
        .await;

        // Exercise real native preparation, HTTP deserialization, SQLite writes,
        // and model HTTP serialization, not just a request-builder unit test.
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/native-image-goldens.json")).unwrap();
        let image = home.0.join("image.png");
        std::fs::write(
            &image,
            base64::engine::general_purpose::STANDARD
                .decode(fixture["files"]["image.png"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        let policy = crate::file_read_safety::FileReadPolicy {
            home: home.0.clone(),
            cwd: home.0.clone(),
            hermes_home: home.0.join(".hermes"),
            hermes_root: home.0.join(".hermes"),
        };
        let options = crate::native_image_content::NativeImageOptions {
            read_policy: &policy,
            accepted_mimes: crate::native_image_content::UNIVERSALLY_SUPPORTED_MIMES,
        };
        let (parts, skipped) = crate::native_image_content::build_native_content_parts(
            "caption",
            &[image.to_str().unwrap().to_owned()],
            &[],
            &options,
        );
        assert!(skipped.is_empty());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap();
        for body in [
            json!({"channel_id":"picture","text":"caption","content_parts":parts}),
            json!({"channel_id":"picture","text":"follow-up"}),
        ] {
            let response = client
                .post(format!("{gateway_url}/message"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.json::<Value>().await.unwrap()["reply"], "seen");
        }
        let requests = calls.lock().unwrap();
        assert_eq!(requests.len(), if with_tools { 4 } else { 2 });
        for request in requests.iter() {
            assert_eq!(request["messages"][0]["content"], json!(parts));
        }
        let last = requests.last().unwrap()["messages"].as_array().unwrap();
        assert!(last
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "follow-up"));
        if with_tools {
            assert_eq!(last.last().unwrap()["role"], "tool");
        }
        drop(requests);
        // A newly opened connection can replay the first image-bearing turn.
        let reopened = crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap();
        let sid = crate::session_db::session_id_for(Platform::Cli, "picture");
        let history = reopened.load_history(&sid, 0).unwrap();
        assert_eq!(history[0].model_content(), json!(parts));
        assert_eq!(history.len(), 4);
    }

    #[tokio::test]
    async fn images_reach_streaming_model_and_survive_next_turn() {
        roundtrip(false).await;
    }
    #[tokio::test]
    async fn images_survive_tool_rounds_and_history_replay() {
        roundtrip(true).await;
    }

    #[tokio::test]
    async fn unsupported_backend_rejects_parts_before_persisting_or_running() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for TextOnly {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                _: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                panic!("unsupported content must be rejected before execution");
            }
        }
        let home = TempHome::new();
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        let state = AppState::new(
            Arc::new(TextOnly),
            Arc::new(json!({})),
            None,
            Some(db.clone()),
        );
        let (url, _server) = serve(
            axum::Router::new()
                .route("/message", post(post_message))
                .with_state(state),
        )
        .await;
        let response = reqwest::Client::new().post(format!("{url}/message")).json(&json!({"text":"caption","content_parts":[{"type":"image_url","image_url":{"url":"https://fixture/image.png"}}]})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let sid = crate::session_db::session_id_for(Platform::Cli, "local");
        assert!(db.load_history(&sid, 0).unwrap().is_empty());
    }
}
