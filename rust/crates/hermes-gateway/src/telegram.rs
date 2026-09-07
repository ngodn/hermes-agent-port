//! Telegram platform adapter (long polling).
//!
//! The first real [`PlatformAdapter`]: it self-drives inbound via the Telegram
//! Bot API `getUpdates` long-poll loop and delivers outbound via `sendMessage`.
//! This is a focused, from-scratch adapter against the documented HTTP contract
//! (https://core.telegram.org/bots/api), not a port of the ~11k-LOC Python
//! `plugins/platforms/telegram/adapter.py`; the rich features land later.
//!
//! Contract used:
//! - `getUpdates` with `offset` + `timeout`; advance `offset` to
//!   `max(update_id) + 1`. Envelope: `{"ok":true,"result":[Update,...]}`.
//! - Update: `update_id`, optional `message`. Message: `message_id`, `from.id`,
//!   `chat.id`, text or voice/audio plus optional caption.
//! - Audio: `getFile`, bounded download, profile cache, then Dispatcher STT.
//! - `sendMessage` with `chat_id` + `text`.

use std::time::Duration;

use async_trait::async_trait;
use hermes_core::{Error, Message, Platform, Result};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::platform::PlatformAdapter;

/// A parsed Telegram update: the id (to advance the poll offset) and, when the
/// update carried a supported text/audio message, the mapped [`Message`].
#[derive(Debug, PartialEq)]
pub struct ParsedUpdate {
    pub update_id: i64,
    pub message: Option<Message>,
}

