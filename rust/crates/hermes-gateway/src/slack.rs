//! Slack platform adapter (Socket Mode).
//!
//! The third real [`PlatformAdapter`]. Slack Socket Mode delivers events over a
//! WebSocket whose URL is minted at runtime: POST `apps.connections.open` with
//! the app-level token to get a `wss://` URL, connect, receive `hello`, then
//! handle `events_api` envelopes, acking each by `envelope_id`. Outbound uses
//! `chat.postMessage` with the bot token. Built from the documented contract
//! (https://docs.slack.dev/apis/events-api/using-socket-mode), not a port of
//! the Python adapter.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hermes_core::{Error, Message, Platform, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::platform::PlatformAdapter;

const API_BASE: &str = "https://slack.com/api";

/// A decoded Socket Mode envelope.
#[derive(Debug, PartialEq)]
pub struct Envelope {
    /// Present on messages that must be acknowledged.
    pub envelope_id: Option<String>,
    /// "hello", "events_api", "disconnect", ...
    pub kind: String,
    pub payload: Value,
}

/// Decode a Socket Mode frame into an [`Envelope`].
pub fn parse_envelope(v: &Value) -> Option<Envelope> {
    let kind = v.get("type").and_then(Value::as_str)?.to_string();
    Some(Envelope {
        envelope_id: v
            .get("envelope_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        kind,
        payload: v.get("payload").cloned().unwrap_or(Value::Null),
    })
}

/// The ack frame for an envelope id.
pub fn ack_payload(envelope_id: &str) -> Value {
    json!({ "envelope_id": envelope_id })
}

/// Map an `events_api` payload to a [`Message`].
///
/// Handles only plain user messages: events with a `bot_id` or a `subtype`
/// (edits, joins, bot posts) are skipped to avoid loops and noise. `channel_type`
/// "im" is a DM; anything else is a group/channel, for access scope.
pub fn parse_message_event(payload: &Value) -> Option<Message> {
    let event = payload.get("event")?;
    if event.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    if event.get("bot_id").is_some() || event.get("subtype").is_some() {
        return None;
    }
    let text = event.get("text").and_then(Value::as_str)?;
    if text.is_empty() {
        return None;
    }
    let channel = event.get("channel").and_then(Value::as_str)?.to_string();
    let user = event.get("user").and_then(Value::as_str)?.to_string();
    let chat_type = match event.get("channel_type").and_then(Value::as_str) {
        Some("im") => "dm",
        _ => "group",
    };
    Some(Message {
        platform: Platform::Slack,
        channel_id: channel,
        sender_id: user,
        text: text.to_string(),
        chat_type: Some(chat_type.to_string()),
    })
}

/// Slack Socket Mode adapter.
pub struct SlackAdapter {
    app_token: String,
    bot_token: String,
    api_base: String,
    client: reqwest::Client,
}

impl SlackAdapter {
    pub fn new(app_token: impl Into<String>, bot_token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Other(format!("slack: build http client: {e}")))?;
        Ok(Self {
            app_token: app_token.into(),
            bot_token: bot_token.into(),
            api_base: API_BASE.to_string(),
            client,
        })
    }

    /// Mint a Socket Mode WebSocket URL via apps.connections.open.
    async fn open_connection(&self) -> Result<String> {
        let resp = self
            .client
            .post(format!("{}/apps.connections.open", self.api_base))
            .header("Authorization", format!("Bearer {}", self.app_token))
            .send()
            .await
            .map_err(|e| Error::Other(format!("slack apps.connections.open: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("slack open decode: {e}")))?;
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Other(format!(
                "slack apps.connections.open not ok: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        body.get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Other("slack: open response missing url".into()))
    }

    async fn connect_and_run(&self, inbound: &mpsc::Sender<Message>) -> Result<()> {
        let url = self.open_connection().await?;
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| Error::Other(format!("slack connect: {e}")))?;
        let (mut write, mut read) = ws.split();

        while let Some(frame) = read.next().await {
            let frame = frame.map_err(|e| Error::Other(format!("slack read: {e}")))?;
            match frame {
                WsMessage::Ping(p) => {
                    // Slack drops connections that don't pong.
                    let _ = write.send(WsMessage::Pong(p)).await;
                    continue;
                }
                WsMessage::Close(_) => {
                    return Err(Error::Other("slack: socket closed".into()));
                }
                _ => {}
            }
            let Some(payload) = parse_text_frame(&frame) else {
                continue;
            };
            let Some(env) = parse_envelope(&payload) else {
                continue;
            };

            // Ack anything that carries an envelope id, before handling it.
            if let Some(id) = &env.envelope_id {
                let _ = write
                    .send(WsMessage::text(ack_payload(id).to_string()))
                    .await;
            }

            match env.kind.as_str() {
                "events_api" => {
                    if let Some(msg) = parse_message_event(&env.payload) {
                        if inbound.send(msg).await.is_err() {
                            debug!("slack: inbound channel closed, stopping");
                            return Ok(());
                        }
                    }
                }
                // Slack asks us to reconnect (URL refresh, server rotation).
                "disconnect" => {
                    return Err(Error::Other("slack: server requested disconnect".into()))
                }
                _ => {} // "hello" and others: nothing to do.
            }
        }
        Err(Error::Other("slack: socket stream ended".into()))
    }
}

fn parse_text_frame(frame: &WsMessage) -> Option<Value> {
    match frame {
        WsMessage::Text(t) => serde_json::from_str(t).ok(),
        WsMessage::Binary(b) => serde_json::from_slice(b).ok(),
        _ => None,
    }
}

#[async_trait]
impl PlatformAdapter for SlackAdapter {
    fn name(&self) -> &str {
        "slack"
    }

    async fn run(&self, inbound: mpsc::Sender<Message>) -> Result<()> {
        loop {
            if let Err(err) = self.connect_and_run(&inbound).await {
                warn!(%err, "slack socket cycle ended; reconnecting after backoff");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if inbound.is_closed() {
                return Ok(());
            }
        }
    }

    async fn send(&self, msg: &Message) -> Result<()> {
        let resp = self
            .client
            .post(format!("{}/chat.postMessage", self.api_base))
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&json!({ "channel": msg.channel_id, "text": msg.text }))
            .send()
            .await
            .map_err(|e| Error::Other(format!("slack chat.postMessage: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("slack postMessage decode: {e}")))?;
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Other(format!(
                "slack chat.postMessage not ok: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_envelope_with_id() {
        let v = json!({
            "type": "events_api",
            "envelope_id": "abc",
            "payload": {"event": {"type": "message"}}
        });
        let env = parse_envelope(&v).unwrap();
        assert_eq!(env.envelope_id.as_deref(), Some("abc"));
        assert_eq!(env.kind, "events_api");
    }

    #[test]
    fn hello_has_no_envelope_id() {
        let env = parse_envelope(&json!({"type": "hello"})).unwrap();
        assert_eq!(env.kind, "hello");
        assert!(env.envelope_id.is_none());
    }

    #[test]
    fn ack_is_just_the_envelope_id() {
        assert_eq!(ack_payload("xyz"), json!({"envelope_id": "xyz"}));
    }

    #[test]
    fn parses_channel_message_as_group() {
        let payload = json!({"event": {
            "type": "message", "text": "hello",
            "channel": "C123", "user": "U456", "channel_type": "channel"
        }});
        let m = parse_message_event(&payload).unwrap();
        assert_eq!(m.platform, Platform::Slack);
        assert_eq!(m.channel_id, "C123");
        assert_eq!(m.sender_id, "U456");
        assert_eq!(m.text, "hello");
        assert_eq!(m.chat_type.as_deref(), Some("group"));
    }

    #[test]
    fn im_is_dm_scope() {
        let payload = json!({"event": {
            "type": "message", "text": "hi",
            "channel": "D1", "user": "U1", "channel_type": "im"
        }});
        assert_eq!(
            parse_message_event(&payload).unwrap().chat_type.as_deref(),
            Some("dm")
        );
    }

    #[test]
    fn skips_bot_and_subtype_and_nonmessage() {
        let bot = json!({"event": {"type": "message", "text": "x", "channel": "C", "user": "U", "bot_id": "B1"}});
        assert!(parse_message_event(&bot).is_none());
        let edited = json!({"event": {"type": "message", "subtype": "message_changed", "text": "x", "channel": "C", "user": "U"}});
        assert!(parse_message_event(&edited).is_none());
        let reaction = json!({"event": {"type": "reaction_added"}});
        assert!(parse_message_event(&reaction).is_none());
    }
}
