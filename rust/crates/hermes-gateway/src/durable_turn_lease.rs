//! Cross-process serialization for the load, model, and flush turn region.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const LEASE_TTL: Duration = Duration::from_secs(300);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
pub const TURN_WAIT: Duration = Duration::from_secs(1800);
pub const MUTATION_GRACE: Duration = Duration::from_millis(250);

fn active_holders() -> &'static Mutex<HashSet<String>> {
    static HOLDERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    HOLDERS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub fn holder_is_active(holder: &str) -> bool {
    active_holders().lock().unwrap().contains(holder)
}

struct ActiveHolderRegistration(String);

impl ActiveHolderRegistration {
    fn new(holder: String) -> Self {
        active_holders().lock().unwrap().insert(holder.clone());
        Self(holder)
    }
}

impl Drop for ActiveHolderRegistration {
    fn drop(&mut self) {
        active_holders().lock().unwrap().remove(&self.0);
    }
}

pub struct DurableTurnLease {
    database: Arc<crate::session_db::SessionDb>,
    session_id: String,
    holder: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    _registration: ActiveHolderRegistration,
}

impl DurableTurnLease {
    pub fn holder(&self) -> &str {
        &self.holder
    }
}

impl Drop for DurableTurnLease {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let database = self.database.clone();
        let session_id = self.session_id.clone();
        let holder = self.holder.clone();
        let release = move || {
            if let Err(error) = database.release_session_turn_lease(&session_id, &holder) {
                tracing::warn!(%error, session = %session_id, "durable turn lease release failed");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(release);
            }
            Err(_) => {
                if let Err(error) = std::thread::Builder::new()
                    .name("hermes-lease-release".into())
                    .spawn(release)
                {
                    tracing::warn!(%error, session = %self.session_id, "durable turn lease release worker failed to start");
                }
            }
        }
    }
}

pub async fn acquire(
    database: Arc<crate::session_db::SessionDb>,
    session_id: &str,
    owner: &str,
    wait: Duration,
) -> anyhow::Result<DurableTurnLease> {
    acquire_with_timing(
        database,
        session_id,
        owner,
        wait,
        LEASE_TTL,
        REFRESH_INTERVAL,
    )
    .await
}

async fn acquire_with_timing(
    database: Arc<crate::session_db::SessionDb>,
    session_id: &str,
    owner: &str,
    wait: Duration,
    ttl: Duration,
    refresh_interval: Duration,
) -> anyhow::Result<DurableTurnLease> {
    anyhow::ensure!(
        !session_id.is_empty(),
        "durable turn lease needs a session id"
    );
    let nonce = crate::install_identity::mint_id()
        .ok_or_else(|| anyhow::anyhow!("could not generate durable turn lease identity"))?;
    let holder = format!("pid={}:rust={}", std::process::id(), &nonce[..8]);
    let registration = ActiveHolderRegistration::new(holder.clone());
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let attempt_db = database.clone();
        let attempt_session = session_id.to_owned();
        let attempt_holder = holder.clone();
        let acquired = tokio::task::spawn_blocking(move || {
            attempt_db.try_acquire_session_turn_lease(
                &attempt_session,
                &attempt_holder,
                ttl.as_secs_f64(),
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("durable turn lease worker failed: {error}"))??;
        if acquired {
            break;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!("durable turn lease wait timed out on session {session_id} for {owner}");
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(50))).await;
    }

    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let refresh_db = database.clone();
    let refresh_session = session_id.to_owned();
    let refresh_holder = holder.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                _ = tokio::time::sleep(refresh_interval) => {
                    let database = refresh_db.clone();
                    let session = refresh_session.clone();
                    let holder = refresh_holder.clone();
                    let refreshed = tokio::task::spawn_blocking(move || {
                        database.refresh_session_turn_lease(&session, &holder, ttl.as_secs_f64())
                    }).await;
                    match refreshed {
                        Ok(Ok(true)) => {}
                        Ok(Ok(false)) => {
                            if stopped.try_recv().is_err() {
                                tracing::error!(session = %refresh_session, "durable turn lease ownership was lost");
                            }
                            break;
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, session = %refresh_session, "durable turn lease refresh failed");
                        }
                        Err(error) => {
                            tracing::warn!(%error, session = %refresh_session, "durable turn lease refresh worker failed");
                        }
                    }
                }
            }
        }
    });

    Ok(DurableTurnLease {
        database,
        session_id: session_id.into(),
        holder,
        stop: Some(stop),
        _registration: registration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (std::path::PathBuf, Arc<crate::session_db::SessionDb>) {
        let path = std::env::temp_dir().join(format!(
            "hermes-durable-lease-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let database = Arc::new(crate::session_db::SessionDb::open(path.clone()).unwrap());
        database
            .ensure_session("session", "local", None, None, None)
            .unwrap();
        (path, database)
    }

    #[tokio::test]
    async fn lease_is_exclusive_refreshes_and_releases() {
        let (path, database) = temp_db();
        let first = acquire_with_timing(
            database.clone(),
            "session",
            "first",
            Duration::from_millis(20),
            Duration::from_millis(120),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(180)).await;
        assert_eq!(
            database
                .session_turn_lease_holder("session")
                .unwrap()
                .as_deref(),
            Some(first.holder())
        );
        assert!(acquire_with_timing(
            database.clone(),
            "session",
            "second",
            Duration::from_millis(20),
            Duration::from_millis(120),
            Duration::from_millis(20),
        )
        .await
        .is_err());
        drop(first);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while database
            .session_turn_lease_holder("session")
            .unwrap()
            .is_some()
        {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(database);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn inactive_holder_from_this_process_is_reclaimed() {
        let (path, database) = temp_db();
        let orphan = format!("pid={}:rust=orphaned", std::process::id());
        assert!(database
            .try_acquire_session_turn_lease("session", &orphan, 300.0)
            .unwrap());

        let replacement = acquire_with_timing(
            database.clone(),
            "session",
            "replacement",
            Duration::from_millis(100),
            Duration::from_millis(120),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert_ne!(replacement.holder(), orphan);
        drop(replacement);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while database
            .session_turn_lease_holder("session")
            .unwrap()
            .is_some()
        {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(database);
        std::fs::remove_file(path).unwrap();
    }
}
