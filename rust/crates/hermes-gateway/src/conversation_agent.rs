//! Select a native client from the internally resolved conversation profile.
//! Clients retain their configuration across turns; resets receive new IDs.

use crate::agent::AgentClient;
use async_trait::async_trait;
use hermes_core::stream::StreamEvent;
use hermes_core::{Message, Result};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;

type FactoryFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Arc<dyn AgentClient>>> + Send + 'a>>;
type Factory = dyn for<'a> Fn(
        &'a Path,
        &'a Message,
        &'a [crate::session_db::HistoryMessage],
        Option<&'a crate::session_db::SessionDb>,
    ) -> FactoryFuture<'a>
    + Send
    + Sync;
type ClientCell = tokio::sync::OnceCell<Arc<dyn AgentClient>>;
type Clients = HashMap<(PathBuf, String), Arc<ClientCell>>;

pub struct ConversationAgent {
    fallback: Arc<dyn AgentClient>,
    factory: Box<Factory>,
    clients: tokio::sync::Mutex<Clients>,
}

impl ConversationAgent {
    pub fn new(
        fallback: Arc<dyn AgentClient>,
        factory: impl for<'a> Fn(
                &'a Path,
                &'a Message,
                &'a [crate::session_db::HistoryMessage],
                Option<&'a crate::session_db::SessionDb>,
            ) -> FactoryFuture<'a>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            fallback,
            factory: Box::new(factory),
            clients: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl AgentClient for ConversationAgent {
    fn supports_structured_content(&self) -> bool {
        self.fallback.supports_structured_content()
    }
    fn manages_history(&self) -> bool {
        self.fallback.manages_history()
    }

    async fn run_turn(
        &self,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
        events: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.fallback.run_turn(msg, history, events).await
    }

    async fn run_turn_with_context(
        &self,
        context: crate::agent::TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
        events: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let Some(home) = context.home else {
            return self.run_turn(msg, history, events).await;
        };
        let key = (home.to_owned(), crate::session_db::message_session_id(msg));
        let cell = self
            .clients
            .lock()
            .await
            .entry(key)
            .or_insert_with(|| Arc::new(ClientCell::new()))
            .clone();
        // The cell provides a defensive single flight even when a caller has
        // bypassed the normal conversation lease. Unrelated keys initialize in
        // parallel, and a failed attempt leaves this cell available for retry.
        let client = cell
            .get_or_try_init(|| async {
                (self.factory)(home, msg, history, context.database)
                    .await
                    .map_err(|error| {
                        hermes_core::Error::Other(format!(
                            "conversation agent initialization failed: {error}"
                        ))
                    })
            })
            .await?
            .clone();
        // Provider I/O begins only after the map lock is released. Failed
        // builds remain uncached and never reuse another profile's client.
        client.run_turn(msg, history, events).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordedAgent {
        label: String,
        calls: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl AgentClient for RecordedAgent {
        async fn run_turn(
            &self,
            _msg: &Message,
            _history: &[crate::session_db::HistoryMessage],
            _events: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(self.label.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn routed_clients_preserve_profile_and_conversation_identity() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let constructed = builds.clone();
        let fallback = Arc::new(RecordedAgent {
            label: "fallback".into(),
            calls: calls.clone(),
        });
        let agent = ConversationAgent::new(fallback, move |home, message, history, database| {
            let home = home.to_owned();
            let message = message.clone();
            let history = history.to_vec();
            let recorded = recorded.clone();
            let constructed = constructed.clone();
            Box::pin(async move {
                assert!(message.resolved_session_id.is_some());
                assert!(database.is_none());
                assert!(history.is_empty());
                tokio::task::yield_now().await;
                if home == Path::new("broken") {
                    anyhow::bail!("missing profile credentials");
                }
                let label = format!("{}:{}", home.display(), constructed.lock().unwrap().len());
                constructed.lock().unwrap().push(label.clone());
                Ok(Arc::new(RecordedAgent {
                    label,
                    calls: recorded.clone(),
                }) as Arc<dyn AgentClient>)
            })
        });
        let mut msg: Message = serde_json::from_value(serde_json::json!({"platform":"cli", "channel_id":"same", "sender_id":"user", "text":"hello"})).unwrap();
        msg.resolved_session_id = Some("one".into());
        let (tx, _rx) = mpsc::channel(8);
        for home in ["red", "red", "blue"] {
            agent
                .run_turn_with_context(
                    crate::agent::TurnContext {
                        home: Some(Path::new(home)),
                        database: None,
                    },
                    &msg,
                    &[],
                    tx.clone(),
                )
                .await
                .unwrap();
        }
        msg.resolved_session_id = Some("two".into());
        agent
            .run_turn_with_context(
                crate::agent::TurnContext {
                    home: Some(Path::new("red")),
                    database: None,
                },
                &msg,
                &[],
                tx.clone(),
            )
            .await
            .unwrap();
        msg.resolved_session_id = Some("one".into());
        agent
            .run_turn_with_context(
                crate::agent::TurnContext {
                    home: Some(Path::new("red")),
                    database: None,
                },
                &msg,
                &[],
                tx.clone(),
            )
            .await
            .unwrap();
        assert!(agent
            .run_turn_with_context(
                crate::agent::TurnContext {
                    home: Some(Path::new("broken")),
                    database: None
                },
                &msg,
                &[],
                tx.clone()
            )
            .await
            .is_err());
        agent
            .run_turn_with_context(crate::agent::TurnContext::default(), &msg, &[], tx)
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["red:0", "red:0", "blue:1", "red:2", "red:0", "fallback"]
        );
        assert_eq!(builds.lock().unwrap().len(), 3);
    }
}
