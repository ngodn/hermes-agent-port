//! CLI-backend agent client.
//!
//! Runs an external agent CLI (Claude Code `claude`, Antigravity `agy`, or any
//! print-mode LLM CLI) for a turn, instead of an OpenAI-compatible HTTP endpoint
//! or the hermes Python bridge. This is the path that makes native (Python-free)
//! turns work for a setup whose provider is a CLI backend rather than an HTTP
//! key, which is common: the user's hermes routes through Claude Code /
//! Antigravity, and `~/.hermes/.env` has no HTTP key.
//!
//! These CLIs are already agents (they run their own tool loop internally), so
//! this client just spawns `program [args...] -p "<prompt>"`, captures stdout as
//! the reply, and emits it. No OpenAI tool protocol is involved.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use hermes_core::{Error, Message, Result, StreamEvent};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::agent::AgentClient;

/// Build the full argument vector: `extra_args` then the prompt, passed via
/// `prompt_flag` when set (e.g. `-p "<prompt>"`) or as a trailing positional.
pub fn build_args(extra_args: &[String], prompt_flag: Option<&str>, prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = extra_args.to_vec();
    if let Some(flag) = prompt_flag {
        args.push(flag.to_string());
    }
    args.push(prompt.to_string());
    args
}

/// Split a whitespace-separated extra-args string. Note: this does not honor
/// shell quoting; callers needing quoted args should pass them structurally.
pub fn split_extra_args(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(str::to_string).collect()
}

/// Spawns an external agent CLI per turn.
pub struct CliAgentClient {
    program: String,
    extra_args: Vec<String>,
    prompt_flag: Option<String>,
    timeout: Duration,
}

impl CliAgentClient {
    pub fn new(
        program: impl Into<String>,
        extra_args: Vec<String>,
        prompt_flag: Option<String>,
    ) -> Self {
        Self {
            program: program.into(),
            extra_args,
            prompt_flag,
            timeout: Duration::from_secs(300),
        }
    }
}

#[async_trait]
impl AgentClient for CliAgentClient {
    async fn run_turn(&self, msg: &Message, events: mpsc::Sender<StreamEvent>) -> Result<()> {
        let args = build_args(&self.extra_args, self.prompt_flag.as_deref(), &msg.text);
        let output = tokio::time::timeout(
            self.timeout,
            Command::new(&self.program)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| Error::Other(format!("cli agent '{}' timed out", self.program)))?
        .map_err(|e| Error::Other(format!("cli agent '{}' spawn failed: {e}", self.program)))?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Other(format!(
                "cli agent '{}' exited {}: {}",
                self.program,
                output.status,
                err.chars().take(300).collect::<String>()
            )));
        }

        let reply = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !reply.is_empty() {
            let _ = events.send(StreamEvent::MessageChunk { text: reply }).await;
        }
        let _ = events.send(StreamEvent::MessageStop { final_: true }).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_with_flag() {
        let extra = split_extra_args("--model gemini-3.8-flash-high --output-format text");
        let args = build_args(&extra, Some("-p"), "hello world");
        assert_eq!(
            args,
            vec![
                "--model",
                "gemini-3.8-flash-high",
                "--output-format",
                "text",
                "-p",
                "hello world"
            ]
        );
    }

    #[test]
    fn args_without_flag_are_positional() {
        let args = build_args(&[], None, "just the prompt");
        assert_eq!(args, vec!["just the prompt"]);
    }

    #[test]
    fn split_extra_args_handles_empty() {
        assert!(split_extra_args("").is_empty());
        assert_eq!(split_extra_args("  a   b "), vec!["a", "b"]);
    }

    /// End-to-end against a stub "CLI": `printf` echoes fixed text as the reply.
    #[tokio::test]
    async fn runs_a_stub_cli_and_emits_reply() {
        // Use printf as a trivial stand-in agent CLI: it prints its argument.
        let client = CliAgentClient::new("printf", vec!["%s".to_string()], None);
        let msg = Message {
            platform: hermes_core::Platform::Cli,
            channel_id: "c".into(),
            sender_id: "u".into(),
            text: "hi from cli".into(),
            chat_type: None,
        };
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
        client.run_turn(&msg, tx).await.unwrap();
        let mut got = String::new();
        let mut stopped = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                StreamEvent::MessageChunk { text } => got.push_str(&text),
                StreamEvent::MessageStop { final_: true } => stopped = true,
                _ => {}
            }
        }
        assert_eq!(got, "hi from cli");
        assert!(stopped);
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error() {
        // `false` exits 1 with no output.
        let client = CliAgentClient::new("false", vec![], None);
        let msg = Message {
            platform: hermes_core::Platform::Cli,
            channel_id: "c".into(),
            sender_id: "u".into(),
            text: "x".into(),
            chat_type: None,
        };
        let (tx, _rx) = mpsc::channel::<StreamEvent>(8);
        assert!(client.run_turn(&msg, tx).await.is_err());
    }
}
