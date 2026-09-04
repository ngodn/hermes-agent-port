//! Port of gateway/session_db_recovery.py.
//!
// Public API is ahead of its callers (the per-profile SessionDB path wires it).
#![allow(dead_code)]
//!
//! Recoverable per-path handle cache for the gateway. A SessionDB open can fail
//! transiently (disk full, a locked/again-readable file, a half-mounted volume);
//! this caches handles by path while letting a failed open heal in-process with
//! bounded, single-flight retries and exponential backoff, so one bad open does
//! not permanently wedge a profile's persistence.
//!
//! Faithful to the Python original except:
//!   * Rust's `Drop` replaces Python's explicit `close(handle)`. A cached handle
//!     (typically `Arc<SessionDb>`, whose `Drop` closes the SQLite connection)
//!     is closed by being dropped, so `close_all` just drains the maps and a
//!     handle opened after an invalidation is dropped instead of being handed to
//!     a `_close_rejected` callback.
//!   * The privacy-safe cross-cache health aggregate is computed and stored, but
//!     the write into the runtime status block is deferred until status.py is
//!     ported (`global_session_store_health()` exposes it in the meantime).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const INITIAL_RETRY_DELAY_SECONDS: f64 = 1.0;
const MAX_RETRY_DELAY_SECONDS: f64 = 60.0;

/// Per-path failure bookkeeping for a not-yet-open handle.
#[derive(Debug, Default, Clone)]
struct Unavailable {
    failures: u32,
    next_retry_at: f64,
    in_flight: bool,
}

struct Inner<H> {
    handles: HashMap<PathBuf, H>,
    unavailable: HashMap<PathBuf, Unavailable>,
    generation: u64,
}

/// A monotonic clock in seconds (like Python `time.monotonic`).
type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

/// One-shot callback fired when an open recovers after prior failures.
type OnRecovered = Box<dyn FnOnce()>;

/// Predicate classifying an open error as non-cacheable (do not retry; forget
/// the path and propagate the error).
type NonCacheable<E> = Box<dyn Fn(&E) -> bool>;

fn default_clock() -> Clock {
    let base = Instant::now();
    Box::new(move || base.elapsed().as_secs_f64())
}

static CACHE_IDS: AtomicU64 = AtomicU64::new(1);

/// Cache handles by path while allowing failed opens to heal in-process.
pub struct RecoverableHandleCache<H: Clone> {
    inner: Mutex<Inner<H>>,
    clock: Clock,
    initial_retry_delay: f64,
    max_retry_delay: f64,
    id: u64,
}

impl<H: Clone> RecoverableHandleCache<H> {
    pub fn new() -> Self {
        Self::with_config(
            default_clock(),
            INITIAL_RETRY_DELAY_SECONDS,
            MAX_RETRY_DELAY_SECONDS,
        )
    }

