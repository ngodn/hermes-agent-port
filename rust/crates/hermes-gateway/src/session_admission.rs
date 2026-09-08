//! Route-stable turn admission shared by HTTP and push ingress.
//!
//! Session resolution performs blocking recovery and may rotate an automatic
//! reset. The async lease wait that follows creates a race window, so admission
//! rechecks the route under the acquired predecessor lease and retries when it
//! changed. No history read or provider checkout can then target a superseded
//! session.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct AdmittedSession {
    pub entry: crate::session_entry::SessionEntry,
    pub database: Option<Arc<crate::session_db::SessionDb>>,
    pub finalizable: bool,
    pub predecessor_id: Option<String>,
    pub lease: Option<crate::turn_lease::TurnLeaseToken>,
}

#[derive(Clone)]
pub struct AdmissionDeps {
    pub store: Arc<crate::session_store::SessionStore>,
    pub transcript_leases: Arc<crate::turn_lease::SessionTurnLeaseRegistry>,
    pub route_leases: Arc<crate::turn_lease::SessionTurnLeaseRegistry>,
    pub generation: Arc<AtomicU64>,
}

pub async fn admit_turn(
    deps: AdmissionDeps,
    source: crate::session::SessionSource,
    legacy: Option<(String, String)>,
    freshness_seconds: f64,
    owner_key: &str,
) -> anyhow::Result<AdmittedSession> {
    for _ in 0..32 {
        let route_key = deps.store.session_key_for_source(&source);
        let route_token = deps
            .route_leases
            .acquire(
                &route_key,
                owner_key,
                deps.generation.fetch_add(1, Ordering::Relaxed),
                None,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let resolve_store = deps.store.clone();
        let resolve_source = source.clone();
        let resolve_legacy = legacy.clone();
        let entry = tokio::task::spawn_blocking(move || {
            resolve_store.get_or_create_with_legacy(
                &resolve_source,
                resolve_legacy
                    .as_ref()
                    .map(|(id, origin)| (id.as_str(), origin.as_str())),
                false,
                false,
                freshness_seconds,
                |_| Ok(false),
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("session resolver worker failed: {error}"))?
        .map_err(|error| anyhow::anyhow!("could not resolve session: {error:#}"))?;

        let token = deps
            .transcript_leases
            .acquire(
                &entry.session_id,
                owner_key,
                deps.generation.fetch_add(1, Ordering::Relaxed),
                None,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error))?;

        let verify_store = deps.store.clone();
        let observed = entry.clone();
        let verified = tokio::task::spawn_blocking(move || {
            if !verify_store.route_matches(&observed) {
                return Ok(None);
            }
            verify_store.update_session(&observed.session_key, None, true)?;
            let finalizable = verify_store.is_session_finalizable(&observed);
            let predecessor_id = verify_store.take_auto_reset_predecessor(&observed)?;
            let database = verify_store.database_for_key(&observed.session_key);
            Ok::<_, anyhow::Error>(Some((database, finalizable, predecessor_id)))
        })
        .await
        .map_err(|error| anyhow::anyhow!("session verifier worker failed: {error}"))??;

        if let Some((database, finalizable, predecessor_id)) = verified {
            drop(route_token);
            return Ok(AdmittedSession {
                entry,
                database,
                finalizable,
                predecessor_id,
                lease: token,
            });
        }
        drop(token);
        drop(route_token);
    }
    anyhow::bail!("session route kept changing during turn admission")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hermes-admission-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn stale_waiter_re_resolves_after_route_rotation() {
        let home = home();
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
        let source = crate::session::SessionSource::new("local", "race");
        let first = store
            .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
            .unwrap();
        let leases = Arc::new(crate::turn_lease::SessionTurnLeaseRegistry::default());
        let held = leases
            .acquire(&first.session_id, "reset", 1, None)
            .await
            .unwrap();

        let waiter = tokio::spawn(admit_turn(
            AdmissionDeps {
                store: store.clone(),
                transcript_leases: leases.clone(),
                route_leases: Arc::new(crate::turn_lease::SessionTurnLeaseRegistry::default()),
                generation: Arc::new(AtomicU64::new(2)),
            },
            source.clone(),
            None,
            3600.0,
            "turn",
        ));
        tokio::task::yield_now().await;
        let reset = store.reset_session(&source, Some(&first)).unwrap().unwrap();
        drop(held);

        let admitted = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(admitted.entry.session_id, reset.entry.session_id);
        assert_ne!(admitted.entry.session_id, first.session_id);
        drop(admitted.lease);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }
}
