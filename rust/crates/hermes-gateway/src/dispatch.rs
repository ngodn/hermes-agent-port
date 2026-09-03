//! The gateway turn-dispatch loop.
//!
// WIP scaffold: wired to adapters/agent as those land. Allow dead code for now.
#![allow(dead_code)]
//!
//! This is the spine of the gateway: platform adapters push inbound
//! [`Message`]s onto a shared channel; the dispatcher runs each through the
//! [`AgentClient`] and streams the resulting [`AgentEvent`]s back out to the
//! adapter that owns the originating platform. It mirrors the role of
//! `GatewayRunner._run_agent_inner` in `gateway/run.py`, minus (for now) the
//! session/lease/queue machinery, which is ported on top of this.

use std::collections::HashMap;
use std::sync::Arc;

use hermes_core::{Message, Platform};
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::agent::{AgentClient, AgentEvent};
use crate::platform::PlatformAdapter;

/// Owns the inbound channel and routes turns to the agent and back out.
pub struct Dispatcher {
    agent: Arc<dyn AgentClient>,
    /// Adapters keyed by platform, used for outbound delivery.
    adapters: HashMap<Platform, Arc<dyn PlatformAdapter>>,
}

impl Dispatcher {
    pub fn new(agent: Arc<dyn AgentClient>) -> Self {
        Self {
            agent,
            adapters: HashMap::new(),
        }
    }

    pub fn register_adapter(&mut self, platform: Platform, adapter: Arc<dyn PlatformAdapter>) {
        self.adapters.insert(platform, adapter);
    }

    /// Consume inbound messages until the channel closes, handling each turn.
    /// Turns are spawned so a slow one does not head-of-line block the queue;
    /// per-session ordering is layered on later (see `gateway/turn_lease.py`).
    pub async fn run(self: Arc<Self>, mut inbound: mpsc::Receiver<Message>) {
        while let Some(msg) = inbound.recv().await {
            let this = Arc::clone(&self);
            tokio::spawn(async move {
                this.handle_turn(msg).await;
            });
        }
    }

    async fn handle_turn(&self, msg: Message) {
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

        // Run the agent turn; it streams events into `tx`.
        let agent = Arc::clone(&self.agent);
        let msg_for_agent = msg.clone();
        let agent_task = tokio::spawn(async move { agent.run_turn(&msg_for_agent, tx).await });

        // Accumulate assistant text and deliver it back to the source platform.
        // Streaming partial deliveries (typing/edits) come later; for now we
        // buffer the turn and send one reply, which every adapter supports.
        let mut reply = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Text(chunk) => reply.push_str(&chunk),
                AgentEvent::Done => break,
                AgentEvent::Error(reason) => {
                    warn!(platform = ?msg.platform, %reason, "agent turn errored");
                    reply.push_str(&reason);
                    break;
                }
            }
        }

        if let Err(err) = agent_task.await {
            error!(?err, "agent task panicked");
        }

        if reply.is_empty() {
            return;
        }

        match self.adapters.get(&msg.platform) {
            Some(adapter) => {
                let out = Message {
                    platform: msg.platform,
                    channel_id: msg.channel_id.clone(),
                    sender_id: msg.sender_id.clone(),
                    text: reply,
                };
                if let Err(err) = adapter.send(&out).await {
                    error!(platform = ?msg.platform, ?err, "outbound delivery failed");
                }
            }
            None => warn!(platform = ?msg.platform, "no adapter registered for delivery"),
        }
    }
}
