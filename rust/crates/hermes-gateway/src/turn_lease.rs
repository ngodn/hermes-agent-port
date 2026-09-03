//! Per-session turn lease — serializes the [load history -> run -> flush] region.
//!
// Full API is intentionally ahead of its callers: `acquire` is wired into the
// Dispatcher now; `rebind` lands with compression session rotation, `release`
// (RAII parity) and `len` are used by tests. Allow until fully wired.
#![allow(dead_code)]
//!
//! Port of `gateway/turn_lease.py` (#64934). The gateway's busy guards are
//! keyed by routing key, but the durable transcript is owned by session_id, and
//! session resolution is many-to-one (`/resume` from a second chat, CLI
//! continuity, topic tip-walks). Two routing keys mapped to one session_id can
//! run concurrent turns and interleave their flushes on one transcript. This
//! lease closes that route by serializing per resolved session_id.
//!
//! Rust note: holding the [`TurnLeaseToken`] *is* holding the lease — the token
//! owns the mutex guard, so releasing is just dropping it, and the Python
//! "a stale unwind can never release a newer turn's lease" property is
//! structural here rather than enforced by an ownership check.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tracing::warn;

/// Upper bound on tracked per-session leases. Idle entries (no holder, no
/// pending acquire) are evicted oldest-first once the cap is reached; live
/// leases are never evicted, so a burst of distinct sessions can transiently
/// exceed the cap rather than break serialization.
pub const DEFAULT_MAX_LEASES: usize = 512;

/// Fallback wait when the caller passes no positive timeout. A waiter that
/// cannot acquire within the budget is rejected (fail-closed), never run
/// unserialized against the holder.
pub const DEFAULT_LEASE_WAIT: Duration = Duration::from_secs(5);

/// The session lease stayed held for the caller's full wait budget. Fail-closed:
/// the caller did not acquire and must not enter the load/run/flush region.
#[derive(Debug, Clone)]
pub struct TurnLeaseTimeout {
    pub session_id: String,
    pub owner_key: String,
    pub generation: u64,
    pub wait: Duration,
}

impl std::fmt::Display for TurnLeaseTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "turn lease wait timed out after {}s on session {} for routing key {} (gen {})",
            self.wait.as_secs(),
            self.session_id,
            self.owner_key,
            self.generation
        )
    }
}

impl std::error::Error for TurnLeaseTimeout {}

/// A held lease. Dropping it releases the lease (idempotent by construction:
/// a moved/dropped token cannot be released again).
pub struct TurnLeaseToken {
    pub session_id: String,
    pub owner_key: String,
    pub generation: u64,
    // Field order matters: the guard is dropped before `_lock`, freeing the
    // mutex while the Arc that owns it is still alive.
    _guard: OwnedMutexGuard<()>,
    _lock: Arc<AsyncMutex<()>>,
}

impl std::fmt::Debug for TurnLeaseToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnLeaseToken")
            .field("session_id", &self.session_id)
            .field("owner_key", &self.owner_key)
            .field("generation", &self.generation)
            .finish()
    }
}

struct LeaseEntry {
    lock: Arc<AsyncMutex<()>>,
    last_used: Instant,
}

/// Asyncio-lease-equivalent per resolved session_id, serializing transcript
/// turns. Process-local, same visibility scope as the routing-key guards it
/// extends.
pub struct SessionTurnLeaseRegistry {
    leases: StdMutex<HashMap<String, LeaseEntry>>,
    max_entries: usize,
}

