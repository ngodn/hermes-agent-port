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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use hermes_core::{Message, Platform, StreamEvent};
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::agent::AgentClient;
use crate::platform::PlatformAdapter;
use crate::turn_lease::SessionTurnLeaseRegistry;

/// Owns the inbound channel and routes turns to the agent and back out.
pub struct Dispatcher {
    agent: Arc<dyn AgentClient>,
    /// Adapters keyed by platform, used for outbound delivery.
    adapters: HashMap<Platform, Arc<dyn PlatformAdapter>>,
    /// Serializes turns per resolved session so two routing keys mapped to one
    /// session never interleave their transcript flushes (see turn_lease).
    lease: Arc<SessionTurnLeaseRegistry>,
    /// Monotonic per-turn generation, for lease ownership diagnostics.
    generation: AtomicU64,
}

impl Dispatcher {
    pub fn new(agent: Arc<dyn AgentClient>) -> Self {
        Self {
            agent,
            adapters: HashMap::new(),
            lease: Arc::new(SessionTurnLeaseRegistry::default()),
            generation: AtomicU64::new(0),
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
        // Serialize per resolved session. The channel_id is the session proxy
        // until full session resolution (switch_session/tip-walk) is ported.
        // A held lease means a same-session turn is in flight; fail closed on
        // timeout rather than run two turns unserialized on one transcript.
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let _lease = match self
            .lease
            .acquire(&msg.channel_id, &msg.sender_id, generation, None)
            .await
        {
            Ok(token) => token, // held for the turn; released on drop below
            Err(err) => {
                warn!(%err, "rejecting turn: could not serialize against the in-flight turn");
                return;
            }
        };

        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);

        // Run the agent turn; it streams events into `tx`.
        let agent = Arc::clone(&self.agent);
        let msg_for_agent = msg.clone();
        let agent_task = tokio::spawn(async move { agent.run_turn(&msg_for_agent, tx).await });

        // Accumulate assistant text and deliver it back to the source platform.
        // Streaming partial deliveries (native drafts, edit-in-place) and tool
        // chrome come later; for now we buffer the turn's text and Commentary
        // into one reply, which every adapter supports. The turn ends on a
        // terminal MessageStop; other event kinds are presentation we don't
        // render yet.
        let mut reply = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::MessageChunk { text } => reply.push_str(&text),
                StreamEvent::Commentary { text } => {
                    if !reply.is_empty() {
                        reply.push_str("\n\n");
                    }
                    reply.push_str(&text);
                }
                StreamEvent::MessageStop { final_: true } => break,
                StreamEvent::MessageStop { final_: false } => reply.push_str("\n\n"),
                // Tool chrome, hints and notices: not rendered in this pass.
                StreamEvent::ToolCallChunk { .. }
                | StreamEvent::ToolCallFinished { .. }
                | StreamEvent::LongToolHint { .. }
                | StreamEvent::GatewayNotice { .. } => {}
            }
        }

        match agent_task.await {
            Ok(Err(err)) => warn!(platform = ?msg.platform, %err, "agent turn failed"),
            Err(err) => error!(?err, "agent task panicked"),
            Ok(Ok(())) => {}
        }

        // Suppress delivery for intentional-silence markers and empty turns.
        if reply.is_empty() || crate::response_filters::is_intentional_silence_response(&reply) {
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
