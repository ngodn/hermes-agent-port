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
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::agent::AgentClient;
use crate::dead_targets::DeadTargetRegistry;
use crate::platform::PlatformAdapter;
use crate::slash::{self, SlashDecision};
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
    /// User config, for slash-command gating.
    user_config: Arc<Value>,
    /// Confirmed-unreachable delivery targets: skip sends to them, clear on
    /// success. Shared with the Python gateway's dead_targets.json.
    dead_targets: Arc<DeadTargetRegistry>,
    /// Conversation-history store for stateless backends (None = stateless).
    session_db: Option<Arc<crate::session_db::SessionDb>>,
    /// Durable delivery-obligation ledger (None = disabled / unavailable).
    delivery_ledger: Option<Arc<crate::delivery_ledger::DeliveryLedger>>,
}

impl Dispatcher {
    pub fn new(
        agent: Arc<dyn AgentClient>,
        user_config: Arc<Value>,
        session_db: Option<Arc<crate::session_db::SessionDb>>,
    ) -> Self {
        let dead_path = crate::config_file::hermes_home()
            .join("gateway")
            .join("dead_targets.json");
        // Open the delivery ledger unless disabled by config.
        let ledger = if crate::delivery_ledger::ledger_enabled(&user_config) {
            crate::delivery_ledger::DeliveryLedger::open_default()
                .map(Arc::new)
                .ok()
        } else {
            None
        };
        Self::with_deps(
            agent,
            user_config,
            Arc::new(DeadTargetRegistry::new(dead_path)),
            session_db,
            ledger,
        )
    }

