//! Mutable physical-session identity for one admitted logical turn.
//!
//! A rotation-mode compression can replace the SQLite session and routing
//! entry while the provider loop is still running. Every later write in that
//! turn must then target the child, while the conversation-level lease remains
//! rooted at the same lineage. This module keeps that transition behind one
//! small interface and leaves [`crate::session_store::SessionStore`] as the
//! sole publisher of the database plus in-memory routing commit.

use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct TurnSession {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<crate::session_store::SessionStore>,
    source: crate::session::SessionSource,
    state: Mutex<State>,
}

struct State {
    session_lineage: Vec<String>,
    entry: crate::session_entry::SessionEntry,
    transcript_lease: Option<TranscriptLease>,
}

struct TranscriptLease {
    registry: Arc<crate::turn_lease::SessionTurnLeaseRegistry>,
    token: crate::turn_lease::TurnLeaseToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressionRotation {
    pub parent_session_id: String,
    pub child_session_id: String,
}

impl TurnSession {
    pub fn new(
        store: Arc<crate::session_store::SessionStore>,
        source: crate::session::SessionSource,
        entry: crate::session_entry::SessionEntry,
    ) -> Self {
        let initial_session_id = entry.session_id.clone();
        Self {
            inner: Arc::new(Inner {
                store,
                source,
                state: Mutex::new(State {
                    session_lineage: vec![initial_session_id],
                    entry,
                    transcript_lease: None,
                }),
            }),
        }
    }

    /// Transfer ownership of the process-local transcript lease into the
    /// session identity. Rotation can then alias the same held mutex to the
    /// child before another turn resolves that child route.
    pub fn bind_transcript_lease(
        &self,
        registry: Arc<crate::turn_lease::SessionTurnLeaseRegistry>,
        token: crate::turn_lease::TurnLeaseToken,
    ) {
        let mut state = self.inner.state.lock().unwrap();
        debug_assert!(state.transcript_lease.is_none());
        state.transcript_lease = Some(TranscriptLease { registry, token });
    }

    pub fn session_id(&self) -> String {
        self.inner.state.lock().unwrap().entry.session_id.clone()
    }

    /// Candidate frozen-client keys from the newest committed physical session
    /// back to the admission parent. Post-commit observer failure can leave the
    /// client parked at any predecessor after multiple rotations in one turn.
    pub fn cache_session_candidates(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .unwrap()
            .session_lineage
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// Atomically publish a compression child and advance this turn's physical
    /// identity only after the store confirms the SQLite and routing commit.
    pub fn publish_compression(
        &self,
        original_messages: &[crate::session_db::CompressionHistoryMessage],
        rows: &[crate::session_db::CompressionReplacementRow],
        turn_lease_holder: Option<&str>,
    ) -> anyhow::Result<Option<CompressionRotation>> {
        let mut state = self.inner.state.lock().unwrap();
        let parent_session_id = state.entry.session_id.clone();
        let published = self.inner.store.publish_compression(
            &self.inner.source,
            &state.entry,
            original_messages,
            rows,
            turn_lease_holder,
        )?;
        let Some(published) = published else {
            return Ok(None);
        };
        anyhow::ensure!(
            published.predecessor_id == parent_session_id,
            "compression publisher returned a mismatched predecessor"
        );
        let child_session_id = published.entry.session_id.clone();
        state.entry = published.entry;
        state.session_lineage.push(child_session_id.clone());
        if let Some(lease) = state.transcript_lease.as_mut() {
            if !lease.registry.rebind(&mut lease.token, &child_session_id) {
                tracing::error!(
                    old_session = %parent_session_id,
                    new_session = %child_session_id,
                    "same-turn compression could not rebind transcript lease"
                );
            }
        }
        Ok(Some(CompressionRotation {
            parent_session_id,
            child_session_id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hermes-turn-session-{label}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(
        label: &str,
    ) -> (
        TempRoot,
        Arc<crate::session_store::SessionStore>,
        crate::session::SessionSource,
        crate::session_entry::SessionEntry,
        Arc<crate::session_db::SessionDb>,
    ) {
        let root = TempRoot::new(label);
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                crate::config_gateway::GatewayConfig {
                    sessions_dir: root.0.join("sessions"),
                    ..Default::default()
                },
                root.0.clone(),
                root.0.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let source = crate::session::SessionSource::new("local", label);
        let entry = store
            .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
            .unwrap();
        let database = store.database_for_key(&entry.session_key).unwrap();
        database
            .append_message(&entry.session_id, "user", "original question")
            .unwrap();
        (root, store, source, entry, database)
    }

    fn summary_row() -> crate::session_db::CompressionReplacementRow {
        crate::session_db::CompressionReplacementRow {
            source_id: None,
            role: "user".into(),
            content: "compressed handoff".into(),
            api_content: None,
            compressed_summary: true,
        }
    }

    #[tokio::test]
    async fn committed_rotation_advances_identity_and_aliases_the_held_lease() {
        let (_root, store, source, entry, database) = fixture("commit");
        let parent_id = entry.session_id.clone();
        let snapshot = database.load_compression_snapshot(&parent_id).unwrap();
        let leases = Arc::new(crate::turn_lease::SessionTurnLeaseRegistry::default());
        let token = leases
            .acquire(&parent_id, "owner", 1, None)
            .await
            .unwrap()
            .unwrap();
        let session = TurnSession::new(store.clone(), source.clone(), entry);
        session.bind_transcript_lease(leases.clone(), token);

        let first_rotation = session
            .publish_compression(&snapshot.messages, &[summary_row()], None)
            .unwrap()
            .unwrap();
        let child_snapshot = database
            .load_compression_snapshot(&first_rotation.child_session_id)
            .unwrap();
        let second_rotation = session
            .publish_compression(&child_snapshot.messages, &[summary_row()], None)
            .unwrap()
            .unwrap();

        assert_eq!(first_rotation.parent_session_id, parent_id);
        assert_eq!(
            second_rotation.parent_session_id,
            first_rotation.child_session_id
        );
        assert_eq!(session.session_id(), second_rotation.child_session_id);
        assert_eq!(
            session.cache_session_candidates(),
            [
                second_rotation.child_session_id.clone(),
                first_rotation.child_session_id,
                parent_id,
            ]
        );
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            second_rotation.child_session_id
        );
        assert!(leases
            .acquire(
                &second_rotation.child_session_id,
                "contender",
                2,
                Some(std::time::Duration::from_millis(20)),
            )
            .await
            .is_err());
        drop(session);
        assert!(leases
            .acquire(
                &second_rotation.child_session_id,
                "next-turn",
                3,
                Some(std::time::Duration::from_secs(1)),
            )
            .await
            .unwrap()
            .is_some());
    }

    #[test]
    fn rejected_publication_leaves_the_turn_on_its_parent() {
        let (_root, store, source, entry, database) = fixture("reject");
        let parent_id = entry.session_id.clone();
        let stale = database.load_compression_snapshot(&parent_id).unwrap();
        database
            .append_message(&parent_id, "assistant", "concurrent append")
            .unwrap();
        let session = TurnSession::new(store.clone(), source.clone(), entry);

        assert!(session
            .publish_compression(&stale.messages, &[summary_row()], None)
            .unwrap()
            .is_none());
        assert_eq!(session.session_id(), parent_id);
        assert_eq!(
            session.cache_session_candidates().as_slice(),
            std::slice::from_ref(&parent_id)
        );
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            parent_id
        );
        assert!(database.get_session(&parent_id).unwrap().unwrap()["ended_at"].is_null());
    }
}
