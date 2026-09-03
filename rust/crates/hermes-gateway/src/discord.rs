//! Discord platform adapter (Gateway WebSocket).
//!
//! The second real [`PlatformAdapter`]. Unlike Telegram's HTTP long-poll,
//! Discord delivers messages over a Gateway WebSocket (v10): connect, receive
//! HELLO (op 10) with the heartbeat interval, heartbeat (op 1) with the last
//! sequence, IDENTIFY (op 2) with intents, then handle dispatch (op 0) events.
//! Outbound uses the REST API (`POST /channels/{id}/messages`). Built from the
//! documented contract (https://docs.discord.com/developers/topics/gateway),
//! not a port of the ~10k-LOC Python adapter; rich features land later.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hermes_core::{Error, Message, Platform, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::platform::PlatformAdapter;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const API_BASE: &str = "https://discord.com/api/v10";

/// Intents bitfield: GUILD_MESSAGES (1<<9) | DIRECT_MESSAGES (1<<12) |
/// MESSAGE_CONTENT (1<<15). MESSAGE_CONTENT is privileged and must be enabled
/// for the bot in the Developer Portal, or `content` arrives empty.
pub const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);

/// Build the IDENTIFY (op 2) payload.
pub fn identify_payload(token: &str, intents: u64) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents,
            "properties": { "os": "linux", "browser": "hermes", "device": "hermes" }
        }
    })
}

/// Build the heartbeat (op 1) payload. `seq` is the last dispatch sequence, or
/// `None` for the initial null heartbeat.
pub fn heartbeat_payload(seq: Option<i64>) -> Value {
    json!({ "op": 1, "d": seq })
}

/// Map a Discord MESSAGE_CREATE `d` payload to a [`Message`].
///
/// Returns `None` for messages from bots (avoids reply loops on the bot's own
/// posts) and for empty content. `chat_type` is "group" when a `guild_id` is
/// present (a server channel) and "dm" otherwise, so access scope resolves.
pub fn parse_message_create(d: &Value) -> Option<Message> {
    let author = d.get("author")?;
    if author.get("bot").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let content = d.get("content").and_then(Value::as_str)?;
    if content.is_empty() {
        return None;
    }
    let channel_id = d.get("channel_id").and_then(Value::as_str)?.to_string();
    let sender_id = author.get("id").and_then(Value::as_str)?.to_string();
    let chat_type = if d.get("guild_id").map(|g| !g.is_null()).unwrap_or(false) {
        "group"
    } else {
        "dm"
    };
    Some(Message {
        platform: Platform::Discord,
        channel_id,
        sender_id,
        text: content.to_string(),
        chat_type: Some(chat_type.to_string()),
    })
}

/// Discord Gateway adapter.
pub struct DiscordAdapter {
    token: String,
    gateway_url: String,
    api_base: String,
    client: reqwest::Client,
}

