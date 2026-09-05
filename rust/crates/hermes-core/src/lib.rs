//! Shared types for the Hermes Rust rewrite.
//!
//! This crate holds the vocabulary that every other crate agrees on: the
//! error type, and the message/channel shapes that cross the gateway <-> agent
//! RPC boundary. It intentionally has no async or IO dependencies so it stays
//! cheap to depend on.

use serde::{Deserialize, Serialize};

pub mod error;
pub mod stream;

pub use error::{Error, Result};
pub use stream::StreamEvent;

/// A messaging platform Hermes can talk on. Mirrors the Python
/// `gateway.platform_registry` set. Kept as an enum so routing is exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Cli,
    Telegram,
    Discord,
    Slack,
    WhatsApp,
    Signal,
}

/// Structured user content shared by gateway transport and model requests.
/// Images arrive as provider-fetchable URLs or prepared base64 data URLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A single inbound or outbound message on some platform. This is the minimal
/// shape the gateway needs before it hands off to the agent; richer per-turn
/// context is layered on later in the port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub platform: Platform,
    /// Opaque per-platform conversation id (chat id, channel id, ...).
    pub channel_id: String,
    /// Opaque per-platform sender id.
    pub sender_id: String,
    pub text: String,
    /// Prepared model content. Text stays available for command dispatch and
    /// platform replies; when present these parts are the model's user turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_parts: Option<Vec<ContentPart>>,
    /// Platform-provided chat type (e.g. Telegram "private"/"group"/"channel").
    /// Used to resolve DM-vs-group access scope. None when unknown.
    #[serde(default)]
    pub chat_type: Option<String>,
    /// Local filesystem paths of inbound audio attachments (voice notes) an
    /// adapter downloaded for this message. Empty when there is no audio. The
    /// dispatcher transcribes these before running the turn; the resulting text
    /// becomes the user turn. Adapters that do not download audio leave it empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_paths: Vec<String>,
}

impl Message {
    pub fn model_content(&self) -> serde_json::Value {
        match &self.content_parts {
            Some(parts) => {
                serde_json::to_value(parts).expect("content parts contain serializable strings")
            }
            None => serde_json::Value::String(self.text.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_text_messages_and_structured_parts_roundtrip() {
        let old = json!({"platform":"cli", "channel_id":"c", "sender_id":"s", "text":"caption"});
        let mut msg: Message = serde_json::from_value(old).unwrap();
        assert_eq!(msg.model_content(), "caption");
        assert!(serde_json::to_value(&msg)
            .unwrap()
            .get("content_parts")
            .is_none());
        let parts = json!([{"type":"text","text":"caption"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"high"}}]);
        msg.content_parts = Some(serde_json::from_value(parts.clone()).unwrap());
        let decoded: Message = serde_json::from_value(serde_json::to_value(&msg).unwrap()).unwrap();
        assert_eq!(decoded.model_content(), parts);
    }
}
