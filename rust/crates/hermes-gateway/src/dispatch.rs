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
#[derive(Clone)]
pub struct Dispatcher {
    agent: Arc<dyn AgentClient>,
    /// Adapters keyed by platform, used for outbound delivery.
    adapters: HashMap<Platform, Arc<dyn PlatformAdapter>>,
    /// Serializes turns per resolved session so two routing keys mapped to one
    /// session never interleave their transcript flushes (see turn_lease).
    lease: Arc<SessionTurnLeaseRegistry>,
    route_lease: Arc<SessionTurnLeaseRegistry>,
    /// Monotonic per-turn generation, for lease ownership diagnostics.
    generation: Arc<AtomicU64>,
    /// User config, for slash-command gating.
    user_config: Arc<Value>,
    /// Confirmed-unreachable delivery targets: skip sends to them, clear on
    /// success. Shared with the Python gateway's dead_targets.json.
    dead_targets: Arc<DeadTargetRegistry>,
    /// Conversation-history store for stateless backends (None = stateless).
    session_db: Option<Arc<crate::session_db::SessionDb>>,
    session_store: Option<(Arc<crate::session_store::SessionStore>, f64)>,
    /// Durable delivery-obligation ledger (None = disabled / unavailable).
    delivery_ledger: Option<Arc<crate::delivery_ledger::DeliveryLedger>>,
    /// Inbound-audio transcription backend (None = STT not configured; audio
    /// attachments are then left untranscribed and the turn runs on the caption).
    transcription: Option<Arc<dyn crate::transcription_enrichment::TranscriptionBackend>>,
    slash_confirmations: Arc<crate::slash_confirm::SlashConfirmations>,
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
            route_lease: Arc::new(SessionTurnLeaseRegistry::default()),
            generation: Arc::new(AtomicU64::new(0)),
            user_config,
            dead_targets,
            session_db,
            session_store: None,
            delivery_ledger,
            transcription: None,
            slash_confirmations: Arc::new(crate::slash_confirm::SlashConfirmations::new(
                crate::config_file::config_path(),
            )),
        }
    }

    /// Share turn ownership across every ingress serving the same transcripts.
    pub fn with_turn_leases(
        mut self,
        leases: Arc<SessionTurnLeaseRegistry>,
        route_leases: Arc<SessionTurnLeaseRegistry>,
        generation: Arc<AtomicU64>,
    ) -> Self {
        self.lease = leases;
        self.route_lease = route_leases;
        self.generation = generation;
        self
    }

    /// Use durable session routing for backends whose history the gateway owns.
    /// Freshness is supplied from the selected gateway configuration.
    pub fn with_session_store(
        mut self,
        store: Arc<crate::session_store::SessionStore>,
        freshness_seconds: f64,
    ) -> Self {
        self.session_store = Some((store, freshness_seconds));
        self
    }

    /// Install the inbound-audio transcription backend. When set, a message
    /// carrying `audio_paths` is transcribed before its turn runs.
    pub fn with_transcription(
        mut self,
        backend: Arc<dyn crate::transcription_enrichment::TranscriptionBackend>,
    ) -> Self {
        self.transcription = Some(backend);
        self
    }

    /// Share the confirmation registry with every other ingress path.
    pub fn with_slash_confirmations(
        mut self,
        confirmations: Arc<crate::slash_confirm::SlashConfirmations>,
    ) -> Self {
        self.slash_confirmations = confirmations;
        self
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
            let session_id = crate::session_db::message_session_id(to);
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
            resolved_session_id: to.resolved_session_id.clone(),
            platform: to.platform,
            channel_id: to.channel_id.clone(),
            sender_id: to.sender_id.clone(),
            text,
            content_parts: None,
            chat_type: to.chat_type.clone(),
            audio_paths: Vec::new(),
            video_paths: Vec::new(),
            workspace_id: to.workspace_id.clone(),
            message_id: to.message_id.clone(),
            thread_id: to.thread_id.clone(),
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
        let mut msg = msg;
        // Inbound audio: transcribe attached voice notes before anything else so
        // the turn (and command/slash handling) sees the transcript as its text.
        // Mirrors GatewayRunner._enrich_message_with_transcription: with no
        // backend configured, or on an unexpected enrichment error, the caption
        // is left as-is rather than dropping the message.
        if !msg.audio_paths.is_empty() {
            if let Some(backend) = &self.transcription {
                let stt_enabled = self
                    .user_config
                    .get("stt_enabled")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                match crate::transcription_enrichment::enrich_message_with_transcription(
                    &msg.text,
                    &msg.audio_paths,
                    stt_enabled,
                    true,
                    backend.as_ref(),
                )
                .await
                {
                    Ok((enriched, _transcripts)) => msg.text = enriched,
                    Err(err) => {
                        tracing::warn!(%err, "inbound transcription enrichment failed; using caption");
                    }
                }
            }
        }

        // Confirmation replies are gateway control input and never enter the
        // transcript or model context. Keep this ahead of ordinary slash
        // dispatch. Native blocking tool approvals will take precedence here
        // once that runtime is ported, matching Python's ordering contract.
        if let Some((store, _)) = &self.session_store {
            let deps = crate::session_admission::AdmissionDeps {
                store: store.clone(),
                transcript_leases: self.lease.clone(),
                route_leases: self.route_lease.clone(),
                generation: self.generation.clone(),
            };
            if let Some(result) = crate::session_commands::resolve_reset_confirmation(
                &self.slash_confirmations,
                deps,
                self.agent.clone(),
                crate::session::source_from_message(&msg),
                &msg,
                &self.user_config,
                false,
            )
            .await
            {
                self.deliver(&msg, result.reply).await;
                return;
            }
        }

        // Slash-command gating + built-ins. Refuse a command this sender may not
        // run before spending a turn; answer gateway built-ins directly; let any
        // other allowed command flow to the agent as normal text.
        let mut native_command = None;
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
                native_command = slash::native_command(&command, &msg.text);
            }
            SlashDecision::NotSlash => {}
        }

        if let Some(crate::slash::NativeSlashCommand::Title { raw_title }) = native_command.clone()
        {
            let Some((store, freshness)) = &self.session_store else {
                self.deliver(&msg, "Session database not available.".into())
                    .await;
                return;
            };
            match crate::session_commands::title_session(crate::session_commands::TitleCommand {
                deps: crate::session_admission::AdmissionDeps {
                    store: store.clone(),
                    transcript_leases: self.lease.clone(),
                    route_leases: self.route_lease.clone(),
                    generation: self.generation.clone(),
                },
                source: crate::session::source_from_message(&msg),
                owner_key: msg.sender_id.clone(),
                raw_title,
                freshness_seconds: *freshness,
            })
            .await
            {
                Ok(result) => self.deliver(&msg, result.reply).await,
                Err(error) => {
                    warn!(%error, "push session title failed");
                    self.deliver(&msg, "Session title failed. Please try again.".into())
                        .await;
                }
            }
            return;
        }

        if let Some(crate::slash::NativeSlashCommand::Resume {
            raw_args,
            from_sessions,
        }) = native_command.clone()
        {
            let Some((store, _)) = &self.session_store else {
                self.deliver(&msg, "Session database not available.".into())
                    .await;
                return;
            };
            match crate::session_commands::resume_session(crate::session_commands::ResumeCommand {
                confirmations: &self.slash_confirmations,
                deps: crate::session_admission::AdmissionDeps {
                    store: store.clone(),
                    transcript_leases: self.lease.clone(),
                    route_leases: self.route_lease.clone(),
                    generation: self.generation.clone(),
                },
                agent: self.agent.clone(),
                source: crate::session::source_from_message(&msg),
                message: &msg,
                user_config: &self.user_config,
                raw_args: &raw_args,
                from_sessions,
            })
            .await
            {
                Ok(result) => self.deliver(&msg, result.reply).await,
                Err(error) => {
                    warn!(%error, "push session resume failed");
                    self.deliver(&msg, "Session resume failed. Please try again.".into())
                        .await;
                }
            }
            return;
        }

        if let Some(crate::slash::NativeSlashCommand::Compress { raw_args }) =
            native_command.clone()
        {
            let Some((store, freshness)) = &self.session_store else {
                self.deliver(&msg, "Session database not available.".into())
                    .await;
                return;
            };
            match crate::session_commands::compress_session(
                crate::session_commands::CompressCommand {
                    deps: crate::session_admission::AdmissionDeps {
                        store: store.clone(),
                        transcript_leases: self.lease.clone(),
                        route_leases: self.route_lease.clone(),
                        generation: self.generation.clone(),
                    },
                    agent: self.agent.clone(),
                    source: crate::session::source_from_message(&msg),
                    message: &msg,
                    owner_key: msg.sender_id.clone(),
                    raw_args,
                    freshness_seconds: *freshness,
                    checkpoint_required: crate::python_value::truthy(
                        &self.user_config["compression"]["checkpoint_required"],
                    ),
                },
            )
            .await
            {
                Ok(result) => self.deliver(&msg, result.reply).await,
                Err(error) => {
                    warn!(%error, "push compression failed");
                    self.deliver(
                        &msg,
                        "Compression could not complete. The conversation was not changed.".into(),
                    )
                    .await;
                }
            }
            return;
        }

        if let Some(crate::slash::NativeSlashCommand::Reset { title }) = native_command {
            let Some((store, _)) = &self.session_store else {
                self.deliver(
                    &msg,
                    "Session reset is not available on this backend.".into(),
                )
                .await;
                return;
            };
            match crate::session_commands::reset_or_confirm(
                &self.slash_confirmations,
                crate::session_admission::AdmissionDeps {
                    store: store.clone(),
                    transcript_leases: self.lease.clone(),
                    route_leases: self.route_lease.clone(),
                    generation: self.generation.clone(),
                },
                self.agent.clone(),
                crate::session::source_from_message(&msg),
                &msg.sender_id,
                title,
                crate::slash::typed_command_prefix(msg.platform),
            )
            .await
            {
                Ok(result) => self.deliver(&msg, result.reply).await,
                Err(error) => {
                    warn!(%error, "push session reset failed");
                    self.deliver(&msg, "Session reset failed. Please try again.".into())
                        .await;
                }
            }
            return;
        }

        // Video context belongs to the model turn, after slash policy has seen
        // the original caption. Native adapters cache on the agent's filesystem.
        for path in &msg.video_paths {
            let display = crate::media_context::attachment_display_name(path);
            let note = crate::media_context::build_video_context_note(&display, path);
            msg.text = format!("{note}\n\n{}", msg.text);
            if let Some(parts) = &mut msg.content_parts {
                parts.insert(0, hermes_core::ContentPart::Text { text: note });
            }
        }

        if msg.content_parts.is_some() && !self.agent.supports_structured_content() {
            self.deliver(
                &msg,
                "Configured agent backend does not accept structured content.".into(),
            )
            .await;
            return;
        }

        let manages = self.agent.manages_history();
        let mut turn_db = self.session_db.clone();
        let mut routing_key = None;
        let mut session_finalizable = false;
        let mut admitted_lease = None;
        let mut admitted_durable_lease = None;
        if !manages {
            if let Some((store, freshness)) = &self.session_store {
                let source = crate::session::source_from_message(&msg);
                let legacy_id = crate::session_db::message_session_id(&msg);
                let legacy_source = format!("{:?}", msg.platform).to_lowercase();
                let resolved = crate::session_admission::admit_turn(
                    crate::session_admission::AdmissionDeps {
                        store: store.clone(),
                        transcript_leases: self.lease.clone(),
                        route_leases: self.route_lease.clone(),
                        generation: self.generation.clone(),
                    },
                    source,
                    Some((legacy_id, legacy_source)),
                    *freshness,
                    &msg.sender_id,
                )
                .await;
                match resolved {
                    Ok(resolved) => {
                        msg.resolved_session_id = Some(resolved.entry.session_id);
                        routing_key = Some(resolved.entry.session_key);
                        turn_db = resolved.database;
                        session_finalizable = resolved.finalizable;
                        admitted_lease = resolved.lease;
                        admitted_durable_lease = resolved.durable_lease;
                        if let Some(previous_session_id) = resolved.predecessor_id {
                            if let Some(route_key) = routing_key.as_deref() {
                                self.slash_confirmations.clear(route_key);
                            }
                            self.agent.retire_conversation(
                                crate::agent::TurnContext::from_database(turn_db.as_deref()),
                                &previous_session_id,
                            );
                        }
                    }
                    Err(error) => {
                        warn!(%error, "could not resolve session for inbound turn");
                        return;
                    }
                }
            }
        }

        // Serialize using the same identity as persisted history and cache routing.
        // Full session resolution (switch_session/tip-walk) remains separate.
        // A held lease means a same-session turn is in flight; fail closed on
        // timeout rather than run two turns unserialized on one transcript.
        let session_id = crate::session_db::message_session_id(&msg);
        let _lease = if admitted_lease.is_some() {
            admitted_lease
        } else {
            let generation = self.generation.fetch_add(1, Ordering::Relaxed);
            match self
                .lease
                .acquire(&session_id, &msg.sender_id, generation, None)
                .await
            {
                Ok(token) => token,
                Err(err) => {
                    warn!(%err, "rejecting turn: could not serialize against the in-flight turn");
                    return;
                }
            }
        };

        // An admitted turn outlives cancellation of its ingress waiter. Keep
        // its configured adapters and shared state alive through persistence
        // and delivery, rather than detaching only the inner agent task.
        let owner = self.clone();
        if let Err(error) = tokio::spawn(async move {
            owner
                .run_admitted_turn(
                    msg,
                    turn_db,
                    manages,
                    routing_key,
                    session_finalizable,
                    (_lease, admitted_durable_lease),
                )
                .await;
        })
        .await
        {
            error!(%error, "push turn owner failed");
        }
    }

    async fn run_admitted_turn(
        &self,
        msg: Message,
        turn_db: Option<Arc<crate::session_db::SessionDb>>,
        manages: bool,
        routing_key: Option<String>,
        session_finalizable: bool,
        _leases: (
            Option<crate::turn_lease::TurnLeaseToken>,
            Option<crate::durable_turn_lease::DurableTurnLease>,
        ),
    ) {
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);

        // Load prior history + record the inbound message for stateless backends.
        let source = format!("{:?}", msg.platform).to_lowercase();
        let history = crate::session_db::begin_turn(turn_db.as_deref(), manages, &msg, &source);

        // Run the agent turn; it streams events into `tx`.
        let agent = Arc::clone(&self.agent);
        let turn_agent = agent.clone();
        let agent_db = turn_db.clone();
        let msg_for_agent = msg.clone();
        let agent_task = tokio::spawn(async move {
            turn_agent
                .run_turn_with_context(
                    crate::agent::TurnContext::from_database(agent_db.as_deref())
                        .with_session_finalizable(session_finalizable),
                    &msg_for_agent,
                    &history,
                    tx,
                )
                .await
        });

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

        let succeeded = match agent_task.await {
            Ok(Err(err)) => {
                warn!(platform = ?msg.platform, %err, "agent turn failed");
                false
            }
            Err(err) => {
                error!(?err, "agent task panicked");
                false
            }
            Ok(Ok(())) => true,
        };

        // Record the assistant reply for stateless backends (before the silence
        // gate: a silence marker is still part of the transcript history).
        crate::session_db::end_turn(turn_db.as_deref(), manages, &msg, &reply);
        if let Err(error) = agent
            .finalize_turn_after_persist(
                crate::agent::TurnContext::from_database(turn_db.as_deref())
                    .with_session_finalizable(session_finalizable),
                &msg,
                &reply,
                succeeded,
            )
            .await
        {
            warn!(%error, "agent post-persist finalization failed");
        }
        if let (Some(key), Some((store, _))) = (routing_key, &self.session_store) {
            let store = store.clone();
            match tokio::task::spawn_blocking(move || store.update_session(&key, None, true)).await
            {
                Ok(Ok(())) => {}
                error => warn!(?error, "session activity update failed"),
            }
        }

        // Suppress delivery for intentional-silence markers and empty turns.
        if reply.is_empty() || crate::response_filters::is_intentional_silence_response(&reply) {
            return;
        }

        self.deliver(&msg, reply).await;
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn cancelled_push_waiter_keeps_history_and_delivery_owned() {
        struct FinishingAgent {
            entered: tokio::sync::Notify,
            finish: tokio::sync::Notify,
        }
        #[async_trait]
        impl crate::agent::AgentClient for FinishingAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                tx: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                tx.send(StreamEvent::MessageChunk {
                    text: "completed answer".into(),
                })
                .await
                .unwrap();
                tx.send(StreamEvent::MessageStop { final_: true })
                    .await
                    .unwrap();
                self.entered.notify_one();
                self.finish.notified().await;
                Ok(())
            }
        }
        let path = std::env::temp_dir().join(format!(
            "hermes-push-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Arc::new(crate::session_db::SessionDb::open(path.join("state.db")).unwrap());
        let agent = Arc::new(FinishingAgent {
            entered: tokio::sync::Notify::new(),
            finish: tokio::sync::Notify::new(),
        });
        let (mut dispatcher, _, sent) = harness("unused", json!({}));
        dispatcher.agent = agent.clone();
        dispatcher.session_db = Some(db.clone());
        let waiter = dispatcher.clone();
        let task = tokio::spawn(async move {
            waiter.handle_turn(cli_msg("question", "U")).await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), agent.entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(dispatcher
            .lease
            .acquire(
                "cli:chan",
                "next-route",
                2,
                Some(std::time::Duration::from_millis(50))
            )
            .await
            .is_err());
        agent.finish.notify_one();
        let next = dispatcher
            .lease
            .acquire("cli:chan", "next-route", 3, None)
            .await
            .unwrap();
        let history = db.load_history("cli:chan", 0).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "completed answer");
        assert_eq!(sent.lock().unwrap()[0].text, "completed answer");
        drop(next);
        drop(dispatcher);
        drop(db);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn coordinated_dispatch_resumes_durable_history_after_store_restart() {
        struct HistoryAgent {
            seen: Arc<Mutex<Vec<(String, usize)>>>,
            finalized: Arc<Mutex<Vec<(String, String)>>>,
        }
        #[async_trait]
        impl crate::agent::AgentClient for HistoryAgent {
            async fn run_turn_with_context(
                &self,
                context: crate::agent::TurnContext<'_>,
                msg: &Message,
                history: &[crate::session_db::HistoryMessage],
                tx: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                assert!(context
                    .home
                    .expect("resolved profile home")
                    .join("state.db")
                    .is_file());
                self.run_turn(msg, history, tx).await
            }

            async fn run_turn(
                &self,
                msg: &Message,
                history: &[crate::session_db::HistoryMessage],
                tx: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                self.seen.lock().unwrap().push((
                    msg.resolved_session_id
                        .clone()
                        .expect("coordinator must assign identity"),
                    history.len(),
                ));
                tx.send(StreamEvent::MessageChunk {
                    text: "answer".into(),
                })
                .await
                .unwrap();
                tx.send(StreamEvent::MessageStop { final_: true })
                    .await
                    .unwrap();
                Ok(())
            }

            async fn finalize_turn_after_persist(
                &self,
                context: crate::agent::TurnContext<'_>,
                msg: &Message,
                reply: &str,
                succeeded: bool,
            ) -> Result<()> {
                assert!(succeeded);
                let history = context
                    .database
                    .unwrap()
                    .load_history(&crate::session_db::message_session_id(msg), 0)
                    .unwrap();
                let last = history.last().unwrap();
                assert_eq!(last.role, "assistant");
                assert_eq!(last.content, reply);
                self.finalized
                    .lock()
                    .unwrap()
                    .push((last.role.clone(), last.content.clone()));
                Ok(())
            }
        }
        let home = std::env::temp_dir().join(format!(
            "hermes-dispatch-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = crate::config_gateway::GatewayConfig {
            sessions_dir: home.join("sessions"),
            ..Default::default()
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let finalized = Arc::new(Mutex::new(Vec::new()));
        for text in ["first", "second"] {
            let store = Arc::new(
                crate::session_store::SessionStore::open(
                    config.clone(),
                    home.clone(),
                    home.clone(),
                    "default".into(),
                    |_| Ok(false),
                )
                .unwrap(),
            );
            let (mut dispatcher, _, sent) = harness("", json!({}));
            dispatcher.agent = Arc::new(HistoryAgent {
                seen: seen.clone(),
                finalized: finalized.clone(),
            });
            let dispatcher = dispatcher.with_session_store(store, 3600.0);
            dispatcher.handle_turn(cli_msg(text, "user")).await;
            let replies = sent.lock().unwrap();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].text, "answer");
            assert!(replies[0].resolved_session_id.is_some());
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, seen[1].0);
        assert_eq!((seen[0].1, seen[1].1), (0, 2));
        assert_eq!(
            *finalized.lock().unwrap(),
            [
                ("assistant".into(), "answer".into()),
                ("assistant".into(), "answer".into())
            ]
        );
        let db = crate::session_db::SessionDb::open_shared(home.join("state.db")).unwrap();
        assert_eq!(db.load_history(&seen[0].0, 10).unwrap().len(), 4);
        assert!(db.get_session("cli:chan").unwrap().is_none());
        drop(seen);
        drop(db);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn same_channel_id_on_different_platforms_can_run_concurrently() {
        struct GateAgent {
            entered: mpsc::Sender<Platform>,
            release: Arc<tokio::sync::Semaphore>,
        }
        #[async_trait::async_trait]
        impl crate::agent::AgentClient for GateAgent {
            async fn run_turn(
                &self,
                msg: &Message,
                _history: &[crate::session_db::HistoryMessage],
                tx: mpsc::Sender<StreamEvent>,
            ) -> hermes_core::Result<()> {
                self.entered.send(msg.platform).await.unwrap();
                self.release.acquire().await.unwrap().forget();
                tx.send(StreamEvent::MessageStop { final_: true })
                    .await
                    .unwrap();
                Ok(())
            }
        }
        let (mut dispatcher, _, _) = harness("", serde_json::json!({}));
        let (entered, mut arrivals) = mpsc::channel(2);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        dispatcher.agent = Arc::new(GateAgent {
            entered,
            release: release.clone(),
        });
        let mut telegram = cli_msg("hello", "sender");
        telegram.platform = Platform::Telegram;
        telegram.channel_id = "same-channel".into();
        let mut discord = telegram.clone();
        discord.platform = Platform::Discord;
        let control = async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let first = arrivals.recv().await.unwrap();
                let second = arrivals.recv().await.unwrap();
                assert_ne!(first, second);
            })
            .await
            .expect("distinct persisted sessions must enter independently");
            release.add_permits(2);
        };
        tokio::join!(
            dispatcher.handle_turn(telegram),
            dispatcher.handle_turn(discord),
            control
        );
    }

    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use hermes_core::Result;
    use serde_json::json;

    /// Agent stub that echoes back the message text it received, so a test can
    /// assert what text actually reached the turn (e.g. after enrichment).
    struct EchoAgent;

    #[async_trait]
    impl crate::agent::AgentClient for EchoAgent {
        async fn run_turn(
            &self,
            msg: &Message,
            _history: &[crate::session_db::HistoryMessage],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            let _ = tx
                .send(StreamEvent::MessageChunk {
                    text: msg.text.clone(),
                })
                .await;
            let _ = tx.send(StreamEvent::MessageStop { final_: true }).await;
            Ok(())
        }
    }

    /// Transcription backend stub returning a fixed transcript for any clip.
    struct FakeTranscription;

    #[async_trait]
    impl crate::transcription_enrichment::TranscriptionBackend for FakeTranscription {
        fn absolute_path(&self, path: &str) -> String {
            path.to_string()
        }
        async fn probe_duration(&self, _abs_path: &str) -> Option<String> {
            None
        }
        async fn transcribe(&self, _path: &str) -> anyhow::Result<Value> {
            Ok(json!({"success": true, "transcript": "hello from audio"}))
        }
        async fn local_fallback(&self, _path: &str) -> anyhow::Result<Value> {
            Ok(json!({"success": false}))
        }
        fn agent_visible_path(&self, abs_path: &str) -> String {
            abs_path.to_string()
        }
    }

    #[tokio::test]
    async fn video_context_reaches_agent_without_hiding_commands() {
        let (mut dispatcher, _calls, sent) = harness("", json!({}));
        dispatcher.agent = Arc::new(EchoAgent);
        let mut msg = cli_msg("describe this", "sender");
        msg.video_paths = vec!["/tmp/video_fixture.mp4".into()];
        msg.message_id = Some("child".into());
        msg.thread_id = Some("root".into());
        dispatcher.handle_turn(msg.clone()).await;
        {
            let out = sent.lock().unwrap();
            assert_eq!(out[0].message_id.as_deref(), Some("child"));
            assert_eq!(out[0].thread_id.as_deref(), Some("root"));
            assert!(out[0].text.contains("/tmp/video_fixture.mp4"));
            assert!(out[0].text.ends_with("\n\ndescribe this"));
        }
        msg.text = "/help".into();
        dispatcher.handle_turn(msg).await;
        let out = sent.lock().unwrap();
        assert_eq!(out.len(), 2);
        assert!(!out[1].text.contains("video_fixture"));
    }

    // A message carrying audio is transcribed before the turn: the agent (and
    // thus the outbound reply that echoes it) sees the transcript, not the
    // empty caption.
    #[tokio::test]
    async fn inbound_audio_is_transcribed_before_the_turn() {
        let (mut dispatcher, _calls, sent) = harness("", json!({}));
        dispatcher.agent = Arc::new(EchoAgent);
        let dispatcher = dispatcher.with_transcription(Arc::new(FakeTranscription));

        let mut msg = cli_msg("", "sender");
        msg.audio_paths = vec!["/tmp/voice.ogg".into()];
        dispatcher.handle_turn(msg).await;

        let out = sent.lock().unwrap();
        assert_eq!(out.len(), 1, "one reply delivered");
        assert!(
            out[0].text.contains("hello from audio"),
            "turn ran on the transcript, got: {:?}",
            out[0].text
        );
    }

    // With no backend configured, an audio message still runs (on its caption),
    // never dropped.
    #[tokio::test]
    async fn inbound_audio_without_backend_uses_caption() {
        let (mut dispatcher, _calls, sent) = harness("", json!({}));
        dispatcher.agent = Arc::new(EchoAgent);
        let mut msg = cli_msg("just a caption", "sender");
        msg.audio_paths = vec!["/tmp/voice.ogg".into()];
        dispatcher.handle_turn(msg).await;
        let out = sent.lock().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "just a caption");
    }

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

        async fn summarize_context(
            &self,
            _context: crate::agent::TurnContext<'_>,
            _msg: &Message,
            _history: &[crate::session_db::CompressionHistoryMessage],
            _focus_topic: Option<&str>,
        ) -> Result<Option<String>> {
            Ok(Some("## Goal\nContinue after compression.".into()))
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
            resolved_session_id: None,
            platform: Platform::Cli,
            channel_id: "chan".into(),
            sender_id: sender.into(),
            text: text.into(),
            content_parts: None,
            chat_type: Some("dm".into()),
            audio_paths: Vec::new(),
            video_paths: Vec::new(),
            workspace_id: None,
            message_id: None,
            thread_id: None,
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
    async fn push_title_reset_and_resume_skip_model_control_traffic() {
        let home = std::env::temp_dir().join(format!(
            "hermes-push-reset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                crate::config_gateway::GatewayConfig {
                    sessions_dir: home.join("sessions"),
                    ..Default::default()
                },
                home.clone(),
                home.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let (dispatcher, calls, sent) = harness("answer", json!({}));
        let dispatcher = dispatcher
            .with_session_store(store.clone(), 3600.0)
            .with_slash_confirmations(Arc::new(crate::slash_confirm::SlashConfirmations::new(
                home.join("config.yaml"),
            )));
        dispatcher.handle_turn(cli_msg("first", "u")).await;
        let source = crate::session::source_from_message(&cli_msg("", "u"));
        let first_id = store.current_entry_for_source(&source).unwrap().session_id;
        let db = store
            .database_for_key(&store.session_key_for_source(&source))
            .unwrap();
        dispatcher
            .handle_turn(cli_msg("/title First Work", "u"))
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            db.get_session_title(&first_id).unwrap().as_deref(),
            Some("First Work")
        );
        dispatcher.handle_turn(cli_msg("/sessions all", "u")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sent
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .text
            .contains("requires a configured admin"));

        dispatcher.handle_turn(cli_msg("/reset", "u")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            first_id
        );
        dispatcher.handle_turn(cli_msg("/approve", "u")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let second_id = store.current_entry_for_source(&source).unwrap().session_id;
        assert_ne!(second_id, first_id);
        {
            let output = sent.lock().unwrap();
            assert_eq!(output.len(), 5);
            assert!(output[3].text.contains("Confirm /new"));
            assert!(output[4].text.contains("Session reset"));
        }
        assert_eq!(
            db.get_session(&first_id).unwrap().unwrap()["end_reason"],
            "session_reset"
        );
        dispatcher.handle_turn(cli_msg("second", "u")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        dispatcher
            .handle_turn(cli_msg("/resume First Work", "u"))
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            first_id
        );
        assert!(sent
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .text
            .contains("Resumed session **First Work**"));
        assert!(db.get_session(&second_id).unwrap().unwrap()["end_reason"]
            .as_str()
            .is_some_and(|reason| reason == "session_switch"));
        drop(db);
        drop(dispatcher);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn push_compression_preview_and_rotation_match_http_control_flow() {
        let home = std::env::temp_dir().join(format!(
            "hermes-push-compress-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                crate::config_gateway::GatewayConfig {
                    sessions_dir: home.join("sessions"),
                    ..Default::default()
                },
                home.clone(),
                home.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let (dispatcher, calls, sent) = harness("answer", json!({}));
        let dispatcher = dispatcher.with_session_store(store.clone(), 3600.0);
        for index in 0..4 {
            dispatcher
                .handle_turn(cli_msg(
                    &format!("turn {index} {}", "detail ".repeat(100)),
                    "u",
                ))
                .await;
        }
        let source = crate::session::source_from_message(&cli_msg("", "u"));
        let parent = store.current_entry_for_source(&source).unwrap().session_id;
        dispatcher
            .handle_turn(cli_msg("/compress --preview here 1", "u"))
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(sent.lock().unwrap().last().unwrap().text.contains("6 of 8"));
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            parent
        );

        dispatcher
            .handle_turn(cli_msg("/compress here 1", "u"))
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let child = store.current_entry_for_source(&source).unwrap().session_id;
        assert_ne!(child, parent);
        let db = store
            .database_for_key(&store.session_key_for_source(&source))
            .unwrap();
        assert_eq!(
            db.get_session(&parent).unwrap().unwrap()["end_reason"],
            "compression"
        );
        assert_eq!(db.load_history(&child, 0).unwrap().len(), 4);
        assert!(sent
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .text
            .contains("6 message(s) summarized"));
        drop(db);
        drop(dispatcher);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
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
