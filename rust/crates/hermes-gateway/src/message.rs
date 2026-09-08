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
    let mut msg = Message {
        resolved_session_id: None,
        platform: Platform::Cli,
        channel_id: req.channel_id,
        sender_id: req.sender_id,
        text: req.text,
        content_parts: req.content_parts,
        chat_type: Some("dm".to_string()),
        audio_paths: Vec::new(),
        video_paths: Vec::new(),
        workspace_id: None,
        message_id: None,
        thread_id: None,
    };

    // A confirmation reply is control input, not a model turn. Intercept it
    // before ordinary slash dispatch, while authorizing against the destructive
    // command that created the prompt. Native tool approvals are not live yet;
    // once ported, their blocking predicate must replace this explicit false.
    if let Some((store, _)) = &state.session_store {
        let deps = crate::session_admission::AdmissionDeps {
            store: store.clone(),
            transcript_leases: state.turn_leases.clone(),
            route_leases: state.route_leases.clone(),
            generation: state.turn_generation.clone(),
        };
        if let Some(result) = crate::session_commands::resolve_reset_confirmation(
            &state.slash_confirmations,
            deps,
            state.agent.clone(),
            crate::session::source_from_message(&msg),
            &msg,
            &state.user_config,
            false,
        )
        .await
        {
            return Ok(Json(MessageResponse {
                reply: result.reply,
            }));
        }
    }

    // Slash-command gating + built-ins, same policy as the push path.
    let mut native_command = None;
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
            native_command = crate::slash::native_command(&command, &msg.text);
            if let Some(crate::slash::NativeSlashCommand::Unavailable { reply }) = &native_command {
                return Ok(Json(MessageResponse {
                    reply: reply.clone(),
                }));
            }
        }
        crate::slash::SlashDecision::NotSlash => {}
    }

    if let Some(crate::slash::NativeSlashCommand::Reset { title }) = native_command {
        let Some((store, _)) = &state.session_store else {
            return Ok(Json(MessageResponse {
                reply: "Session reset is not available on this backend.".into(),
            }));
        };
        let result = crate::session_commands::reset_or_confirm(
            &state.slash_confirmations,
            crate::session_admission::AdmissionDeps {
                store: store.clone(),
                transcript_leases: state.turn_leases.clone(),
                route_leases: state.route_leases.clone(),
                generation: state.turn_generation.clone(),
            },
            state.agent.clone(),
            crate::session::source_from_message(&msg),
            &msg.sender_id,
            title,
            crate::slash::typed_command_prefix(msg.platform),
        )
        .await
        .map_err(|error| {
            warn!(%error, "HTTP session reset failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error":"session reset failed"})),
            )
        })?;
        return Ok(Json(MessageResponse {
            reply: result.reply,
        }));
    }

    // Load prior history + record the inbound message for stateless backends.
    let manages = state.agent.manages_history();
    let mut turn_db = state.session_db.clone();
    let mut routing_key = None;
    let mut session_finalizable = false;
    let mut admitted_lease = None;
    if !manages {
        if let Some((store, freshness)) = &state.session_store {
            let source = crate::session::source_from_message(&msg);
            let legacy_id = crate::session_db::message_session_id(&msg);
            let resolved = crate::session_admission::admit_turn(
                crate::session_admission::AdmissionDeps {
                    store: store.clone(),
                    transcript_leases: state.turn_leases.clone(),
                    route_leases: state.route_leases.clone(),
                    generation: state.turn_generation.clone(),
                },
                source,
                Some((legacy_id, "cli".into())),
                *freshness,
                &msg.sender_id,
            )
            .await
            .map_err(|error| {
                warn!(%error, "could not resolve HTTP session");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error":"session unavailable"})),
                )
            })?;
            msg.resolved_session_id = Some(resolved.entry.session_id);
            routing_key = Some(resolved.entry.session_key);
            turn_db = resolved.database;
            session_finalizable = resolved.finalizable;
            admitted_lease = resolved.lease;
            if let Some(previous_session_id) = resolved.predecessor_id {
                if let Some(route_key) = routing_key.as_deref() {
                    state.slash_confirmations.clear(route_key);
                }
                state.agent.retire_conversation(
                    crate::agent::TurnContext::from_database(turn_db.as_deref()),
                    &previous_session_id,
                );
            }
        }
    }

    // Serialize before history reads. The same registry is passed to push
    // dispatchers, so routes that resolve to one transcript cannot interleave.
    let session_id = crate::session_db::message_session_id(&msg);
    let _lease = if admitted_lease.is_some() {
        admitted_lease
    } else {
        let generation = state
            .turn_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state
            .turn_leases
            .acquire(
                &session_id,
                routing_key.as_deref().unwrap_or(&msg.sender_id),
                generation,
                None,
            )
            .await
            .map_err(|error| {
                warn!(%error, "HTTP turn could not acquire session lease");
                (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"error":"session is busy"})),
                )
            })?
    };
    // Once admitted, the turn owns the lease and persistence independently of
    // the HTTP waiter. Dropping a client request must not detach a live agent
    // from its transcript lock or discard its completed assistant message.
    tokio::spawn(async move {
        let _turn_lease = _lease;
        let history = crate::session_db::begin_turn(turn_db.as_deref(), manages, &msg, "cli");

        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
        let agent = state.agent.clone();
        let turn_agent = agent.clone();
        let agent_db = turn_db.clone();
        let msg_for_agent = msg.clone();
        let turn = tokio::spawn(async move {
            turn_agent
                .run_turn_with_context(
                    crate::agent::TurnContext::from_database(agent_db.as_deref())
                        .with_session_finalizable(session_finalizable),
                    &msg_for_agent,
                    &history,
                    tx,
                )
                .await
        });

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
        // The producer may still be finishing after its terminal stream event.
        // Keep ownership until it has stopped, including on error paths.
        let outcome = turn.await;
        let succeeded = matches!(&outcome, Ok(Ok(())));
        crate::session_db::end_turn(turn_db.as_deref(), manages, &msg, &reply);
        if let Err(error) = agent
            .finalize_turn_after_persist(
                crate::agent::TurnContext::from_database(turn_db.as_deref())
                    .with_session_finalizable(session_finalizable),
                &msg,
                &reply,
                succeeded,
            )
            .await
        {
            warn!(%error, "HTTP agent post-persist finalization failed");
        }
        if let (Some(key), Some((store, _))) = (routing_key, &state.session_store) {
            let store = store.clone();
            match tokio::task::spawn_blocking(move || store.update_session(&key, None, true)).await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(%error, "HTTP session activity update failed"),
                Err(error) => warn!(%error, "HTTP session activity worker failed"),
            }
        }

        // Intentional-silence markers suppress delivery: return an empty reply
        // rather than echoing "NO_REPLY" to the caller.
        if crate::response_filters::is_intentional_silence_response(&reply) {
            reply.clear();
        }

        match outcome {
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
    })
    .await
    .map_err(|error| {
        warn!(%error, "HTTP turn owner failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"turn owner failed"})),
        )
    })?
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
    #[async_trait::async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "test echo".into(),
                parameters: json!({"type":"object"}),
                extra: Default::default(),
            }
        }
        async fn call(&self, _: &Value) -> hermes_core::Result<Value> {
            Ok(json!("done"))
        }
    }

    async fn roundtrip(with_tools: bool, coordinated: bool) {
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
        let mut state = AppState::new(Arc::new(agent), Arc::new(json!({})), None, Some(db.clone()));
        let routing_config = crate::config_gateway::GatewayConfig {
            sessions_dir: home.0.join("sessions"),
            ..Default::default()
        };
        if coordinated {
            let store = crate::session_store::SessionStore::open(
                routing_config.clone(),
                home.0.clone(),
                home.0.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap();
            state.session_store = Some((Arc::new(store), 3600.0));
        }
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
        let sid = if coordinated {
            // Reconstruct routing from disk and replay the durable transcript.
            // This catches HTTP writes accidentally falling back to cli:picture.
            let store = crate::session_store::SessionStore::open(
                routing_config,
                home.0.clone(),
                home.0.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap();
            let mut source = crate::session::SessionSource::new("local", "picture");
            source.user_id = Some("local".into());
            let entry = store
                .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
                .unwrap();
            assert!(reopened.get_session("cli:picture").unwrap().is_none());
            entry.session_id
        } else {
            crate::session_db::session_id_for(Platform::Cli, "picture")
        };
        let history = reopened.load_history(&sid, 0).unwrap();
        assert_eq!(history[0].model_content(), json!(parts));
        assert_eq!(history.len(), 4);
    }

    #[tokio::test]
    async fn images_reach_streaming_model_and_survive_next_turn() {
        roundtrip(false, false).await;
    }
    #[tokio::test]
    async fn images_survive_tool_rounds_and_history_replay() {
        roundtrip(true, false).await;
    }

    #[tokio::test]
    async fn coordinated_http_history_survives_routing_reload() {
        roundtrip(true, true).await;
    }

    #[tokio::test]
    async fn http_adopts_existing_rust_transcript_before_calling_model() {
        let home = TempHome::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (model_url, _model_server) = serve(
            axum::Router::new()
                .route("/chat/completions", post(model))
                .with_state(calls.clone()),
        )
        .await;
        let agent = NativeAgentClient::new("fixture-model", "fixture-key", model_url).unwrap();
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        db.ensure_session("cli:legacy", "cli", None, Some("legacy"), Some("dm"))
            .unwrap();
        db.append_message("cli:legacy", "user", "previous question")
            .unwrap();
        db.append_message("cli:legacy", "assistant", "previous answer")
            .unwrap();
        let store = crate::session_store::SessionStore::open(
            crate::config_gateway::GatewayConfig {
                sessions_dir: home.0.join("sessions"),
                ..Default::default()
            },
            home.0.clone(),
            home.0.clone(),
            "default".into(),
            |_| Ok(false),
        )
        .unwrap();
        let expected_home = home.0.clone();
        let selections = Arc::new(Mutex::new(0usize));
        let recorded_selections = selections.clone();
        let selected = Arc::new(agent);
        let fallback = selected.clone();
        let routed = crate::conversation_agent::ConversationAgent::new(
            fallback,
            move |home, message, history, database| {
                let selected = selected.clone();
                let expected_home = expected_home.clone();
                let recorded_selections = recorded_selections.clone();
                Box::pin(async move {
                    assert_eq!(home, expected_home);
                    assert!(message.resolved_session_id.is_some());
                    assert_eq!(history.len(), 2);
                    assert!(database
                        .unwrap()
                        .get_session("cli:legacy")
                        .unwrap()
                        .is_some());
                    *recorded_selections.lock().unwrap() += 1;
                    Ok(selected as Arc<dyn crate::agent::AgentClient>)
                })
            },
            crate::agent_cache_pressure::AgentCacheBounds::default(),
        );
        let mut state = AppState::new(
            Arc::new(routed),
            Arc::new(json!({})),
            None,
            Some(db.clone()),
        );
        state.session_store = Some((Arc::new(store), 3600.0));
        let (url, _gateway_server) = serve(
            axum::Router::new()
                .route("/message", post(post_message))
                .with_state(state),
        )
        .await;
        let reply = reqwest::Client::new()
            .post(format!("{url}/message"))
            .json(&json!({"channel_id":"legacy","text":"follow-up"}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(reply.json::<Value>().await.unwrap()["reply"], "seen");
        assert_eq!(*selections.lock().unwrap(), 1);
        let requests = calls.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["messages"][0]["content"], "previous question");
        assert_eq!(requests[0]["messages"][1]["content"], "previous answer");
        assert_eq!(requests[0]["messages"][2]["content"], "follow-up");
        assert_eq!(db.load_history("cli:legacy", 0).unwrap().len(), 4);
        assert!(db.get_session("cli:legacy").unwrap().unwrap()["session_key"].is_string());
    }

    #[tokio::test]
    async fn cancelled_http_waiter_keeps_lease_until_agent_and_flush_finish() {
        struct FinishingAgent {
            entered: tokio::sync::Notify,
            finish: tokio::sync::Notify,
        }
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for FinishingAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                events: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                events
                    .send(StreamEvent::MessageChunk {
                        text: "completed answer".into(),
                    })
                    .await
                    .unwrap();
                events
                    .send(StreamEvent::MessageStop { final_: true })
                    .await
                    .unwrap();
                self.entered.notify_one();
                // Terminal text is not proof that the agent has stopped.
                self.finish.notified().await;
                Ok(())
            }
        }
        let home = TempHome::new();
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        let agent = Arc::new(FinishingAgent {
            entered: tokio::sync::Notify::new(),
            finish: tokio::sync::Notify::new(),
        });
        let state = AppState::new(agent.clone(), Arc::new(json!({})), None, Some(db.clone()));
        let request = tokio::spawn(post_message(
            State(state.clone()),
            Json(MessageRequest {
                channel_id: "cancelled".into(),
                sender_id: "local".into(),
                text: "question".into(),
                content_parts: None,
            }),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), agent.entered.notified())
            .await
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(state
            .turn_leases
            .acquire(
                "cli:cancelled",
                "next-route",
                2,
                Some(std::time::Duration::from_millis(50))
            )
            .await
            .is_err());
        agent.finish.notify_one();
        let _next = state
            .turn_leases
            .acquire("cli:cancelled", "next-route", 3, None)
            .await
            .unwrap();
        let history = db.load_history("cli:cancelled", 0).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "completed answer");
    }

    #[tokio::test]
    async fn http_turn_waits_for_shared_transcript_lease() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (model_url, _server) = serve(
            axum::Router::new()
                .route("/chat/completions", post(model))
                .with_state(calls.clone()),
        )
        .await;
        let agent = NativeAgentClient::new("fixture-model", "fixture-key", model_url).unwrap();
        let state = AppState::new(Arc::new(agent), Arc::new(json!({})), None, None);
        let held = state
            .turn_leases
            .acquire("cli:shared", "push-route", 1, None)
            .await
            .unwrap();
        let request = post_message(
            State(state),
            Json(MessageRequest {
                channel_id: "shared".into(),
                sender_id: "local".into(),
                text: "hello".into(),
                content_parts: None,
            }),
        );
        tokio::pin!(request);
        // Poll the handler while another ingress owns the transcript. It must
        // not reach the model until that owner releases its lease.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut request)
                .await
                .is_err()
        );
        assert!(calls.lock().unwrap().is_empty());
        drop(held);
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.0.reply, "seen");
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn automatic_http_reset_retires_the_previous_cached_conversation() {
        struct LifecycleAgent {
            ended: Arc<Mutex<Vec<Vec<Value>>>>,
        }
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for LifecycleAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                events: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                events
                    .send(StreamEvent::MessageChunk {
                        text: "answer".into(),
                    })
                    .await
                    .unwrap();
                events
                    .send(StreamEvent::MessageStop { final_: true })
                    .await
                    .unwrap();
                Ok(())
            }

            async fn close_conversation(
                &self,
                messages: Option<&[Value]>,
            ) -> hermes_core::Result<()> {
                self.ended
                    .lock()
                    .unwrap()
                    .push(messages.unwrap_or_default().to_vec());
                Ok(())
            }
        }

        let home = TempHome::new();
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                crate::config_gateway::GatewayConfig {
                    sessions_dir: home.0.join("sessions"),
                    default_reset_policy: crate::config_types::SessionResetPolicy {
                        mode: json!("idle"),
                        idle_minutes: json!(0),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                home.0.clone(),
                home.0.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let builds = Arc::new(Mutex::new(0usize));
        let ended = Arc::new(Mutex::new(Vec::new()));
        let fallback = Arc::new(LifecycleAgent {
            ended: ended.clone(),
        });
        let cache = Arc::new(crate::conversation_agent::ConversationAgent::new(
            fallback,
            {
                let builds = builds.clone();
                let ended = ended.clone();
                move |_, _, _, _| {
                    let builds = builds.clone();
                    let ended = ended.clone();
                    Box::pin(async move {
                        *builds.lock().unwrap() += 1;
                        Ok(Arc::new(LifecycleAgent { ended })
                            as Arc<dyn crate::agent::AgentClient>)
                    })
                }
            },
            crate::agent_cache_pressure::AgentCacheBounds::default(),
        ));
        let mut state = AppState::new(cache, Arc::new(json!({})), None, Some(db));
        state.session_store = Some((store, 3600.0));
        let request = || {
            post_message(
                State(state.clone()),
                Json(MessageRequest {
                    channel_id: "reset".into(),
                    sender_id: "local".into(),
                    text: "question".into(),
                    content_parts: None,
                }),
            )
        };
        assert_eq!(request().await.unwrap().0.reply, "answer");
        assert_eq!(request().await.unwrap().0.reply, "answer");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !ended.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*builds.lock().unwrap(), 2);
        let ended = ended.lock().unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].len(), 2);
        assert_eq!(ended[0][0]["role"], "user");
        assert_eq!(ended[0][1]["role"], "assistant");
    }

    #[tokio::test]
    async fn explicit_http_reset_rotates_history_and_retires_the_old_client() {
        struct RecordingAgent {
            turns: Arc<Mutex<Vec<(String, usize, String)>>>,
            ended: Arc<Mutex<Vec<Vec<Value>>>>,
        }
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for RecordingAgent {
            async fn run_turn(
                &self,
                message: &Message,
                history: &[crate::session_db::HistoryMessage],
                events: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                self.turns.lock().unwrap().push((
                    message.resolved_session_id.clone().unwrap(),
                    history.len(),
                    message.text.clone(),
                ));
                events
                    .send(StreamEvent::MessageChunk {
                        text: "answer".into(),
                    })
                    .await
                    .unwrap();
                events
                    .send(StreamEvent::MessageStop { final_: true })
                    .await
                    .unwrap();
                Ok(())
            }

            async fn close_conversation(
                &self,
                messages: Option<&[Value]>,
            ) -> hermes_core::Result<()> {
                self.ended
                    .lock()
                    .unwrap()
                    .push(messages.unwrap_or_default().to_vec());
                Ok(())
            }
        }

        let home = TempHome::new();
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                crate::config_gateway::GatewayConfig {
                    sessions_dir: home.0.join("sessions"),
                    ..Default::default()
                },
                home.0.clone(),
                home.0.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let turns = Arc::new(Mutex::new(Vec::new()));
        let ended = Arc::new(Mutex::new(Vec::new()));
        let fallback = Arc::new(RecordingAgent {
            turns: turns.clone(),
            ended: ended.clone(),
        });
        let cache = Arc::new(crate::conversation_agent::ConversationAgent::new(
            fallback,
            {
                let turns = turns.clone();
                let ended = ended.clone();
                move |_, _, _, _| {
                    let turns = turns.clone();
                    let ended = ended.clone();
                    Box::pin(async move {
                        Ok(Arc::new(RecordingAgent { turns, ended })
                            as Arc<dyn crate::agent::AgentClient>)
                    })
                }
            },
            crate::agent_cache_pressure::AgentCacheBounds::default(),
        ));
        let mut state = AppState::new(cache, Arc::new(json!({})), None, Some(db.clone()));
        state.slash_confirmations = Arc::new(crate::slash_confirm::SlashConfirmations::new(
            home.0.join("config.yaml"),
        ));
        state.session_store = Some((store.clone(), 3600.0));
        let (url, _server) = serve(
            axum::Router::new()
                .route("/message", post(post_message))
                .with_state(state),
        )
        .await;
        let client = reqwest::Client::new();
        let send = |text: &'static str| {
            client
                .post(format!("{url}/message"))
                .json(&json!({"channel_id":"explicit","sender_id":"local","text":text}))
                .send()
        };
        let first_reply = send("first").await.unwrap();
        assert_eq!(first_reply.status(), StatusCode::OK);
        assert_eq!(
            first_reply.json::<Value>().await.unwrap()["reply"],
            "answer"
        );
        let source = crate::session::SessionSource {
            user_id: Some("local".into()),
            ..crate::session::SessionSource::new("local", "explicit")
        };
        let first_id = store.current_entry_for_source(&source).unwrap().session_id;

        let reset_reply = send("/reset").await.unwrap();
        assert_eq!(reset_reply.status(), StatusCode::OK);
        assert!(reset_reply.json::<Value>().await.unwrap()["reply"]
            .as_str()
            .unwrap()
            .contains("Confirm /new"));
        assert_eq!(turns.lock().unwrap().len(), 1);
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            first_id
        );

        let reset_reply = send("/approve").await.unwrap();
        assert_eq!(reset_reply.status(), StatusCode::OK);
        assert!(reset_reply.json::<Value>().await.unwrap()["reply"]
            .as_str()
            .unwrap()
            .contains("Session reset"));
        let second_id = store.current_entry_for_source(&source).unwrap().session_id;
        assert_ne!(second_id, first_id);

        let second_reply = send("second").await.unwrap();
        assert_eq!(second_reply.status(), StatusCode::OK);
        assert_eq!(
            second_reply.json::<Value>().await.unwrap()["reply"],
            "answer"
        );
        {
            let turns = turns.lock().unwrap();
            assert_eq!(
                turns.as_slice(),
                [
                    (first_id.clone(), 0, "first".into()),
                    (second_id.clone(), 0, "second".into()),
                ]
            );
        }
        let first_row = db.get_session(&first_id).unwrap().unwrap();
        assert_eq!(first_row["end_reason"], "session_reset");
        let second_row = db.get_session(&second_id).unwrap().unwrap();
        assert_eq!(second_row["parent_session_id"], first_id);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while ended.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let ended = ended.lock().unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].len(), 2);
    }

    #[tokio::test]
    async fn http_always_approve_persists_and_the_next_reset_skips_live() {
        struct NeverRun;
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for NeverRun {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                _: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                panic!("confirmation control message reached the model")
            }
        }

        let home = TempHome::new();
        let config_path = home.0.join("config.yaml");
        std::fs::write(
            &config_path,
            "# keep this comment\napprovals:\n  destructive_slash_confirm: true\n",
        )
        .unwrap();
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                crate::config_gateway::GatewayConfig {
                    sessions_dir: home.0.join("sessions"),
                    ..Default::default()
                },
                home.0.clone(),
                home.0.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let mut state = AppState::new(Arc::new(NeverRun), Arc::new(json!({})), None, Some(db));
        state.slash_confirmations = Arc::new(crate::slash_confirm::SlashConfirmations::new(
            config_path.clone(),
        ));
        state.session_store = Some((store.clone(), 3600.0));
        let (url, _server) = serve(
            axum::Router::new()
                .route("/message", post(post_message))
                .with_state(state),
        )
        .await;
        let client = reqwest::Client::new();
        let send = |text: &'static str| {
            client
                .post(format!("{url}/message"))
                .json(&json!({"channel_id":"always","sender_id":"local","text":text}))
                .send()
        };

        let prompt = send("/new").await.unwrap().json::<Value>().await.unwrap();
        assert!(prompt["reply"].as_str().unwrap().contains("Confirm /new"));
        let source = crate::session::SessionSource {
            user_id: Some("local".into()),
            ..crate::session::SessionSource::new("local", "always")
        };
        assert!(store.current_entry_for_source(&source).is_none());

        let cancelled = send("/cancel")
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(
            cancelled["reply"],
            "🟡 /new cancelled. Conversation unchanged."
        );
        assert!(store.current_entry_for_source(&source).is_none());

        let prompt = send("/new").await.unwrap().json::<Value>().await.unwrap();
        assert!(prompt["reply"].as_str().unwrap().contains("Confirm /new"));

        let approved = send("/always")
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert!(approved["reply"]
            .as_str()
            .unwrap()
            .contains("will run without confirmation"));
        let first_id = store.current_entry_for_source(&source).unwrap().session_id;
        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(written.contains("# keep this comment"));
        assert!(written.contains("destructive_slash_confirm: false"));

        let immediate = send("/new").await.unwrap().json::<Value>().await.unwrap();
        assert!(immediate["reply"]
            .as_str()
            .unwrap()
            .contains("Session reset"));
        assert!(!immediate["reply"]
            .as_str()
            .unwrap()
            .contains("Confirm /new"));
        assert_ne!(
            store.current_entry_for_source(&source).unwrap().session_id,
            first_id
        );
    }

    #[tokio::test]
    async fn explicit_http_reset_waits_for_in_flight_persistence() {
        struct GateAgent {
            entered: tokio::sync::Notify,
            release: tokio::sync::Semaphore,
        }
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for GateAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                events: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
                events
                    .send(StreamEvent::MessageChunk {
                        text: "persisted".into(),
                    })
                    .await
                    .unwrap();
                events
                    .send(StreamEvent::MessageStop { final_: true })
                    .await
                    .unwrap();
                Ok(())
            }
        }

        let home = TempHome::new();
        let db = Arc::new(crate::session_db::SessionDb::open(home.0.join("state.db")).unwrap());
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                crate::config_gateway::GatewayConfig {
                    sessions_dir: home.0.join("sessions"),
                    ..Default::default()
                },
                home.0.clone(),
                home.0.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let agent = Arc::new(GateAgent {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let mut state = AppState::new(agent.clone(), Arc::new(json!({})), None, Some(db.clone()));
        state.slash_confirmations = Arc::new(crate::slash_confirm::SlashConfirmations::new(
            home.0.join("config.yaml"),
        ));
        state.session_store = Some((store.clone(), 3600.0));
        let first = tokio::spawn(post_message(
            State(state.clone()),
            Json(MessageRequest {
                channel_id: "reset-race".into(),
                sender_id: "local".into(),
                text: "first".into(),
                content_parts: None,
            }),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), agent.entered.notified())
            .await
            .unwrap();
        let source = crate::session::SessionSource {
            user_id: Some("local".into()),
            ..crate::session::SessionSource::new("local", "reset-race")
        };
        let first_id = store.current_entry_for_source(&source).unwrap().session_id;
        let prompt = post_message(
            State(state.clone()),
            Json(MessageRequest {
                channel_id: "reset-race".into(),
                sender_id: "local".into(),
                text: "/new".into(),
                content_parts: None,
            }),
        )
        .await
        .unwrap();
        assert!(prompt.0.reply.contains("Confirm /new"));
        let reset = tokio::spawn(post_message(
            State(state),
            Json(MessageRequest {
                channel_id: "reset-race".into(),
                sender_id: "local".into(),
                text: "/approve".into(),
                content_parts: None,
            }),
        ));
        tokio::pin!(reset);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut reset)
                .await
                .is_err()
        );
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            first_id
        );

        agent.release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap().0.reply, "persisted");
        let reset_reply = tokio::time::timeout(std::time::Duration::from_secs(5), reset)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(reset_reply.0.reply.contains("Session reset"));
        assert_ne!(
            store.current_entry_for_source(&source).unwrap().session_id,
            first_id
        );
        let history = db.load_history(&first_id, 0).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "persisted");
        assert_eq!(
            db.get_session(&first_id).unwrap().unwrap()["end_reason"],
            "session_reset"
        );
    }

    #[tokio::test]
    async fn unported_lifecycle_commands_never_reach_the_agent() {
        struct NeverRun;
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for NeverRun {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                _: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                panic!("native lifecycle command reached the model")
            }
        }
        let state = AppState::new(Arc::new(NeverRun), Arc::new(json!({})), None, None);
        for text in ["/compress", "/compact --preview", "/resume", "/sessions"] {
            let response = post_message(
                State(state.clone()),
                Json(MessageRequest {
                    channel_id: "commands".into(),
                    sender_id: "local".into(),
                    text: text.into(),
                    content_parts: None,
                }),
            )
            .await
            .unwrap();
            assert!(response.0.reply.contains("not available"));
        }
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