    pub fn with_config(clock: Clock, initial_retry_delay: f64, max_retry_delay: f64) -> Self {
        let initial = initial_retry_delay.max(0.0);
        Self {
            inner: Mutex::new(Inner {
                handles: HashMap::new(),
                unavailable: HashMap::new(),
                generation: 0,
            }),
            clock,
            initial_retry_delay: initial,
            max_retry_delay: max_retry_delay.max(initial),
            id: CACHE_IDS.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn backoff_delay(&self, failures: u32) -> f64 {
        let exp = (failures.saturating_sub(1)).min(30);
        (self.initial_retry_delay * 2f64.powi(exp as i32)).min(self.max_retry_delay)
    }

    /// Return a cached handle, or make one bounded, single-flight open attempt.
    ///
    /// `Ok(Some(h))` is a live handle; `Ok(None)` means "not available right now"
    /// (a retry is backing off, or another caller's open is in flight); `Err`
    /// is propagated when `raise_on_error` is set or when `non_cacheable`
    /// classifies the error as one that must not be cached/retried.
    ///
    /// `on_recovered` fires once when an open succeeds after prior failures.
    pub fn get<F, E>(
        &self,
        path: &Path,
        opener: F,
        raise_on_error: bool,
        on_recovered: Option<OnRecovered>,
        non_cacheable: Option<NonCacheable<E>>,
    ) -> Result<Option<H>, E>
    where
        F: FnOnce() -> Result<H, E>,
    {
        let path = path.to_path_buf();

        // Phase 1: decide whether to attempt an open, under the lock.
        let (was_unavailable, generation) = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(h) = inner.handles.get(&path) {
                return Ok(Some(h.clone()));
            }
            let now = (self.clock)();
            let entry = inner.unavailable.entry(path.clone()).or_default();
            if entry.in_flight || now < entry.next_retry_at {
                return Ok(None);
            }
            entry.in_flight = true;
            let was_unavailable = entry.failures > 0;
            let generation = inner.generation;
            (was_unavailable, generation)
        };

        if was_unavailable {
            self.publish_health(&path, "retrying");
        }

        // Phase 2: the actual open happens OUTSIDE the lock (it may block/IO).
        match opener() {
            Err(exc) => {
                if let Some(pred) = &non_cacheable {
                    if pred(&exc) {
                        // Non-cacheable: forget the path entirely and propagate.
                        let mut inner = self.inner.lock().unwrap();
                        if generation == inner.generation {
                            inner.unavailable.remove(&path);
                        }
                        return Err(exc);
                    }
                }
                let stale = {
                    let mut inner = self.inner.lock().unwrap();
                    let stale = generation != inner.generation;
                    if !stale {
                        if let Some(entry) = inner.unavailable.get_mut(&path) {
                            entry.failures += 1;
                            let delay = self.backoff_delay(entry.failures);
                            entry.next_retry_at = (self.clock)() + delay;
                            entry.in_flight = false;
                        }
                    }
                    stale
                };
                if stale {
                    return if raise_on_error { Err(exc) } else { Ok(None) };
                }
                self.publish_health(&path, "unavailable");
                if raise_on_error {
                    Err(exc)
                } else {
                    Ok(None)
                }
            }
            Ok(handle) => {
                let stale = {
                    let mut inner = self.inner.lock().unwrap();
                    let stale = generation != inner.generation;
                    if !stale {
                        inner.handles.insert(path.clone(), handle.clone());
                        inner.unavailable.remove(&path);
                    }
                    stale
                };
                if stale {
                    // A close_all ran while we were opening: drop the late handle
                    // (Rust's Drop closes it) and report unavailable.
                    drop(handle);
                    return Ok(None);
                }
                self.publish_health(&path, "ok");
                if was_unavailable {
                    if let Some(cb) = on_recovered {
                        cb();
                    }
                }
                Ok(Some(handle))
            }
        }
    }

    /// Drain cached handles (dropping them, which closes them) and invalidate any
    /// open currently in flight by bumping the generation.
    pub fn close_all(&self) {
        let (handles, paths) = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            let handles: Vec<H> = inner.handles.values().cloned().collect();
            let mut paths: Vec<PathBuf> = inner.handles.keys().cloned().collect();
            paths.extend(inner.unavailable.keys().cloned());
            inner.handles.clear();
            inner.unavailable.clear();
            (handles, paths)
        };
        // Drop outside the lock (closing may do IO).
        drop(handles);
        if let Some(global) = health_map().get() {
            let mut g = global.lock().unwrap();
            if let Some(states) = g.states.get_mut(&self.id) {
                for path in &paths {
                    states.remove(path);
                }
            }
            g.recompute();
        }
    }

    /// A sanitized state for tests and internal diagnostics.
    pub fn status_for(&self, path: &Path) -> &'static str {
        let inner = self.inner.lock().unwrap();
        if inner.handles.contains_key(path) {
            return "ok";
        }
        match inner.unavailable.get(path) {
            None => "unknown",
            Some(u) if u.in_flight => "retrying",
            Some(_) => "unavailable",
        }
    }

    fn publish_health(&self, path: &Path, state: &str) {
        let global = health_map().get_or_init(|| Mutex::new(GlobalHealth::default()));
        let mut g = global.lock().unwrap();
        g.states
            .entry(self.id)
            .or_default()
            .insert(path.to_path_buf(), state.to_string());
        g.recompute();
    }
}

impl<H: Clone> Default for RecoverableHandleCache<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: Clone> Drop for RecoverableHandleCache<H> {
    fn drop(&mut self) {
        // Best-effort: forget this cache's health entry (Python relies on a
        // weakref; we clean up explicitly so a dropped cache never lingers).
        if let Some(global) = health_map().get() {
            if let Ok(mut g) = global.lock() {
                g.states.remove(&self.id);
                g.recompute();
            }
        }
    }
}

// ── Cross-cache health aggregate ─────────────────────────────────────────────

#[derive(Default)]
struct GlobalHealth {
    states: HashMap<u64, HashMap<PathBuf, String>>,
    aggregate: String,
}

impl GlobalHealth {
    fn recompute(&mut self) {
        let mut has_retrying = false;
        let mut has_unavailable = false;
        for states in self.states.values() {
            for v in states.values() {
                match v.as_str() {
                    "retrying" => has_retrying = true,
                    "unavailable" => has_unavailable = true,
                    _ => {}
                }
            }
        }
        self.aggregate = if has_retrying {
            "retrying"
        } else if has_unavailable {
            "unavailable"
        } else {
            "ok"
        }
        .to_string();
    }
}

fn health_map() -> &'static OnceLock<Mutex<GlobalHealth>> {
    static MAP: OnceLock<Mutex<GlobalHealth>> = OnceLock::new();
    &MAP
}

