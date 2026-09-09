//! The gateway -> agent boundary.
//!
// WIP: SubprocessAgentClient is implemented but not yet wired into main().
#![allow(dead_code)]
//!
//! In the Python codebase the gateway imports `run_agent` and calls it
//! in-process; there is no network seam between them. For the Rust rewrite we
//! introduce one explicit boundary here so the two halves can be ported
//! independently:
//!
//! * [`SubprocessAgentClient`] (strangler step): spawns the existing Python
//!   agent as a child process per turn and streams its events back. This keeps
//!   the Python agent authoritative while the Rust gateway owns the network and
//!   lifecycle. The child is `python -m hermes_cli.stream_turn` (the bridge shim
//!   added for the port), which emits newline-delimited JSON events.
//! * a native in-process agent client lands later, once `run_agent.py` itself
//!   is ported, and swaps in behind the same trait.
//!
//! Turns stream [`StreamEvent`]s (the real agent->gateway contract, ported in
//! `hermes_core::stream`). The turn ends with `MessageStop { final_: true }`;
//! a hard failure surfaces as `Err` from `run_turn`.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use hermes_core::{Error, Message, Result, StreamEvent};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::warn;

/// Selected internal session context. Keeping the actual store avoids reopening
/// a guessed state.db path when a caller uses a custom database location.
#[derive(Clone, Copy, Default)]
pub struct TurnContext<'a> {
    pub home: Option<&'a std::path::Path>,
    pub database: Option<&'a crate::session_db::SessionDb>,
    /// Stable gateway route that owns this admitted turn. Unlike a physical
    /// session id, this stays aligned with control replies after resume.
    pub route_key: Option<&'a str>,
    /// Shared physical identity for a routed turn that may publish a
    /// compression child before the provider loop finishes.
    pub turn_session: Option<&'a crate::turn_session::TurnSession>,
    /// Owning conversation wrapper. Post-commit compression notification must
    /// pass through it so the frozen client cache follows a rotated session.
    pub compression_observer: Option<&'a dyn AgentClient>,
    /// Owner token for the cross-process conversation lease held by this
    /// admitted turn. Transcript mutations can recheck it inside SQLite.
    pub turn_lease_holder: Option<&'a str>,
    /// True when the session reset policy has a finite boundary. Cache
    /// pressure must preserve its end-of-session memory extraction before
    /// releasing the live conversation client.
    pub session_finalizable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressionPreflight {
    pub model: String,
    pub context_length: u64,
    pub max_output_tokens: Option<u64>,
    pub request_tokens: u64,
    /// True when generic reasoning text from stale assistant turns reaches
    /// the active provider wire and must count against tail budgets.
    pub stale_thinking_on_wire: bool,
}

/// Result of the external-memory boundary that runs before full compression.
/// A required checkpoint may proceed only when `checkpoint_supported` is true
/// and the request itself completed successfully.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreCompressionCheckpoint {
    pub checkpoint_supported: bool,
    pub memory_context: Option<String>,
}

impl<'a> TurnContext<'a> {
    pub fn from_database(database: Option<&'a crate::session_db::SessionDb>) -> Self {
        Self {
            home: database.and_then(|db| db.profile_home()),
            database,
            route_key: None,
            turn_session: None,
            compression_observer: None,
            turn_lease_holder: None,
            session_finalizable: false,
        }
    }

    pub fn with_route_key(mut self, route_key: Option<&'a str>) -> Self {
        self.route_key = route_key;
        self
    }

    pub fn with_session_finalizable(mut self, finalizable: bool) -> Self {
        self.session_finalizable = finalizable;
        self
    }

    pub fn with_turn_lease_holder(mut self, holder: Option<&'a str>) -> Self {
        self.turn_lease_holder = holder;
        self
    }

    pub fn with_turn_session(
        mut self,
        turn_session: Option<&'a crate::turn_session::TurnSession>,
    ) -> Self {
        self.turn_session = turn_session;
        self
    }

    pub fn with_compression_observer(mut self, observer: &'a dyn AgentClient) -> Self {
        self.compression_observer = Some(observer);
        self
    }
}

/// Drives a single agent turn for an inbound message, streaming events out.
///
/// `history` is the session's prior messages (oldest first, excluding the
/// current `msg`), for backends that are otherwise stateless (native HTTP, CLI).
/// A backend that manages its own history (the Python subprocess bridge, which
/// loads/persists via the hermes session DB) reports [`Self::manages_history`]
/// `= true`, and the caller then neither loads nor persists history for it.
#[async_trait]
pub trait AgentClient: Send + Sync {
    /// Internal routing context from the selected history store, never ingress
    /// payload. Existing backends keep their own profile/history semantics.
    async fn run_turn_with_context(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
        events: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let _ = context;
        self.run_turn(msg, history, events).await
    }

