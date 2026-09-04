//! Native (in-Rust) agent client for simple chat turns.
//!
//! The first step toward dropping the Python subprocess for the actual agent
//! turn (the real footprint goal). This client calls an OpenAI-compatible
//! `/chat/completions` endpoint directly and streams the reply, so a plain-chat
//! turn needs no Python at all.
//!
//! Scope is deliberately narrow: single user message, streamed text out, no
//! tools, no conversation history, no memory or skills. Those are what
//! `run_agent.py` provides and are ported later; until then the subprocess
//! bridge remains the default and this is opt-in.
//!
//! Streaming contract (OpenAI / OpenRouter, verified against the docs): POST
//! `{base_url}/chat/completions` with `Authorization: Bearer <key>` and
//! `{model, messages, stream:true}`; the response is SSE where each `data: {..}`
//! line carries `choices[0].delta.content`, `:`-prefixed lines are keepalive
//! comments to skip, and `data: [DONE]` terminates (a trailing usage chunk with
//! an empty delta arrives just before it).

use async_trait::async_trait;
use futures_util::StreamExt;
use hermes_core::{Error, Message, Result, StreamEvent};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::agent::AgentClient;
use crate::native_tools::{parse_message_step, ChatModel, Step};

/// One decoded SSE line.
#[derive(Debug, PartialEq)]
pub enum SseEvent {
    /// A text delta to forward.
    Delta(String),
    /// The stream is complete (`data: [DONE]`).
    Done,
    /// A line with nothing to forward (keepalive, role-only/usage delta, blank).
    Ignore,
}

/// Build the chat-completions request body for a single user message.
pub fn build_request_body(model: &str, text: &str) -> Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": text }],
        "stream": true,
    })
}

/// Decode one SSE line into an [`SseEvent`].
pub fn parse_sse_line(line: &str) -> SseEvent {
    let line = line.trim_end_matches('\r');
    // Keepalive comment lines start with ':' (e.g. ": OPENROUTER PROCESSING").
    if line.is_empty() || line.starts_with(':') {
        return SseEvent::Ignore;
    }
    let Some(data) = line.strip_prefix("data:") else {
        return SseEvent::Ignore;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return SseEvent::Done;
    }
    // A delta chunk: choices[0].delta.content. Missing/empty (role-only or the
    // trailing usage chunk) yields nothing to forward.
    match serde_json::from_str::<Value>(data) {
        Ok(v) => match v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
        {
            Some(s) if !s.is_empty() => SseEvent::Delta(s.to_string()),
            _ => SseEvent::Ignore,
        },
        Err(_) => SseEvent::Ignore,
    }
}

/// Native OpenAI-compatible chat client.
pub struct NativeAgentClient {
    model: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl NativeAgentClient {
    /// `base_url` is the API root (e.g. `https://openrouter.ai/api/v1`).
    pub fn new(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::Other(format!("native agent: build http client: {e}")))?;
        Ok(Self {
            model: model.into(),
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        })
    }
}

#[async_trait]
impl AgentClient for NativeAgentClient {
    async fn run_turn(&self, msg: &Message, events: mpsc::Sender<StreamEvent>) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&build_request_body(&self.model, &msg.text))
            .send()
            .await
            .map_err(|e| Error::Other(format!("native agent request: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "native agent HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            )));
        }

        // Parse the SSE byte stream line by line, buffering partial lines across
        // chunk boundaries.
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Other(format!("native agent stream: {e}")))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                match parse_sse_line(&line) {
                    SseEvent::Delta(text) => {
                        let _ = events.send(StreamEvent::MessageChunk { text }).await;
                    }
                    SseEvent::Done => {
                        done = true;
                        break;
                    }
                    SseEvent::Ignore => {}
                }
            }
            if done {
                break;
            }
        }
        // Handle any final buffered line if the stream ended without a newline.
        if !done {
            if let SseEvent::Delta(text) = parse_sse_line(&buf) {
                let _ = events.send(StreamEvent::MessageChunk { text }).await;
            }
        }

        let _ = events.send(StreamEvent::MessageStop { final_: true }).await;
        Ok(())
    }
}

#[async_trait]
impl ChatModel for NativeAgentClient {
    /// One non-streaming completion with tools. Tool calls arrive whole in the
    /// message, which is simpler and more reliable than reassembling streamed
    /// tool-call deltas; the streaming path ([`AgentClient::run_turn`]) stays
    /// for the no-tools case.
    async fn step(&self, messages: &[Value], tools: &[Value]) -> Result<Step> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = json!({ "model": self.model, "messages": messages, "stream": false });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("native agent step request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "native agent step HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("native agent step decode: {e}")))?;
        let message = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .ok_or_else(|| Error::Other("native agent step: no choices[0].message".into()))?;
        Ok(parse_message_step(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_shape() {
        let b = build_request_body("openai/gpt-x", "hi");
        assert_eq!(b["model"], "openai/gpt-x");
        assert_eq!(b["stream"], true);
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][0]["content"], "hi");
    }

    #[test]
    fn parses_delta_lines() {
        let line = r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert_eq!(parse_sse_line(line), SseEvent::Delta("Hello".into()));
    }

    #[test]
    fn done_terminator() {
        assert_eq!(parse_sse_line("data: [DONE]"), SseEvent::Done);
    }

    #[test]
    fn keepalive_and_blank_are_ignored() {
        assert_eq!(parse_sse_line(": OPENROUTER PROCESSING"), SseEvent::Ignore);
        assert_eq!(parse_sse_line(""), SseEvent::Ignore);
        assert_eq!(parse_sse_line("\r"), SseEvent::Ignore);
    }

    #[test]
    fn role_only_and_usage_deltas_forward_nothing() {
        // Opening chunk carries the role but no content.
        let role = r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert_eq!(parse_sse_line(role), SseEvent::Ignore);
        // Trailing usage chunk has an empty delta.
        let usage =
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"total_tokens":5}}"#;
        assert_eq!(parse_sse_line(usage), SseEvent::Ignore);
        // Explicit empty content string.
        let empty = r#"data: {"choices":[{"delta":{"content":""}}]}"#;
        assert_eq!(parse_sse_line(empty), SseEvent::Ignore);
    }

    #[test]
    fn malformed_and_non_data_lines_are_ignored() {
        assert_eq!(parse_sse_line("data: {not json"), SseEvent::Ignore);
        assert_eq!(parse_sse_line("event: message"), SseEvent::Ignore);
    }

    #[test]
    fn carriage_returns_trimmed_before_done() {
        assert_eq!(parse_sse_line("data: [DONE]\r"), SseEvent::Done);
    }
}