/// Extract the update id and a supported [`Message`] from one raw
/// Telegram Update object. Returns `None` only when there is no `update_id`
/// (a malformed update we cannot use to advance the offset).
pub fn extract_update(update: &Value) -> Option<ParsedUpdate> {
    let update_id = update.get("update_id").and_then(Value::as_i64)?;

    // Plain text and voice/audio become turns. Edited messages, channel
    // posts, callbacks etc. still advance the offset but produce no Message.
    let message = update.get("message").and_then(|m| {
        let text = m.get("text").and_then(Value::as_str).or_else(|| {
            audio_attachment(m).map(|_| m.get("caption").and_then(Value::as_str).unwrap_or(""))
        })?;
        let chat = m.get("chat");
        let chat_id = chat.and_then(|c| c.get("id")).and_then(Value::as_i64)?;
        let chat_type = chat
            .and_then(|c| c.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let sender_id = m
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(Value::as_i64)
            // Channel posts have no `from`; fall back to the chat id.
            .unwrap_or(chat_id);
        Some(Message {
            resolved_session_id: None,
            platform: Platform::Telegram,
            channel_id: chat_id.to_string(),
            sender_id: sender_id.to_string(),
            text: text.to_string(),
            content_parts: None,
            chat_type,
            audio_paths: Vec::new(),
            video_paths: Vec::new(),
            workspace_id: None,
            message_id: None,
            thread_id: None,
        })
    });

    Some(ParsedUpdate { update_id, message })
}

/// Telegram long-poll adapter.
pub struct TelegramAdapter {
    token: String,
    api_base: String,
    poll_timeout: Duration,
    client: reqwest::Client,
    audio_home: std::path::PathBuf,
    audio_limit: i64,
}

fn audio_attachment(message: &Value) -> Option<(&str, &'static str)> {
    for (field, ext) in [("voice", ".ogg"), ("audio", ".mp3")] {
        if let Some(id) = message[field]["file_id"].as_str() {
            return Some((id, ext));
        }
    }
    None
}

impl TelegramAdapter {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            // getUpdates blocks up to poll_timeout; give the request headroom.
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Other(format!("telegram: build http client: {e}")))?;
        Ok(Self {
            token: token.into(),
            api_base: "https://api.telegram.org".to_string(),
            poll_timeout: Duration::from_secs(30),
            client,
            audio_home: crate::config_file::hermes_home(),
            audio_limit: 128 * 1024 * 1024,
        })
    }

    pub fn with_audio_cache(mut self, home: std::path::PathBuf, config: &Value) -> Self {
        self.audio_home = home;
        self.audio_limit = crate::audio_process::inbound_limit(config);
        self
    }

    /// Download before handing the message to Dispatcher. Keep token-bearing
    /// API URLs out of diagnostics, including reqwest's embedded request URL.
    async fn prepare_update(&self, raw: &Value) -> Result<Option<ParsedUpdate>> {
        let Some(mut parsed) = extract_update(raw) else {
            return Ok(None);
        };
        if let (Some(message), Some((id, ext))) =
            (&mut parsed.message, audio_attachment(&raw["message"]))
        {
            let result = self.download_audio(id, ext).await;
            match result {
                Ok(path) => message
                    .audio_paths
                    .push(path.to_string_lossy().into_owned()),
                Err(_) => {
                    warn!("telegram audio download failed");
                    if !message.text.is_empty() {
                        message.text.push('\n');
                    }
                    message
                        .text
                        .push_str("[Audio attachment could not be downloaded.]");
                }
            }
        }
        Ok(Some(parsed))
    }

    async fn download_audio(&self, id: &str, ext: &str) -> anyhow::Result<std::path::PathBuf> {
        let envelope: Value = self
            .client
            .post(self.method_url("getFile"))
            .json(&serde_json::json!({"file_id":id}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::ensure!(envelope["ok"] == true, "Telegram file unavailable");
        let path = envelope["result"]["file_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing file path"))?;
        // The API supplies a relative path. Reject traversal and URL delimiters
        // before placing it after the token-bearing download prefix.
        anyhow::ensure!(
            !path.starts_with('/')
                && !path.contains(['?', '#', '\\', '%'])
                && path.split('/').all(|part| !matches!(part, ".." | "." | "")),
            "invalid file path"
        );
        let url = format!("{}/file/bot{}/{path}", self.api_base, self.token);
        let response = self.client.get(url).send().await?;
        crate::audio_process::cache_audio_response(
            &self.audio_home,
            response,
            ext,
            self.audio_limit,
        )
        .await
    }

    /// Override the API base (for tests or a proxy).
    #[allow(dead_code)]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.token, method)
    }

    async fn get_updates(&self, offset: i64) -> Result<Vec<Value>> {
        let url = self.method_url("getUpdates");
        let body = serde_json::json!({
            "offset": offset,
            "timeout": self.poll_timeout.as_secs(),
            "allowed_updates": ["message"],
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("telegram getUpdates: {e}")))?;
        let envelope: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("telegram getUpdates decode: {e}")))?;
        if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Other(format!(
                "telegram getUpdates not ok: {}",
                envelope
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )));
        }
        Ok(envelope
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn run(&self, inbound: mpsc::Sender<Message>) -> Result<()> {
        // Telegram remembers the offset server-side until confirmed, so we start
        // from 0 and advance to max(update_id)+1 as updates arrive.
        let mut offset: i64 = 0;
        loop {
            match self.get_updates(offset).await {
                Ok(updates) => {
                    for raw in &updates {
                        let Some(parsed) = self.prepare_update(raw).await? else {
                            continue;
                        };
                        // Confirm this update by moving the offset past it.
                        offset = offset.max(parsed.update_id + 1);
                        if let Some(msg) = parsed.message {
                            if inbound.send(msg).await.is_err() {
                                // Dispatcher gone: nothing consumes inbound.
                                debug!("telegram: inbound channel closed, stopping");
                                return Ok(());
                            }
                        }
                    }
                }
                Err(err) => {
                    // Back off on transient errors rather than hot-looping.
                    warn!(%err, "telegram getUpdates failed; backing off");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    }

    async fn send(&self, msg: &Message) -> Result<()> {
        let url = self.method_url("sendMessage");
        let body = serde_json::json!({
            "chat_id": msg.channel_id,
            "text": msg.text,
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("telegram sendMessage: {e}")))?;
        let envelope: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("telegram sendMessage decode: {e}")))?;
        if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Other(format!(
                "telegram sendMessage not ok: {}",
                envelope
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct RecordingAgent(mpsc::Sender<String>);
    #[async_trait]
    impl crate::agent::AgentClient for RecordingAgent {
        async fn run_turn(
            &self,
            msg: &Message,
            _: &[crate::session_db::HistoryMessage],
            _: mpsc::Sender<hermes_core::StreamEvent>,
        ) -> Result<()> {
            self.0.send(msg.text.clone()).await.unwrap();
            Ok(())
        }
    }

    struct LocalAudioContext;
    #[async_trait]
    impl crate::transcription_enrichment::TranscriptionBackend for LocalAudioContext {
        fn absolute_path(&self, path: &str) -> String {
            path.into()
        }
        async fn probe_duration(&self, _: &str) -> Option<String> {
            None
        }
        async fn transcribe(&self, _: &str) -> anyhow::Result<Value> {
            unreachable!("HTTP backend owns transcription")
        }
        async fn local_fallback(&self, _: &str) -> anyhow::Result<Value> {
            Ok(json!({"success":false}))
        }
        fn agent_visible_path(&self, path: &str) -> String {
            path.into()
        }
    }

    #[tokio::test]
    async fn voice_download_populates_cached_audio_and_preserves_failed_caption() {
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let app = Router::new()
            .route(
                "/botfixture/getFile",
                post(|Json(body): Json<Value>| async move {
                    let path = if body["file_id"] == "bad" {
                        "../escape"
                    } else {
                        "voice/clip.oga"
                    };
                    Json(json!({"ok":true,"result":{"file_path":path}}))
                }),
            )
            .route(
                "/file/botfixture/voice/clip.oga",
                get(|| async { b"OggSfixture".to_vec() }),
            )
            .route(
                "/audio/transcriptions",
                post(|body: axum::body::Bytes| async move {
                    assert!(body.windows(11).any(|part| part == b"OggSfixture"));
                    "spoken fixture transcript"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let home = std::env::temp_dir().join(format!(
            "hermes-telegram-audio-{}",
            crate::install_identity::mint_id().unwrap()
        ));
        // Existing legacy cache must remain the selected profile layout.
        tokio::fs::create_dir_all(home.join("audio_cache"))
            .await
            .unwrap();
        tokio::fs::write(home.join("audio_cache/existing.ogg"), b"OggSold")
            .await
            .unwrap();
        let adapter = TelegramAdapter::new("fixture")
            .unwrap()
            .with_api_base(base.clone())
            .with_audio_cache(home.clone(), &json!({}));
        for field in ["voice", "audio"] {
            let mut raw = json!({"update_id":3,"message":{"chat":{"id":42},"caption":"caption"}});
            raw["message"][field] = json!({"file_id":"clip"});
            let message = adapter
                .prepare_update(&raw)
                .await
                .unwrap()
                .unwrap()
                .message
                .unwrap();
            assert_eq!(message.text, "caption");
            assert_eq!(message.audio_paths.len(), 1);
            let path = std::path::Path::new(&message.audio_paths[0]);
            assert_eq!(path.parent().unwrap(), home.join("audio_cache"));
            assert_eq!(path.extension().unwrap(), "ogg");
            assert_eq!(tokio::fs::read(path).await.unwrap(), b"OggSfixture");
            // Exercise the actual downloaded bytes through HTTP STT and the
            // Dispatcher queue. Only the final model is a recording boundary.
            let transport = crate::transcription_http::TranscriptionHttp::new(
                &base,
                "fixture-key".into(),
                "openai".into(),
                "whisper-1".into(),
                "whisper-1",
                None,
                None,
            )
            .unwrap();
            let backend = crate::transcription_http::HttpTranscriptionBackend {
                transport,
                read_policy: crate::file_read_safety::FileReadPolicy {
                    home: home.clone(),
                    cwd: home.clone(),
                    hermes_home: home.clone(),
                    hermes_root: home.clone(),
                },
                context: LocalAudioContext,
            };
            let (seen_tx, mut seen_rx) = mpsc::channel(1);
            let dispatcher = std::sync::Arc::new(
                crate::dispatch::Dispatcher::new(
                    std::sync::Arc::new(RecordingAgent(seen_tx)),
                    std::sync::Arc::new(json!({})),
                    None,
                )
                .with_transcription(std::sync::Arc::new(backend)),
            );
            let (in_tx, in_rx) = mpsc::channel(1);
            let worker = tokio::spawn(dispatcher.run(in_rx));
            in_tx.send(message).await.unwrap();
            let text = tokio::time::timeout(Duration::from_secs(10), seen_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(text.contains("spoken fixture transcript"), "{text}");
            assert!(text.contains("caption"));
            drop(in_tx);
            worker.await.unwrap();
            raw["message"][field]["file_id"] = json!("bad");
            let message = adapter
                .prepare_update(&raw)
                .await
                .unwrap()
                .unwrap()
                .message
                .unwrap();
            assert!(message.audio_paths.is_empty());
            assert!(message.text.starts_with("caption\n"));
            assert!(!message.text.contains("fixture"));
        }
        let adapter = adapter.with_audio_cache(
            home.clone(),
            &json!({"gateway":{"max_inbound_media_bytes":3}}),
        );
        let raw = json!({"update_id":4,"message":{"chat":{"id":42},"voice":{"file_id":"clip"}}});
        let message = adapter
            .prepare_update(&raw)
            .await
            .unwrap()
            .unwrap()
            .message
            .unwrap();
        assert!(message.audio_paths.is_empty());
        assert!(message.text.contains("could not be downloaded"));
        server.abort();
        tokio::fs::remove_dir_all(home).await.unwrap();
    }

    #[test]
    fn extracts_text_message() {
        let update = json!({
            "update_id": 42,
            "message": {
                "message_id": 7,
                "from": {"id": 111},
                "chat": {"id": 222},
                "text": "hello"
            }
        });
        let parsed = extract_update(&update).unwrap();
        assert_eq!(parsed.update_id, 42);
        let msg = parsed.message.unwrap();
        assert_eq!(msg.platform, Platform::Telegram);
        assert_eq!(msg.channel_id, "222");
        assert_eq!(msg.sender_id, "111");
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn non_text_update_still_advances_offset() {
        // An update with no text message (e.g. a sticker) yields no Message but
        // must still carry its update_id so the poll offset advances past it.
        let update = json!({
            "update_id": 99,
            "message": {"message_id": 1, "from": {"id": 5}, "chat": {"id": 6}}
        });
        let parsed = extract_update(&update).unwrap();
        assert_eq!(parsed.update_id, 99);
        assert!(parsed.message.is_none());
    }

    #[test]
    fn channel_post_without_from_falls_back_to_chat_id() {
        let update = json!({
            "update_id": 3,
            "message": {"chat": {"id": 777}, "text": "hi"}
        });
        let msg = extract_update(&update).unwrap().message.unwrap();
        assert_eq!(msg.sender_id, "777");
        assert_eq!(msg.channel_id, "777");
    }

    #[test]
    fn missing_update_id_is_unusable() {
        assert_eq!(extract_update(&json!({"message": {}})), None);
    }

    #[test]
    fn method_url_is_well_formed() {
        let a = TelegramAdapter::new("T0KEN").unwrap();
        assert_eq!(
            a.method_url("getUpdates"),
            "https://api.telegram.org/botT0KEN/getUpdates"
        );
    }
}