    /// Whether prepared model content can cross this backend's input boundary.
    fn supports_structured_content(&self) -> bool {
        false
    }
    async fn run_turn(
        &self,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
        events: mpsc::Sender<StreamEvent>,
    ) -> Result<()>;

    /// Produce one unwrapped out-of-band context summary without running tools
    /// or persisting a user turn. `None` means this backend has no native
    /// summary surface and the caller must leave the transcript unchanged.
    async fn summarize_context(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::CompressionHistoryMessage],
        focus_topic: Option<&str>,
    ) -> Result<Option<String>> {
        let _ = (context, msg, history, focus_topic);
        Ok(None)
    }

    /// Notify external memory before a full compression can discard direct
    /// evidence. Backends without an extension manager report unsupported;
    /// callers decide whether that is a best-effort no-op or a required gate.
    async fn prepare_pre_compression_checkpoint(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::CompressionHistoryMessage],
        require_checkpoint: bool,
    ) -> Result<PreCompressionCheckpoint> {
        let _ = (context, msg, history, require_checkpoint);
        Ok(PreCompressionCheckpoint::default())
    }

    /// Summary boundary with optional provider-supplied memory context. The
    /// default preserves compatibility for backends that own summarization but
    /// have no native memory manager.
    async fn summarize_context_with_memory(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::CompressionHistoryMessage],
        focus_topic: Option<&str>,
        memory_context: Option<&str>,
    ) -> Result<Option<String>> {
        let _ = memory_context;
        self.summarize_context(context, msg, history, focus_topic)
            .await
    }

    /// Notify conversation-scoped extensions after a full-compression
    /// publication commits. Rotation changes the physical session id while
    /// preserving the logical conversation; in-place publication reports the
    /// same id on both sides. Observer failures must never undo the commit.
    async fn notify_compression_boundary(
        &self,
        context: TurnContext<'_>,
        old_session_id: &str,
        new_session_id: &str,
        in_place: bool,
    ) -> Result<()> {
        let _ = (context, old_session_id, new_session_id, in_place);
        Ok(())
    }

    /// Measure the provider-visible request before any provider I/O. Native
    /// clients include the frozen system prompt, history, inbound content and
    /// frozen tool schemas. `None` keeps bridge backends on their existing
    /// compression implementation.
    async fn compression_preflight(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
    ) -> Result<Option<CompressionPreflight>> {
        let _ = (context, msg, history);
        Ok(None)
    }

    /// Remaining transient delay after compression proved structurally
    /// impossible for the current conversation. This guard is intentionally
    /// process-local and must not be persisted with the durable anti-thrash
    /// breaker.
    fn compression_structural_backoff_remaining(
        &self,
        context: TurnContext<'_>,
        session_id: &str,
    ) -> Option<std::time::Duration> {
        let _ = (context, session_id);
        None
    }

    /// Defer automatic retries after finding no complete compressible region.
    fn record_compression_structural_no_op(
        &self,
        context: TurnContext<'_>,
        session_id: &str,
        reason: &str,
    ) {
        let _ = (context, session_id, reason);
    }

    /// Lift the transient structural guard for a forced attempt or a committed
    /// compression boundary.
    fn clear_compression_structural_backoff(&self, context: TurnContext<'_>, session_id: &str) {
        let _ = (context, session_id);
    }

    /// Complete post-turn side effects after the gateway has durably recorded
    /// the assistant reply. Backends without such hooks keep the default no-op.
    async fn finalize_turn_after_persist(
        &self,
        context: TurnContext<'_>,
        msg: &Message,
        reply: &str,
        succeeded: bool,
    ) -> Result<()> {
        let _ = (context, msg, reply, succeeded);
        Ok(())
    }

    /// Release conversation-owned resources. `Some(messages)` marks a real
    /// session boundary and lets providers run end-of-session extraction;
    /// `None` is a cache-only release. Implementations must be idempotent.
    async fn close_conversation(
        &self,
        session_messages: Option<&[serde_json::Value]>,
    ) -> Result<()> {
        let _ = session_messages;
        Ok(())
    }

    /// Retire one superseded cached conversation. The default is a no-op for
    /// backends without per-conversation client ownership.
    fn retire_conversation(&self, context: TurnContext<'_>, session_id: &str) -> bool {
        let _ = (context, session_id);
        false
    }

    /// Evict conversation-local provider state without declaring a true
    /// session end. Callers use this for non-boundary invalidation; successful
    /// compression rotation transfers a byte-stable client instead.
    fn release_conversation(&self, context: TurnContext<'_>, session_id: &str) -> bool {
        let _ = (context, session_id);
        false
    }

    /// True when the backend loads and persists conversation history itself, so
    /// the gateway must not inject or record history around it.
    fn manages_history(&self) -> bool {
        false
    }
}