impl DiscordAdapter {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Other(format!("discord: build http client: {e}")))?;
        Ok(Self {
            token: token.into(),
            gateway_url: GATEWAY_URL.to_string(),
            api_base: API_BASE.to_string(),
            client,
        })
    }

    /// One connect + run cycle. Returns on close/error so the caller reconnects.
    async fn connect_and_run(&self, inbound: &mpsc::Sender<Message>) -> Result<()> {
        let (ws, _) = tokio_tungstenite::connect_async(&self.gateway_url)
            .await
            .map_err(|e| Error::Other(format!("discord connect: {e}")))?;
        let (mut write, mut read) = ws.split();

        // First frame must be HELLO (op 10) carrying the heartbeat interval.
        let hello = read
            .next()
            .await
            .ok_or_else(|| Error::Other("discord: closed before HELLO".into()))?
            .map_err(|e| Error::Other(format!("discord read HELLO: {e}")))?;
        let hello: Value = parse_text_frame(&hello)
            .ok_or_else(|| Error::Other("discord: HELLO not a text frame".into()))?;
        let interval_ms = hello
            .get("d")
            .and_then(|d| d.get("heartbeat_interval"))
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Other("discord: HELLO missing heartbeat_interval".into()))?;

        // Single writer: heartbeat task and the read loop funnel outbound frames
        // here so the sink is owned by one task.
        let (out_tx, mut out_rx) = mpsc::channel::<WsMessage>(32);
        let writer = tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if write.send(m).await.is_err() {
                    break;
                }
            }
        });

        // IDENTIFY.
        out_tx
            .send(WsMessage::text(
                identify_payload(&self.token, INTENTS).to_string(),
            ))
            .await
            .map_err(|_| Error::Other("discord: writer gone before IDENTIFY".into()))?;

        // Heartbeat loop with the shared last-sequence.
        let last_seq = Arc::new(AtomicI64::new(-1));
        let hb_seq = Arc::clone(&last_seq);
        let hb_tx = out_tx.clone();
        let heartbeat = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
            loop {
                ticker.tick().await;
                let s = hb_seq.load(Ordering::SeqCst);
                let seq = if s < 0 { None } else { Some(s) };
                if hb_tx
                    .send(WsMessage::text(heartbeat_payload(seq).to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let result = self.read_loop(&mut read, &last_seq, &out_tx, inbound).await;

        heartbeat.abort();
        writer.abort();
        result
    }

    async fn read_loop(
        &self,
        read: &mut (impl StreamExt<Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
                  + Unpin),
        last_seq: &AtomicI64,
        out_tx: &mpsc::Sender<WsMessage>,
        inbound: &mpsc::Sender<Message>,
    ) -> Result<()> {
        while let Some(frame) = read.next().await {
            let frame = frame.map_err(|e| Error::Other(format!("discord read: {e}")))?;
            if frame.is_close() {
                return Err(Error::Other("discord: gateway closed connection".into()));
            }
            let Some(payload) = parse_text_frame(&frame) else {
                continue;
            };
            let op = payload.get("op").and_then(Value::as_i64).unwrap_or(-1);
            if let Some(s) = payload.get("s").and_then(Value::as_i64) {
                last_seq.store(s, Ordering::SeqCst);
            }
            match op {
                // Dispatch.
                0 if payload.get("t").and_then(Value::as_str) == Some("MESSAGE_CREATE") => {
                    if let Some(msg) = payload.get("d").and_then(parse_message_create) {
                        if inbound.send(msg).await.is_err() {
                            debug!("discord: inbound channel closed, stopping");
                            return Ok(());
                        }
                    }
                }
                // Server-requested heartbeat: reply immediately.
                1 => {
                    let s = last_seq.load(Ordering::SeqCst);
                    let seq = if s < 0 { None } else { Some(s) };
                    let _ = out_tx
                        .send(WsMessage::text(heartbeat_payload(seq).to_string()))
                        .await;
                }
                // Reconnect / invalid session: drop and let the caller reconnect.
                7 | 9 => return Err(Error::Other(format!("discord: op {op}, reconnecting"))),
                // 10 HELLO already consumed; 11 is heartbeat ACK; ignore others.
                _ => {}
            }
        }
        Err(Error::Other("discord: gateway stream ended".into()))
    }
}

/// Extract the JSON payload from a text WebSocket frame, if it is one.
fn parse_text_frame(frame: &WsMessage) -> Option<Value> {
    match frame {
        WsMessage::Text(t) => serde_json::from_str(t).ok(),
        WsMessage::Binary(b) => serde_json::from_slice(b).ok(),
        _ => None,
    }
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    fn name(&self) -> &str {
        "discord"
    }

    async fn run(&self, inbound: mpsc::Sender<Message>) -> Result<()> {
        loop {
            if let Err(err) = self.connect_and_run(&inbound).await {
                warn!(%err, "discord gateway cycle ended; reconnecting after backoff");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if inbound.is_closed() {
                return Ok(());
            }
        }
    }

    async fn send(&self, msg: &Message) -> Result<()> {
        let url = format!("{}/channels/{}/messages", self.api_base, msg.channel_id);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .json(&json!({ "content": msg.text }))
            .send()
            .await
            .map_err(|e| Error::Other(format!("discord sendMessage: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "discord sendMessage failed: {status} {body}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_include_guild_dm_and_content() {
        assert_eq!(INTENTS, 512 + 4096 + 32768);
    }

    #[test]
    fn identify_has_token_and_intents() {
        let p = identify_payload("secret", INTENTS);
        assert_eq!(p["op"], 2);
        assert_eq!(p["d"]["token"], "secret");
        assert_eq!(p["d"]["intents"], INTENTS);
        assert_eq!(p["d"]["properties"]["os"], "linux");
    }

    #[test]
    fn heartbeat_null_and_seq() {
        assert_eq!(heartbeat_payload(None), json!({"op": 1, "d": null}));
        assert_eq!(heartbeat_payload(Some(42)), json!({"op": 1, "d": 42}));
    }

    #[test]
    fn parses_guild_message() {
        let d = json!({
            "content": "hello",
            "author": {"id": "111", "bot": false},
            "channel_id": "222",
            "guild_id": "333"
        });
        let m = parse_message_create(&d).unwrap();
        assert_eq!(m.platform, Platform::Discord);
        assert_eq!(m.channel_id, "222");
        assert_eq!(m.sender_id, "111");
        assert_eq!(m.text, "hello");
        assert_eq!(m.chat_type.as_deref(), Some("group"));
    }

    #[test]
    fn dm_has_no_guild_id() {
        let d = json!({
            "content": "hi",
            "author": {"id": "111"},
            "channel_id": "222"
        });
        let m = parse_message_create(&d).unwrap();
        assert_eq!(m.chat_type.as_deref(), Some("dm"));
    }

    #[test]
    fn skips_bots_and_empty() {
        let bot = json!({"content": "x", "author": {"id": "1", "bot": true}, "channel_id": "2"});
        assert!(parse_message_create(&bot).is_none());
        let empty = json!({"content": "", "author": {"id": "1"}, "channel_id": "2"});
        assert!(parse_message_create(&empty).is_none());
    }
}