/// The privacy-safe aggregate across every live gateway DB cache: `ok`,
/// `unavailable`, or `retrying`. Empty string when nothing has been published.
/// (status.py will surface this as `session_store.status` once ported.)
pub fn global_session_store_health() -> String {
    match health_map().get() {
        Some(global) => global.lock().unwrap().aggregate.clone(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A test clock whose value we can advance by hand.
    #[derive(Clone)]
    struct TestClock(Arc<Mutex<f64>>);
    impl TestClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(0.0)))
        }
        fn advance(&self, by: f64) {
            *self.0.lock().unwrap() += by;
        }
        fn as_clock(&self) -> Clock {
            let inner = self.0.clone();
            Box::new(move || *inner.lock().unwrap())
        }
    }

    fn cache_with(clock: &TestClock) -> RecoverableHandleCache<i32> {
        RecoverableHandleCache::with_config(clock.as_clock(), 1.0, 60.0)
    }

    #[test]
    fn caches_successful_open_once() {
        let clock = TestClock::new();
        let cache = cache_with(&clock);
        let path = PathBuf::from("/db/a");
        let calls = Arc::new(Mutex::new(0));

        let c2 = calls.clone();
        let h = cache
            .get::<_, ()>(
                &path,
                move || {
                    *c2.lock().unwrap() += 1;
                    Ok(7)
                },
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(h, Some(7));
        assert_eq!(cache.status_for(&path), "ok");

        // Second get returns the cached handle without calling the opener again.
        let h2 = cache
            .get::<_, ()>(&path, || panic!("opener should not run"), false, None, None)
            .unwrap();
        assert_eq!(h2, Some(7));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn failed_open_backs_off_then_recovers() {
        let clock = TestClock::new();
        let cache = cache_with(&clock);
        let path = PathBuf::from("/db/b");

        // First attempt fails -> unavailable, backoff 1s.
        let r = cache.get::<_, &str>(&path, || Err("boom"), false, None, None);
        assert_eq!(r, Ok(None));
        assert_eq!(cache.status_for(&path), "unavailable");

        // Immediately retrying is suppressed by backoff.
        let r2 = cache.get::<_, &str>(&path, || panic!("still backing off"), false, None, None);
        assert_eq!(r2, Ok(None));

        // After the delay elapses, a retry runs and recovers.
        clock.advance(1.5);
        let recovered = Arc::new(Mutex::new(false));
        let rc = recovered.clone();
        let r3 = cache.get::<_, &str>(
            &path,
            || Ok(42),
            false,
            Some(Box::new(move || *rc.lock().unwrap() = true)),
            None,
        );
        assert_eq!(r3, Ok(Some(42)));
        assert!(*recovered.lock().unwrap());
        assert_eq!(cache.status_for(&path), "ok");
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let clock = TestClock::new();
        let cache = RecoverableHandleCache::<i32>::with_config(clock.as_clock(), 1.0, 10.0);
        let path = PathBuf::from("/db/c");

        // failures=1 -> 1s, failures=2 -> 2s, failures=3 -> 4s, failures=4 -> 8s,
        // failures=5 -> capped at 10s.
        let expected = [1.0, 2.0, 4.0, 8.0, 10.0];
        for &delay in &expected {
            let r = cache.get::<_, ()>(&path, || Err(()), false, None, None);
            assert_eq!(r, Ok(None));
            // Not yet retryable just before the delay.
            clock.advance(delay - 0.01);
            let blocked = cache.get::<_, ()>(&path, || panic!("too early"), false, None, None);
            assert_eq!(blocked, Ok(None));
            clock.advance(0.02); // now past next_retry_at
        }
    }

    #[test]
    fn raise_on_error_propagates() {
        let clock = TestClock::new();
        let cache = cache_with(&clock);
        let path = PathBuf::from("/db/d");
        let r = cache.get::<_, &str>(&path, || Err("io"), true, None, None);
        assert_eq!(r, Err("io"));
    }

    #[test]
    fn non_cacheable_error_is_not_retried_and_propagates() {
        let clock = TestClock::new();
        let cache = cache_with(&clock);
        let path = PathBuf::from("/db/e");
        let r = cache.get::<_, &str>(
            &path,
            || Err("fatal"),
            false,
            None,
            Some(Box::new(|e: &&str| *e == "fatal")),
        );
        assert_eq!(r, Err("fatal"));
        // The path was forgotten, not left in a backoff state.
        assert_eq!(cache.status_for(&path), "unknown");
    }

    #[test]
    fn close_all_drops_handles() {
        let clock = TestClock::new();
        let cache = cache_with(&clock);
        let path = PathBuf::from("/db/f");
        cache
            .get::<_, ()>(&path, || Ok(1), false, None, None)
            .unwrap();
        assert_eq!(cache.status_for(&path), "ok");
        cache.close_all();
        assert_eq!(cache.status_for(&path), "unknown");
    }
}