/// The JSONL envelope emitted by `hermes_cli.stream_turn` (one per stdout line).
#[derive(Debug, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
enum BridgeEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolStart {
        tool: String,
        #[serde(default)]
        args: Option<std::collections::HashMap<String, serde_json::Value>>,
    },
    ToolComplete {
        tool: String,
        #[serde(default = "default_true")]
        ok: bool,
        #[serde(default)]
        duration: f64,
    },
    Done {
        #[serde(default = "default_true")]
        completed: bool,
        #[serde(default)]
        final_response: String,
    },
    Error {
        message: String,
    },
}

fn default_true() -> bool {
    true
}

/// Result of mapping one bridge event: stream events to forward, or a fatal
/// turn error.
enum Mapped {
    Emit(Vec<StreamEvent>),
    Error(String),
}

/// Map one JSONL bridge event onto stream events. `streamed_text` tracks whether
/// any assistant text has been forwarded, so a turn that only carried a
/// `final_response` (no deltas) still delivers that text on `done`. Pure aside
/// from flipping `streamed_text`, so it is unit-tested directly.
fn map_bridge_event(event: BridgeEvent, streamed_text: &mut bool) -> Mapped {
    match event {
        BridgeEvent::TextDelta { text } => {
            *streamed_text = true;
            Mapped::Emit(vec![StreamEvent::MessageChunk { text }])
        }
        // Reasoning is presentation-only and filtered from history; the stream
        // contract keeps it out of MessageChunk. Drop for now.
        BridgeEvent::ThinkingDelta { .. } => Mapped::Emit(vec![]),
        BridgeEvent::ToolStart { tool, args } => Mapped::Emit(vec![StreamEvent::ToolCallChunk {
            tool_name: tool,
            preview: None,
            args,
            index: 0,
        }]),
        BridgeEvent::ToolComplete { tool, ok, duration } => {
            Mapped::Emit(vec![StreamEvent::ToolCallFinished {
                tool_name: tool,
                duration,
                ok,
                index: 0,
            }])
        }
        BridgeEvent::Done {
            completed: _,
            final_response,
        } => {
            let mut out = Vec::new();
            if !*streamed_text && !final_response.is_empty() {
                out.push(StreamEvent::MessageChunk {
                    text: final_response,
                });
            }
            out.push(StreamEvent::MessageStop { final_: true });
            Mapped::Emit(out)
        }
        BridgeEvent::Error { message } => Mapped::Error(message),
    }
}

/// Strangler-step client: runs the existing Python agent as a subprocess via
/// the `hermes_cli.stream_turn` bridge shim.
pub struct SubprocessAgentClient {
    /// Python interpreter to invoke (e.g. "python3", or a venv path).
    pub python: String,
    /// Working directory: the repo root, so `-m hermes_cli.stream_turn` resolves.
    pub cwd: PathBuf,
    /// Optional model override passed through to the agent.
    pub model: Option<String>,
}

