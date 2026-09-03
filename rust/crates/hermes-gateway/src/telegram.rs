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
//!   `chat.id`, optional `text`.
//! - `sendMessage` with `chat_id` + `text`.

use std::time::Duration;

use async_trait::async_trait;
use hermes_core::{Error, Message, Platform, Result};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::platform::PlatformAdapter;

/// A parsed Telegram update: the id (to advance the poll offset) and, when the
/// update carried a text message, the mapped [`Message`].
#[derive(Debug, PartialEq)]
pub struct ParsedUpdate {
    pub update_id: i64,
    pub message: Option<Message>,
}

/// Extract the update id and (if a text message) a [`Message`] from one raw
/// Telegram Update object. Returns `None` only when there is no `update_id`
/// (a malformed update we cannot use to advance the offset).
pub fn extract_update(update: &Value) -> Option<ParsedUpdate> {
    let update_id = update.get("update_id").and_then(Value::as_i64)?;

    // Only plain text messages become turns for now. Edited messages, channel
    // posts, callbacks etc. still advance the offset but produce no Message.
    let message = update.get("message").and_then(|m| {
        let text = m.get("text").and_then(Value::as_str)?;
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
            platform: Platform::Telegram,
            channel_id: chat_id.to_string(),
            sender_id: sender_id.to_string(),
            text: text.to_string(),
            chat_type,
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
}

impl TelegramAdapter {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            // getUpdates blocks up to poll_timeout; give the request headroom.
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Other(format!("telegram: build http client: {e}")))?;
        Ok(Self {
            token: token.into(),
            api_base: "https://api.telegram.org".to_string(),
            poll_timeout: Duration::from_secs(30),
            client,
        })
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
                        let Some(parsed) = extract_update(raw) else {
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
