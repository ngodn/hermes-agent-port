//! Stateful native session commands.

use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct ResetResult {
    pub reply: String,
}

/// Resolve a pending text confirmation before ordinary slash dispatch. `None`
/// means the message was unrelated and must continue through the normal path.
pub async fn resolve_reset_confirmation(
    confirmations: &crate::slash_confirm::SlashConfirmations,
    deps: crate::session_admission::AdmissionDeps,
    agent: Arc<dyn crate::agent::AgentClient>,
    source: crate::session::SessionSource,
    message: &hermes_core::Message,
    user_config: &serde_json::Value,
    tool_approval_live: bool,
) -> Option<ResetResult> {
    let route_key = deps.store.session_key_for_source(&source);
    let resolution =
        confirmations.resolve_text(&route_key, &message.text, tool_approval_live, |command| {
            crate::slash::can_run_command(user_config, message, command)
        });
    match resolution {
        crate::slash_confirm::Resolution::NotHandled => None,
        crate::slash_confirm::Resolution::Expired => Some(ResetResult {
            reply: String::new(),
        }),
        crate::slash_confirm::Resolution::Cancelled { command } => Some(ResetResult {
            reply: format!("🟡 /{command} cancelled. Conversation unchanged."),
        }),
        crate::slash_confirm::Resolution::Approved(approved) => {
            let persisted = if approved.always {
                match confirmations.persist_opt_out().await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(%error, session = %route_key, "could not persist destructive slash confirmation opt-out");
                        false
                    }
                }
            } else {
                false
            };
            Some(
                match reset_session(deps, agent, source, &message.sender_id, approved.title).await {
                    Ok(mut result) => {
                        if approved.always {
                            if persisted {
                                result.reply.push_str(
                                "\n\nℹ️ Future /clear, /new, /reset, and /undo will run without confirmation. Re-enable via `approvals.destructive_slash_confirm: true` in config.yaml.",
                            );
                            } else {
                                result.reply.push_str(
                                "\n\n⚠️ Could not save that preference (config.yaml is not writable), so /clear, /new, /reset, and /undo will ask again next time. To silence it permanently, set `approvals.destructive_slash_confirm: false` in config.yaml.",
                            );
                            }
                        }
                        result
                    }
                    Err(error) => {
                        tracing::error!(%error, session = %route_key, "destructive slash confirmation handler failed");
                        ResetResult {
                            reply: format!("❌ Error handling confirmation: {error}"),
                        }
                    }
                },
            )
        }
    }
}

/// Gate a newly authorized reset. The prompt is registered before it is
/// returned; when the live config disables confirmation, reset immediately.
pub async fn reset_or_confirm(
    confirmations: &crate::slash_confirm::SlashConfirmations,
    deps: crate::session_admission::AdmissionDeps,
    agent: Arc<dyn crate::agent::AgentClient>,
    source: crate::session::SessionSource,
    owner_key: &str,
    title: Option<String>,
    typed_prefix: &str,
) -> anyhow::Result<ResetResult> {
    if confirmations.required().await {
        let route_key = deps.store.session_key_for_source(&source);
        return Ok(ResetResult {
            reply: confirmations.register_reset(&route_key, title, typed_prefix),
        });
    }
    reset_session(deps, agent, source, owner_key, title).await
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