impl SubprocessAgentClient {
    pub fn new(python: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            python: python.into(),
            cwd: cwd.into(),
            model: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

#[async_trait]
impl AgentClient for SubprocessAgentClient {
    // The Python agent loads and persists its own history via the hermes
    // session DB, so the gateway must not inject or record history around it.
    fn manages_history(&self) -> bool {
        true
    }

    async fn run_turn(
        &self,
        msg: &Message,
        _history: &[crate::session_db::HistoryMessage],
        events: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        if msg.content_parts.is_some() {
            return Err(Error::Other(
                "Python bridge does not yet accept structured content".into(),
            ));
        }
        let mut cmd = Command::new(&self.python);
        cmd.arg("-m")
            .arg("hermes_cli.stream_turn")
            .arg("-p")
            .arg("-") // prompt on stdin
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(model) = &self.model {
            cmd.arg("-m").arg(model);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Other(format!("failed to spawn agent subprocess: {e}")))?;

        // Feed the prompt on stdin, then close it so the shim's read() returns.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(msg.text.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Other("agent subprocess stdout not captured".into()))?;
        let mut lines = BufReader::new(stdout).lines();

        // Track whether any streamed text arrived, so a turn that only produced
        // a final_response (no deltas) still delivers its text.
        let mut streamed_text = false;
        let mut turn_error: Option<String> = None;

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: BridgeEvent = match serde_json::from_str(line) {
                Ok(ev) => ev,
                Err(e) => {
                    warn!(%line, error = %e, "unparseable agent event line, skipping");
                    continue;
                }
            };

            match map_bridge_event(event, &mut streamed_text) {
                Mapped::Emit(events_out) => {
                    for ev in events_out {
                        let _ = events.send(ev).await;
                    }
                }
                Mapped::Error(message) => turn_error = Some(message),
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| Error::Other(format!("agent subprocess wait failed: {e}")))?;

        if let Some(message) = turn_error {
            return Err(Error::Other(format!("agent turn error: {message}")));
        }
        if !status.success() {
            return Err(Error::Other(format!(
                "agent subprocess exited with {status}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::Platform;

    fn emit(event: BridgeEvent, streamed: &mut bool) -> Vec<StreamEvent> {
        match map_bridge_event(event, streamed) {
            Mapped::Emit(v) => v,
            Mapped::Error(m) => panic!("unexpected error: {m}"),
        }
    }

    #[test]
    fn text_delta_maps_to_chunk_and_marks_streamed() {
        let mut streamed = false;
        let out = emit(BridgeEvent::TextDelta { text: "hi".into() }, &mut streamed);
        assert!(streamed);
        assert!(matches!(&out[..], [StreamEvent::MessageChunk { text }] if text == "hi"));
    }

    #[test]
    fn thinking_is_dropped() {
        let mut streamed = false;
        let out = emit(
            BridgeEvent::ThinkingDelta {
                text: "reasoning".into(),
            },
            &mut streamed,
        );
        assert!(out.is_empty());
        assert!(!streamed);
    }

    #[test]
    fn tool_events_map_through() {
        let mut streamed = false;
        let start = emit(
            BridgeEvent::ToolStart {
                tool: "bash".into(),
                args: None,
            },
            &mut streamed,
        );
        assert!(
            matches!(&start[..], [StreamEvent::ToolCallChunk { tool_name, .. }] if tool_name == "bash")
        );
        let done = emit(
            BridgeEvent::ToolComplete {
                tool: "bash".into(),
                ok: false,
                duration: 1.5,
            },
            &mut streamed,
        );
        assert!(matches!(
            &done[..],
            [StreamEvent::ToolCallFinished { tool_name, ok: false, duration, .. }]
                if tool_name == "bash" && (*duration - 1.5).abs() < 1e-9
        ));
    }

    #[test]
    fn done_without_deltas_delivers_final_then_stop() {
        let mut streamed = false;
        let out = emit(
            BridgeEvent::Done {
                completed: true,
                final_response: "answer".into(),
            },
            &mut streamed,
        );
        assert!(matches!(
            &out[..],
            [StreamEvent::MessageChunk { text }, StreamEvent::MessageStop { final_: true }]
                if text == "answer"
        ));
    }

    #[test]
    fn done_after_deltas_only_stops() {
        // Text already streamed: don't re-send final_response, just stop.
        let mut streamed = true;
        let out = emit(
            BridgeEvent::Done {
                completed: true,
                final_response: "answer".into(),
            },
            &mut streamed,
        );
        assert!(matches!(
            &out[..],
            [StreamEvent::MessageStop { final_: true }]
        ));
    }

    #[test]
    fn error_event_is_fatal() {
        let mut streamed = false;
        match map_bridge_event(
            BridgeEvent::Error {
                message: "boom".into(),
            },
            &mut streamed,
        ) {
            Mapped::Error(m) => assert_eq!(m, "boom"),
            Mapped::Emit(_) => panic!("expected error"),
        }
    }

    /// End-to-end wiring check that does NOT hit the model: an all-whitespace
    /// prompt makes the bridge shim emit `done` and exit 0 before it imports
    /// the agent or resolves any provider. Proves spawn + JSONL parse + the
    /// terminal MessageStop path. Requires python3 and the repo layout, so it
    /// is ignored by default; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs python3 + repo root; run manually"]
    async fn empty_prompt_terminates_cleanly() {
        // crate dir is rust/crates/hermes-gateway; repo root is three up.
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf();

        let client = SubprocessAgentClient::new("python3", repo_root);
        let msg = Message {
            resolved_session_id: None,
            platform: Platform::Cli,
            channel_id: "t".into(),
            sender_id: "t".into(),
            text: "   ".into(),
            content_parts: None,
            chat_type: None,
            audio_paths: Vec::new(),
            video_paths: Vec::new(),
            workspace_id: None,
            message_id: None,
            thread_id: None,
        };
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(16);
        let run = tokio::spawn(async move { client.run_turn(&msg, &[], tx).await });

        let mut saw_stop = false;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, StreamEvent::MessageStop { final_: true }) {
                saw_stop = true;
            }
        }
        let result = run.await.unwrap();
        assert!(result.is_ok(), "run_turn errored: {result:?}");
        assert!(saw_stop, "expected a terminal MessageStop");
    }
}
