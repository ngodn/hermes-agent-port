//! The gateway -> agent boundary.
//!
// WIP scaffold: wired in as the agent side is ported. Allow dead code for now.
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
//!   lifecycle.
//! * a native in-process agent client lands later, once `run_agent.py` itself
//!   is ported, and swaps in behind the same trait.

use async_trait::async_trait;
use hermes_core::{Message, Result};
use tokio::sync::mpsc;

/// One streamed piece of an agent turn. Kept minimal for now; richer event
/// kinds (tool calls, reasoning, usage) are added as the agent side is ported.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A chunk of assistant text.
    Text(String),
    /// The turn finished normally.
    Done,
    /// The turn failed; carries a human-readable reason.
    Error(String),
}

/// Drives a single agent turn for an inbound message, streaming events out.
#[async_trait]
pub trait AgentClient: Send + Sync {
    async fn run_turn(&self, msg: &Message, events: mpsc::Sender<AgentEvent>) -> Result<()>;
}

/// Strangler-step client: runs the existing Python agent as a subprocess.
///
/// The exact entrypoint/args/env are being confirmed against the Python CLI
/// (see rust/tools/tasks/agent-invocation.txt); `program`/`base_args` are wired
/// once that lands. Until then this is a documented placeholder that returns a
/// clear error rather than pretending to run.
pub struct SubprocessAgentClient {
    pub program: String,
    pub base_args: Vec<String>,
}

impl SubprocessAgentClient {
    pub fn new(program: impl Into<String>, base_args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            base_args,
        }
    }
}

#[async_trait]
impl AgentClient for SubprocessAgentClient {
    async fn run_turn(&self, _msg: &Message, events: mpsc::Sender<AgentEvent>) -> Result<()> {
        // TODO(port): spawn `self.program self.base_args...` with the turn
        // payload, parse the agent's stream-json output into AgentEvent, and
        // forward. Blocked on confirming the non-interactive invocation.
        let _ = events
            .send(AgentEvent::Error(
                "subprocess agent client not yet wired: pending Python invocation spec".into(),
            ))
            .await;
        Ok(())
    }
}