    /// Construct with explicit dependencies (for tests).
    pub fn with_deps(
        agent: Arc<dyn AgentClient>,
        user_config: Arc<Value>,
        dead_targets: Arc<DeadTargetRegistry>,
        session_db: Option<Arc<crate::session_db::SessionDb>>,
        delivery_ledger: Option<Arc<crate::delivery_ledger::DeliveryLedger>>,
    ) -> Self {
        Self {
            agent,
            adapters: HashMap::new(),
            lease: Arc::new(SessionTurnLeaseRegistry::default()),
            generation: AtomicU64::new(0),
            user_config,
            dead_targets,
            session_db,
            delivery_ledger,
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

    /// Deliver text back to the source platform's adapter, if one is registered.
    async fn deliver(&self, to: &Message, text: String) {
        let Some(adapter) = self.adapters.get(&to.platform) else {
            warn!(platform = ?to.platform, "no adapter registered for delivery");
            return;
        };
        let platform_key = adapter.name();

        // Skip a target we have already proven unreachable (self-heals on the
        // next successful send to it).
        if self.dead_targets.is_dead(platform_key, &to.channel_id) {
            info!(platform = platform_key, channel = %to.channel_id, "skipping delivery to dead target");
            return;
        }

        // Durably record the obligation before attempting the send, so a crash
        // between here and the platform ACK can be recovered on restart.
        let obligation = self.delivery_ledger.as_ref().map(|ledger| {
            let session_id = crate::session_db::session_id_for(to.platform, &to.channel_id);
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let oid = crate::delivery_ledger::compute_obligation_id(
                &session_id,
                &now_ns.to_string(),
                &text,
            );
            let _ = ledger.record_obligation(
                &oid,
                &session_id,
                platform_key,
                &to.channel_id,
                to.chat_type.as_deref(),
                &text,
                None,
            );
            let _ = ledger.mark_attempting(&oid);
            oid
        });

        let out = Message {
            platform: to.platform,
            channel_id: to.channel_id.clone(),
            sender_id: to.sender_id.clone(),
            text,
            chat_type: to.chat_type.clone(),
        };
        match adapter.send(&out).await {
            Ok(()) => {
                // A successful send clears any stale dead flag and settles the
                // obligation.
                self.dead_targets.clear(platform_key, &to.channel_id);
                if let (Some(ledger), Some(oid)) = (&self.delivery_ledger, &obligation) {
                    let _ = ledger.mark_delivered(oid);
                }
            }
            Err(err) => {
                // Marking a target dead needs adapter send-error classification
                // (forbidden / not_found), which is not ported yet.
                error!(platform = ?to.platform, ?err, "outbound delivery failed");
                if let (Some(ledger), Some(oid)) = (&self.delivery_ledger, &obligation) {
                    let _ = ledger.mark_failed(oid, &err.to_string());
                }
            }
        }
    }

    async fn handle_turn(&self, msg: Message) {
        // Slash-command gating + built-ins. Refuse a command this sender may not
        // run before spending a turn; answer gateway built-ins directly; let any
        // other allowed command flow to the agent as normal text.
        match slash::evaluate(&self.user_config, &msg) {
            SlashDecision::Denied { command } => {
                info!(platform = ?msg.platform, %command, "slash command denied by policy");
                self.deliver(&msg, slash::denial_text(&command)).await;
                return;
            }
            SlashDecision::Allowed { command } => {
                if let Some(reply) = slash::handle_builtin(&command, &msg, &self.user_config) {
                    self.deliver(&msg, reply).await;
                    return;
                }
            }
            SlashDecision::NotSlash => {}
        }

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

        // Load prior history + record the inbound message for stateless backends.
        let manages = self.agent.manages_history();
        let source = format!("{:?}", msg.platform).to_lowercase();
        let history =
            crate::session_db::begin_turn(self.session_db.as_deref(), manages, &msg, &source);

        // Run the agent turn; it streams events into `tx`.
        let agent = Arc::clone(&self.agent);
        let msg_for_agent = msg.clone();
        let agent_task =
            tokio::spawn(async move { agent.run_turn(&msg_for_agent, &history, tx).await });

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

        // Record the assistant reply for stateless backends (before the silence
        // gate: a silence marker is still part of the transcript history).
        crate::session_db::end_turn(self.session_db.as_deref(), manages, &msg, &reply);

        // Suppress delivery for intentional-silence markers and empty turns.
        if reply.is_empty() || crate::response_filters::is_intentional_silence_response(&reply) {
            return;
        }

        self.deliver(&msg, reply).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use hermes_core::Result;
    use serde_json::json;

    /// Agent stub: emits a fixed reply and counts how many turns it ran, so a
    /// test can assert the agent was (or was not) invoked.
    struct StubAgent {
        reply: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl crate::agent::AgentClient for StubAgent {
        async fn run_turn(
            &self,
            _msg: &Message,
            _history: &[crate::session_db::HistoryMessage],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = tx
                .send(StreamEvent::MessageChunk {
                    text: self.reply.clone(),
                })
                .await;
            let _ = tx.send(StreamEvent::MessageStop { final_: true }).await;
            Ok(())
        }
    }

    /// Adapter stub: records every outbound message.
    struct StubAdapter {
        sent: Arc<Mutex<Vec<Message>>>,
    }

    #[async_trait]
    impl crate::platform::PlatformAdapter for StubAdapter {
        fn name(&self) -> &str {
            "stub"
        }
        async fn run(&self, _inbound: mpsc::Sender<Message>) -> Result<()> {
            Ok(())
        }
        async fn send(&self, msg: &Message) -> Result<()> {
            self.sent.lock().unwrap().push(msg.clone());
            Ok(())
        }
    }

    fn cli_msg(text: &str, sender: &str) -> Message {
        Message {
            platform: Platform::Cli,
            channel_id: "chan".into(),
            sender_id: sender.into(),
            text: text.into(),
            chat_type: Some("dm".into()),
        }
    }

    /// Build a dispatcher with the stub agent+adapter, returning the shared
    /// call counter and outbound record.
    fn harness(
        reply: &str,
        cfg: Value,
    ) -> (Dispatcher, Arc<AtomicUsize>, Arc<Mutex<Vec<Message>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let agent = Arc::new(StubAgent {
            reply: reply.to_string(),
            calls: calls.clone(),
        });
        // Use a throwaway dead-target registry so tests never touch the real
        // HERMES_HOME.
        let mut dead_path = std::env::temp_dir();
        dead_path.push(format!(
            "hermes_disp_dead_{}_{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let dead = Arc::new(crate::dead_targets::DeadTargetRegistry::new(dead_path));
        let mut d = Dispatcher::with_deps(agent, Arc::new(cfg), dead, None, None);
        d.register_adapter(Platform::Cli, Arc::new(StubAdapter { sent: sent.clone() }));
        (d, calls, sent)
    }

    #[tokio::test]
    async fn normal_turn_runs_agent_and_delivers_reply() {
        let (d, calls, sent) = harness("hello there", json!({}));
        d.handle_turn(cli_msg("hi", "u")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let out = sent.lock().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hello there");
        assert_eq!(out[0].channel_id, "chan");
    }

    #[tokio::test]
    async fn silence_marker_suppresses_delivery() {
        let (d, calls, sent) = harness("NO_REPLY", json!({}));
        d.handle_turn(cli_msg("hi", "u")).await;
        // The agent ran, but the silence marker means nothing is delivered.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn denied_slash_command_skips_agent_and_delivers_refusal() {
        let cfg = json!({"platforms": {"cli": {"extra": {
            "allow_admin_from": ["admin"],
            "user_allowed_commands": ["status"]
        }}}});
        let (d, calls, sent) = harness("should not run", cfg);
        d.handle_turn(cli_msg("/deploy", "nonadmin")).await;
        // Denied before any turn is spent.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let out = sent.lock().unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].text.contains("not allowed"));
    }

    #[tokio::test]
    async fn builtin_status_answered_without_agent() {
        let (d, calls, sent) = harness("should not run", json!({}));
        d.handle_turn(cli_msg("/status", "u")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let out = sent.lock().unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].text.contains("online"));
    }

    #[tokio::test]
    async fn delivery_to_dead_target_is_skipped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let agent = Arc::new(StubAgent {
            reply: "hello".into(),
            calls: calls.clone(),
        });
        let mut dead_path = std::env::temp_dir();
        dead_path.push(format!(
            "hermes_disp_deadskip_{}_{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let dead = Arc::new(crate::dead_targets::DeadTargetRegistry::new(dead_path));
        // The stub adapter's name() is "stub"; mark the target dead under it.
        dead.mark_dead("stub", "chan", "test");
        let mut d = Dispatcher::with_deps(agent, Arc::new(json!({})), dead, None, None);
        d.register_adapter(Platform::Cli, Arc::new(StubAdapter { sent: sent.clone() }));

        d.handle_turn(cli_msg("hi", "u")).await;
        // The agent still runs, but delivery is skipped because the target is dead.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn successful_delivery_records_a_delivered_obligation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let agent = Arc::new(StubAgent {
            reply: "durable hello".into(),
            calls: calls.clone(),
        });
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut dead_path = std::env::temp_dir();
        dead_path.push(format!(
            "hermes_disp_led_dead_{}_{uniq}.json",
            std::process::id()
        ));
        let dead = Arc::new(crate::dead_targets::DeadTargetRegistry::new(dead_path));
        let mut led_path = std::env::temp_dir();
        led_path.push(format!(
            "hermes_disp_led_{}_{uniq}/state.db",
            std::process::id()
        ));
        let ledger =
            Arc::new(crate::delivery_ledger::DeliveryLedger::open(led_path.clone()).unwrap());

        let mut d =
            Dispatcher::with_deps(agent, Arc::new(json!({})), dead, None, Some(ledger.clone()));
        d.register_adapter(Platform::Cli, Arc::new(StubAdapter { sent: sent.clone() }));

        d.handle_turn(cli_msg("hi", "u")).await;
        assert_eq!(sent.lock().unwrap().len(), 1);
        // The obligation was recorded and settled as delivered.
        assert_eq!(ledger.count_state("delivered").unwrap(), 1);
        assert_eq!(ledger.count_state("pending").unwrap(), 0);
        let _ = std::fs::remove_dir_all(led_path.parent().unwrap());
    }
}
