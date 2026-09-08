//! Stateful native session commands.

use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct ResetResult {
    pub reply: String,
}

/// Rotate one stable route while holding the predecessor turn lease. The
/// compare-and-swap store write closes the no-route race without creating an
/// empty predecessor when `/new` is the conversation's first message.
pub async fn reset_session(
    deps: crate::session_admission::AdmissionDeps,
    agent: Arc<dyn crate::agent::AgentClient>,
    source: crate::session::SessionSource,
    owner_key: &str,
    title: Option<String>,
) -> anyhow::Result<ResetResult> {
    for _ in 0..32 {
        let route_key = deps.store.session_key_for_source(&source);
        let route_lease = deps
            .route_leases
            .acquire(
                &route_key,
                owner_key,
                deps.generation.fetch_add(1, Ordering::Relaxed),
                None,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let observe_store = deps.store.clone();
        let observe_source = source.clone();
        let expected = tokio::task::spawn_blocking(move || {
            observe_store.current_entry_for_source(&observe_source)
        })
        .await
        .map_err(|error| anyhow::anyhow!("reset route observer failed: {error}"))?;

        let lease = if let Some(entry) = expected.as_ref() {
            deps.transcript_leases
                .acquire(
                    &entry.session_id,
                    owner_key,
                    deps.generation.fetch_add(1, Ordering::Relaxed),
                    None,
                )
                .await
                .map_err(|error| anyhow::anyhow!(error))?
        } else {
            None
        };

        let reset_store = deps.store.clone();
        let reset_source = source.clone();
        let reset_expected = expected.clone();
        let reset = tokio::task::spawn_blocking(move || {
            reset_store.reset_session(&reset_source, reset_expected.as_ref())
        })
        .await
        .map_err(|error| anyhow::anyhow!("reset worker failed: {error}"))??;
        let Some(reset) = reset else {
            drop(lease);
            drop(route_lease);
            continue;
        };

        if let Some(predecessor_id) = reset.predecessor_id.as_deref() {
            let database = deps.store.database_for_key(&reset.entry.session_key);
            agent.retire_conversation(
                crate::agent::TurnContext::from_database(database.as_deref()),
                predecessor_id,
            );
        }
        drop(lease);
        drop(route_lease);

        let mut reply = if reset.predecessor_id.is_some() {
            "✨ Session reset! Starting fresh.".to_owned()
        } else {
            "✨ New session started!".to_owned()
        };
        if title.is_some() {
            reply.push_str("\n\nSession titles are not available in the native gateway yet.");
        }
        return Ok(ResetResult { reply });
    }
    anyhow::bail!("session route kept changing during reset")
}