impl SessionTurnLeaseRegistry {
    pub fn new(max_entries: usize) -> Self {
        Self {
            leases: StdMutex::new(HashMap::new()),
            max_entries: max_entries.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.leases.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Acquire the turn lease for `session_id`, waiting if held.
    ///
    /// Returns `Ok(None)` for an empty session id, `Ok(Some(token))` on
    /// success, and `Err(TurnLeaseTimeout)` when the wait budget expires — in
    /// which case the caller must reject the turn rather than run it.
    pub async fn acquire(
        &self,
        session_id: &str,
        owner_key: &str,
        generation: u64,
        timeout: Option<Duration>,
    ) -> Result<Option<TurnLeaseToken>, TurnLeaseTimeout> {
        if session_id.is_empty() {
            return Ok(None);
        }
        let wait = match timeout {
            Some(t) if t > Duration::ZERO => t,
            _ => DEFAULT_LEASE_WAIT,
        };

        // Clone the Arc under the sync lock, then release it before awaiting.
        // Holding the Arc across the await keeps the entry non-idle (its
        // strong_count > 1), so eviction cannot orphan it mid-acquire.
        let lock = self.get_or_create(session_id);

        if lock.try_lock().is_err() {
            warn!(
                session = session_id,
                owner_key,
                generation,
                "turn lease contention: two routing keys mapped to one session_id \
                 (#64934); serializing this turn behind the in-flight turn's flush"
            );
        }

        let guard = match tokio::time::timeout(wait, Arc::clone(&lock).lock_owned()).await {
            Ok(g) => g,
            Err(_) => {
                let err = TurnLeaseTimeout {
                    session_id: session_id.to_string(),
                    owner_key: owner_key.to_string(),
                    generation,
                    wait,
                };
                warn!(%err, "failing closed: refusing to run this turn unserialized");
                return Err(err);
            }
        };

        Ok(Some(TurnLeaseToken {
            session_id: session_id.to_string(),
            owner_key: owner_key.to_string(),
            generation,
            _guard: guard,
            _lock: lock,
        }))
    }

    /// Release a token's lease. With RAII this is just dropping the token;
    /// provided for call-site parity with the Python API. Always returns true.
    pub fn release(&self, token: TurnLeaseToken) -> bool {
        drop(token);
        true
    }

    /// Alias a held lease onto `new_session_id` after a mid-turn session
    /// rotation (compression). The SAME mutex is registered under the new id so
    /// acquirers on either id serialize against one lock. Blocked (returns
    /// false) if the target id already has a live lease of its own.
    pub fn rebind(&self, token: &mut TurnLeaseToken, new_session_id: &str) -> bool {
        if new_session_id.is_empty() || new_session_id == token.session_id {
            return false;
        }
        let mut leases = self.leases.lock().unwrap();

        // The target must not already be a distinct live lease.
        if let Some(existing) = leases.get(new_session_id) {
            let same = Arc::ptr_eq(&existing.lock, &token._lock);
            let live = Arc::strong_count(&existing.lock) > 1 || existing.lock.try_lock().is_err();
            if !same && live {
                warn!(
                    from = token.session_id,
                    to = new_session_id,
                    "turn lease rebind blocked: target session's lease is already live; \
                     keeping the lease on the old id (#64934 rotation-alias edge)"
                );
                return false;
            }
        }

        leases.insert(
            new_session_id.to_string(),
            LeaseEntry {
                lock: Arc::clone(&token._lock),
                last_used: Instant::now(),
            },
        );
        token.session_id = new_session_id.to_string();
        true
    }

    fn get_or_create(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut leases = self.leases.lock().unwrap();
        if let Some(entry) = leases.get_mut(session_id) {
            entry.last_used = Instant::now();
            return Arc::clone(&entry.lock);
        }
        self.evict_idle(&mut leases);
        let lock = Arc::new(AsyncMutex::new(()));
        leases.insert(
            session_id.to_string(),
            LeaseEntry {
                lock: Arc::clone(&lock),
                last_used: Instant::now(),
            },
        );
        lock
    }

    /// Drop oldest idle entries so a new lease fits under the cap. An entry is
    /// idle when nobody holds or awaits it: the registry map is the sole owner
    /// of its Arc (`strong_count == 1`) and the mutex is free. Never evicts a
    /// held or contended lease — correctness beats the cap.
    fn evict_idle(&self, leases: &mut HashMap<String, LeaseEntry>) {
        let overflow = (leases.len() + 1).saturating_sub(self.max_entries);
        if overflow == 0 {
            return;
        }
        let mut idle: Vec<(String, Instant)> = leases
            .iter()
            .filter(|(_, e)| Arc::strong_count(&e.lock) == 1 && e.lock.try_lock().is_ok())
            .map(|(sid, e)| (sid.clone(), e.last_used))
            .collect();
        idle.sort_by_key(|(_, t)| *t);
        for (sid, _) in idle.into_iter().take(overflow) {
            leases.remove(&sid);
        }
    }
}

impl Default for SessionTurnLeaseRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_LEASES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn empty_session_returns_none() {
        let reg = SessionTurnLeaseRegistry::default();
        let tok = reg.acquire("", "k", 1, None).await.unwrap();
        assert!(tok.is_none());
    }

    #[tokio::test]
    async fn serializes_same_session() {
        let reg = Arc::new(SessionTurnLeaseRegistry::default());
        let counter = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for i in 0..8 {
            let reg = Arc::clone(&reg);
            let counter = Arc::clone(&counter);
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                let tok = reg
                    .acquire("s1", &format!("key{i}"), i, Some(Duration::from_secs(5)))
                    .await
                    .unwrap()
                    .unwrap();
                let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                counter.fetch_sub(1, Ordering::SeqCst);
                reg.release(tok);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // If the lease serializes, no two turns are ever inside at once.
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn times_out_when_held() {
        let reg = SessionTurnLeaseRegistry::default();
        let _held = reg.acquire("s", "a", 1, None).await.unwrap().unwrap();
        let err = reg
            .acquire("s", "b", 2, Some(Duration::from_millis(50)))
            .await
            .unwrap_err();
        assert_eq!(err.session_id, "s");
        assert_eq!(err.owner_key, "b");
    }

    #[tokio::test]
    async fn release_frees_for_next_waiter() {
        let reg = SessionTurnLeaseRegistry::default();
        let first = reg.acquire("s", "a", 1, None).await.unwrap().unwrap();
        reg.release(first);
        let second = reg
            .acquire("s", "b", 2, Some(Duration::from_millis(50)))
            .await
            .unwrap();
        assert!(second.is_some());
    }

    #[tokio::test]
    async fn rebind_aliases_the_same_lock() {
        let reg = SessionTurnLeaseRegistry::default();
        let mut tok = reg.acquire("old", "a", 1, None).await.unwrap().unwrap();
        assert!(reg.rebind(&mut tok, "new"));
        assert_eq!(tok.session_id, "new");
        // A waiter on the new id must block behind the still-held lease.
        let err = reg
            .acquire("new", "b", 2, Some(Duration::from_millis(50)))
            .await
            .unwrap_err();
        assert_eq!(err.session_id, "new");
    }
}
