//! Bounded native clients selected from the resolved conversation profile.
//!
//! One entry owns the byte-stable prompt, tool snapshot and optional Python
//! extension child for a conversation. Cache metadata is synchronous and tiny;
//! factories, model calls, SQLite reads and lifecycle RPCs never run under its
//! lock.

use crate::agent::AgentClient;
use crate::agent_cache_pressure::{
    effective_idle_ttl, effective_max_size, plan_pressure_evictions, AgentCacheBounds,
};
use async_trait::async_trait;
use hermes_core::stream::StreamEvent;
use hermes_core::{Error, Message, Result};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

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
type CacheKey = (PathBuf, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RetirementKind {
    Release,
    SessionEnd,
}

struct CacheEntry {
    cell: Arc<ClientCell>,
    last_used: Instant,
    recency: u64,
    /// Covers model execution, durable assistant persistence and the
    /// post-persist provider finalizer as one indivisible eviction window.
    pending_turns: u32,
    retirement: Option<RetirementKind>,
    database_path: Option<PathBuf>,
    finalizable: bool,
}

struct RetiredEntry {
    key: CacheKey,
    cell: Arc<ClientCell>,
    database_path: Option<PathBuf>,
    kind: RetirementKind,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    recency: u64,
    shutting_down: bool,
}

pub struct ConversationAgent {
    fallback: Arc<dyn AgentClient>,
    factory: Box<Factory>,
    state: Mutex<CacheState>,
    bounds: AgentCacheBounds,
    changed: Notify,
    /// Serializes cache removal with retirement-task registration so shutdown
    /// cannot drain the task registry in the gap between those two steps.
    retirement_gate: Mutex<()>,
    retirement_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl ConversationAgent {
    const INITIAL_MAINTENANCE_DELAY: Duration = Duration::from_secs(60);
    const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(300);

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
        bounds: AgentCacheBounds,
    ) -> Self {
        Self {
            fallback,
            factory: Box::new(factory),
            state: Mutex::new(CacheState::default()),
            bounds,
            changed: Notify::new(),
            retirement_gate: Mutex::new(()),
            retirement_tasks: Mutex::new(Vec::new()),
        }
    }

    fn key(context: crate::agent::TurnContext<'_>, msg: &Message) -> Option<CacheKey> {
        Some((
            context.home?.to_owned(),
            crate::session_db::message_session_id(msg),
        ))
    }

    fn initialized_client(
        &self,
        context: crate::agent::TurnContext<'_>,
        session_id: &str,
    ) -> Option<Arc<dyn AgentClient>> {
        let key = (context.home?.to_owned(), session_id.to_owned());
        let cell = self
            .state
            .lock()
            .unwrap()
            .entries
            .get(&key)
            .map(|entry| entry.cell.clone())?;
        cell.get().cloned()
    }

    /// Pin an already-built conversation client across an observer call. A
    /// compression notification must not race TTL, pressure, or explicit
    /// retirement after its durable publication has committed.
    fn checkout_initialized_session(
        &self,
        home: &Path,
        session_id: &str,
    ) -> Option<(CacheKey, Arc<ClientCell>, Arc<dyn AgentClient>)> {
        let _gate = self.retirement_gate.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        if state.shutting_down {
            return None;
        }
        state.recency = state.recency.wrapping_add(1);
        let recency = state.recency;
        let key = (home.to_owned(), session_id.to_owned());
        let entry = state
            .entries
            .get_mut(&key)
            .filter(|entry| entry.retirement.is_none())?;
        let client = entry.cell.get()?.clone();
        entry.last_used = Instant::now();
        entry.recency = recency;
        entry.pending_turns = entry.pending_turns.saturating_add(1);
        Some((key, entry.cell.clone(), client))
    }

    /// Finish a compression-boundary observer and, on a successful physical
    /// rotation, atomically transfer the frozen client to the child session
    /// key. A transport/protocol failure retires the stale host so the child
    /// can rebuild cleanly on its next turn.
    fn finish_compression_boundary(
        &self,
        old_key: CacheKey,
        new_key: CacheKey,
        cell: &Arc<ClientCell>,
        notification_succeeded: bool,
    ) {
        let _gate = self.retirement_gate.lock().unwrap();
        let retired = {
            let mut state = self.state.lock().unwrap();
            let Some(current) = state
                .entries
                .get(&old_key)
                .filter(|entry| Arc::ptr_eq(&entry.cell, cell))
            else {
                self.changed.notify_one();
                return;
            };
            let existing_retirement = current.retirement;
            let mut entry = state.entries.remove(&old_key).unwrap();
            entry.pending_turns = entry.pending_turns.saturating_sub(1);
            entry.last_used = Instant::now();

            let target_occupied = old_key != new_key && state.entries.contains_key(&new_key);
            if !notification_succeeded || target_occupied {
                let kind = RetirementKind::Release;
                entry.retirement = Some(kind);
                if entry.pending_turns == 0 {
                    Some(Self::retired(old_key, entry, kind))
                } else {
                    state.entries.insert(old_key, entry);
                    None
                }
            } else if let Some(kind) = existing_retirement {
                entry.retirement = Some(kind);
                if entry.pending_turns == 0 {
                    // The host already observes the child identity. A pending
                    // hard retirement must therefore finalize the child
                    // transcript, not the archived compression parent.
                    Some(Self::retired(new_key, entry, kind))
                } else {
                    state.entries.insert(new_key, entry);
                    None
                }
            } else {
                state.entries.insert(new_key, entry);
                None
            }
        };
        self.changed.notify_one();
        if let Some(retired) = retired {
            self.schedule_retirements(vec![retired]);
        }
    }

    fn checkout(
        &self,
        key: &CacheKey,
        database: Option<&crate::session_db::SessionDb>,
        finalizable: bool,
        now: Instant,
    ) -> Result<Arc<ClientCell>> {
        let _gate = self.retirement_gate.lock().unwrap();
        let (cell, retired) = {
            let mut state = self.state.lock().unwrap();
            if state.shutting_down {
                return Err(Error::Other("conversation cache is shutting down".into()));
            }
            state.recency = state.recency.wrapping_add(1);
            let recency = state.recency;
            let entry = state
                .entries
                .entry(key.clone())
                .or_insert_with(|| CacheEntry {
                    cell: Arc::new(ClientCell::new()),
                    last_used: now,
                    recency,
                    pending_turns: 0,
                    retirement: None,
                    database_path: database.map(|db| db.database_path().to_owned()),
                    finalizable,
                });
            entry.last_used = now;
            entry.recency = recency;
            entry.pending_turns = entry.pending_turns.saturating_add(1);
            if let Some(database) = database {
                entry.database_path = Some(database.database_path().to_owned());
            }
            entry.finalizable = finalizable;
            let cell = entry.cell.clone();
            let retired = Self::enforce_cap_locked(&mut state, effective_max_size(&self.bounds));
            (cell, retired)
        };
        self.schedule_retirements(retired);
        Ok(cell)
    }

    fn enforce_cap_locked(state: &mut CacheState, cap: usize) -> Vec<RetiredEntry> {
        let excess = state.entries.len().saturating_sub(cap);
        if excess == 0 {
            return Vec::new();
        }
        let mut ordered: Vec<(CacheKey, u64)> = state
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.recency))
            .collect();
        ordered.sort_by_key(|(_, recency)| *recency);
        // Inspect only the excess LRU positions. An active old entry may leave
        // the cache temporarily over cap; do not substitute a newer prefix.
        ordered
            .into_iter()
            .take(excess)
            .filter_map(|(key, _)| {
                state
                    .entries
                    .get(&key)
                    .is_some_and(|entry| entry.pending_turns == 0)
                    .then(|| state.entries.remove(&key))
                    .flatten()
                    .map(|entry| {
                        let kind = if entry.finalizable {
                            RetirementKind::SessionEnd
                        } else {
                            RetirementKind::Release
                        };
                        Self::retired(key, entry, kind)
                    })
            })
            .collect()
    }

    fn retired(key: CacheKey, entry: CacheEntry, kind: RetirementKind) -> RetiredEntry {
        RetiredEntry {
            key,
            cell: entry.cell,
            database_path: entry.database_path,
            kind,
        }
    }

    fn finish_turn(&self, key: &CacheKey, cell: &Arc<ClientCell>, failed: bool) {
        let _gate = self.retirement_gate.lock().unwrap();
        let retired = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state
                .entries
                .get_mut(key)
                .filter(|entry| Arc::ptr_eq(&entry.cell, cell))
            else {
                return;
            };
            entry.pending_turns = entry.pending_turns.saturating_sub(1);
            entry.last_used = Instant::now();
            let kind = entry.retirement;
            let remove_empty = failed && entry.cell.get().is_none();
            if entry.pending_turns == 0 && (kind.is_some() || remove_empty) {
                let entry = state.entries.remove(key).unwrap();
                kind.map(|kind| Self::retired(key.clone(), entry, kind))
            } else {
                None
            }
        };
        self.changed.notify_one();
        if let Some(retired) = retired {
            self.schedule_retirements(vec![retired]);
        }
    }

    /// Mark a superseded session for hard retirement. If its finalizer is
    /// pending, retirement stays attached to the entry until that finalizer
    /// completes instead of racing it.
    pub fn retire_session(&self, home: &Path, session_id: &str) -> bool {
        self.remove_session(home, session_id, RetirementKind::SessionEnd)
    }

    pub fn release_session(&self, home: &Path, session_id: &str) -> bool {
        self.remove_session(home, session_id, RetirementKind::Release)
    }

    fn remove_session(&self, home: &Path, session_id: &str, kind: RetirementKind) -> bool {
        let _gate = self.retirement_gate.lock().unwrap();
        let key = (home.to_owned(), session_id.to_owned());
        let retired = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.get_mut(&key) else {
                return false;
            };
            entry.retirement = Some(kind);
            if entry.pending_turns == 0 {
                state
                    .entries
                    .remove(&key)
                    .map(|entry| Self::retired(key, entry, kind))
            } else {
                None
            }
        };
        if let Some(retired) = retired {
            self.schedule_retirements(vec![retired]);
        }
        true
    }

    pub fn sweep_idle(&self) -> usize {
        self.sweep_idle_at(Instant::now())
    }

    fn sweep_idle_at(&self, now: Instant) -> usize {
        let _gate = self.retirement_gate.lock().unwrap();
        let ttl = effective_idle_ttl(&self.bounds);
        let retired = {
            let mut state = self.state.lock().unwrap();
            let keys: Vec<_> = state
                .entries
                .iter()
                .filter(|(_, entry)| {
                    entry.pending_turns == 0
                        && !entry.finalizable
                        && now.saturating_duration_since(entry.last_used).as_secs_f64() > ttl
                })
                .map(|(key, _)| key.clone())
                .collect();
            keys.into_iter()
                .filter_map(|key| {
                    state
                        .entries
                        .remove(&key)
                        .map(|entry| Self::retired(key, entry, RetirementKind::Release))
                })
                .collect::<Vec<_>>()
        };
        let count = retired.len();
        self.schedule_retirements(retired);
        count
    }

    pub fn sweep_pressure(&self) -> usize {
        self.sweep_pressure_at(crate::agent_cache_pressure::read_process_tree_anon_rss_mb())
    }

    /// Run the cache's periodic TTL and memory-pressure maintenance until the
    /// gateway shutdown token fires.
    pub fn start_maintenance(
        self: &Arc<Self>,
        shutdown: tokio_util::sync::CancellationToken,
        session_store: Option<Arc<crate::session_store::SessionStore>>,
    ) {
        let cache = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(Self::INITIAL_MAINTENANCE_DELAY) => {}
            }
            loop {
                if let Some(store) = &session_store {
                    let store_for_read = store.clone();
                    match tokio::task::spawn_blocking(move || store_for_read.expired_sessions())
                        .await
                    {
                        Ok(expired) => {
                            for session in expired {
                                match cache.expire_session(store.clone(), session).await {
                                    Ok(true) => {}
                                    Ok(false) => tracing::debug!(
                                        "expired conversation is still finalizing a turn"
                                    ),
                                    Err(error) => {
                                        tracing::warn!(%error, "expired conversation finalization failed")
                                    }
                                }
                            }
                        }
                        Err(error) => tracing::warn!(%error, "session expiry scan failed"),
                    }
                }
                let idle = cache.sweep_idle();
                let pressure = cache.sweep_pressure();
                if idle > 0 || pressure > 0 {
                    tracing::info!(
                        idle,
                        pressure,
                        "conversation cache maintenance retired clients"
                    );
                }
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Self::MAINTENANCE_INTERVAL) => {}
                }
            }
        });
    }

    /// Finalize one policy-expired session. Active turns are left untouched and
    /// retried by the next watcher pass. Provider teardown completes before the
    /// idempotent durable expiry marker is written.
    pub async fn expire_session(
        &self,
        store: Arc<crate::session_store::SessionStore>,
        expired: crate::session_store::ExpiredSession,
    ) -> anyhow::Result<bool> {
        let key = (expired.home.clone(), expired.session_id.clone());
        let gate = self.retirement_gate.lock().unwrap();
        let retired = {
            let mut state = self.state.lock().unwrap();
            if state.shutting_down {
                anyhow::bail!("conversation cache is shutting down");
            }
            match state.entries.get(&key) {
                Some(entry) if entry.pending_turns != 0 => return Ok(false),
                Some(_) => state
                    .entries
                    .remove(&key)
                    .map(|entry| Self::retired(key, entry, RetirementKind::SessionEnd)),
                None => None,
            }
        };
        let (completed, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                if let Some(retired) = retired {
                    close_retired(retired).await?;
                }
                tokio::task::spawn_blocking(move || store.finalize_expired_session(&expired))
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("session expiry persistence worker failed: {error}")
                    })??;
                Ok(())
            }
            .await;
            let _ = completed.send(result);
        });
        self.register_retirement_task(task);
        drop(gate);
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("session expiry task ended without a result"))??;
        Ok(true)
    }

    fn sweep_pressure_at(&self, rss_mb: Option<i64>) -> usize {
        let Some(budget) = self.bounds.memory_high_mb else {
            return 0;
        };
        let Some(rss_mb) = rss_mb.filter(|rss| *rss >= budget) else {
            return 0;
        };
        let _gate = self.retirement_gate.lock().unwrap();
        let retired = {
            let mut state = self.state.lock().unwrap();
            let mut ordered: Vec<(CacheKey, (u64, bool))> = state
                .entries
                .iter()
                .map(|(key, entry)| (key.clone(), (entry.recency, entry.pending_turns == 0)))
                .collect();
            ordered.sort_by_key(|(_, (recency, _))| *recency);
            let plan = plan_pressure_evictions(
                ordered,
                |_, (_, evictable)| *evictable,
                self.bounds.max_evictions_per_pass,
                self.bounds.protect_recent,
            );
            plan.into_iter()
                .filter_map(|(key, _)| {
                    state.entries.remove(&key).map(|entry| {
                        let kind = if entry.finalizable {
                            RetirementKind::SessionEnd
                        } else {
                            RetirementKind::Release
                        };
                        Self::retired(key, entry, kind)
                    })
                })
                .collect::<Vec<_>>()
        };
        let count = retired.len();
        if count > 0 {
            tracing::warn!(
                rss_mb,
                budget,
                count,
                "retiring LRU conversation clients under memory pressure"
            );
        }
        self.schedule_retirements(retired);
        count
    }

    fn schedule_retirements(&self, retired: Vec<RetiredEntry>) {
        if retired.is_empty() {
            return;
        }
        let task = tokio::spawn(async move {
            for result in
                futures_util::future::join_all(retired.into_iter().map(close_retired)).await
            {
                if let Err(error) = result {
                    tracing::warn!(%error, "conversation client retirement failed");
                }
            }
        });
        self.register_retirement_task(task);
    }

    fn register_retirement_task(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self.retirement_tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// Stop new cached turns, allow pending durable finalizers a short grace,
    /// then hard-close every cached or already-retiring client within `budget`.
    pub async fn shutdown(&self, budget: Duration) -> usize {
        let started = Instant::now();
        let mut retired_count = 0;
        {
            let _gate = self.retirement_gate.lock().unwrap();
            let mut state = self.state.lock().unwrap();
            state.shutting_down = true;
            for entry in state.entries.values_mut() {
                entry.retirement = Some(RetirementKind::SessionEnd);
            }
        }

        let active_grace = budget.min(Duration::from_secs(10));
        loop {
            let gate = self.retirement_gate.lock().unwrap();
            let retired = {
                let mut state = self.state.lock().unwrap();
                let keys: Vec<_> = state
                    .entries
                    .iter()
                    .filter(|(_, entry)| entry.pending_turns == 0)
                    .map(|(key, _)| key.clone())
                    .collect();
                keys.into_iter()
                    .filter_map(|key| {
                        state
                            .entries
                            .remove(&key)
                            .map(|entry| Self::retired(key, entry, RetirementKind::SessionEnd))
                    })
                    .collect::<Vec<_>>()
            };
            retired_count += retired.len();
            self.schedule_retirements(retired);
            drop(gate);
            if self.state.lock().unwrap().entries.is_empty() || started.elapsed() >= active_grace {
                break;
            }
            let wait = active_grace.saturating_sub(started.elapsed());
            if tokio::time::timeout(wait, self.changed.notified())
                .await
                .is_err()
            {
                break;
            }
        }

        let gate = self.retirement_gate.lock().unwrap();
        // A process shutdown is bounded. Once the grace expires, a still-live
        // turn may lose its external-memory finalizer, matching Python's
        // bounded shutdown behavior instead of keeping the gateway alive.
        let forced = {
            let mut state = self.state.lock().unwrap();
            std::mem::take(&mut state.entries)
                .into_iter()
                .map(|(key, entry)| Self::retired(key, entry, RetirementKind::SessionEnd))
                .collect::<Vec<_>>()
        };
        retired_count += forced.len();
        self.schedule_retirements(forced);
        drop(gate);

        let mut tasks = {
            let _gate = self.retirement_gate.lock().unwrap();
            std::mem::take(&mut *self.retirement_tasks.lock().unwrap())
        };
        let wait_budget = budget.saturating_sub(started.elapsed());
        if tokio::time::timeout(wait_budget, async {
            for task in &mut tasks {
                let _ = task.await;
            }
        })
        .await
        .is_err()
        {
            for task in tasks {
                task.abort();
            }
        }
        retired_count
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    #[cfg(test)]
    fn contains(&self, home: &Path, session_id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .entries
            .contains_key(&(home.to_owned(), session_id.to_owned()))
    }

    #[cfg(test)]
    fn pending(&self, home: &Path, session_id: &str) -> Option<u32> {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(&(home.to_owned(), session_id.to_owned()))
            .map(|entry| entry.pending_turns)
    }
}

async fn close_retired(retired: RetiredEntry) -> Result<()> {
    let Some(client) = retired.cell.get().cloned() else {
        return Ok(());
    };
    let messages = if retired.kind == RetirementKind::SessionEnd {
        match retired.database_path {
            Some(path) => {
                let session_id = retired.key.1.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::session_db::SessionDb::open_shared(path)?
                        .load_lifecycle_messages(&session_id)
                })
                .await
                {
                    Ok(Ok(messages)) => Some(messages),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, session_id = %retired.key.1, "could not load transcript for conversation retirement");
                        Some(Vec::new())
                    }
                    Err(error) => {
                        tracing::warn!(%error, session_id = %retired.key.1, "conversation retirement transcript worker failed");
                        Some(Vec::new())
                    }
                }
            }
            None => Some(Vec::new()),
        }
    } else {
        None
    };
    client.close_conversation(messages.as_deref()).await
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
        let Some(key) = Self::key(context, msg) else {
            return self.run_turn(msg, history, events).await;
        };
        let cell = self.checkout(
            &key,
            context.database,
            context.session_finalizable,
            Instant::now(),
        )?;
        let initialized = cell
            .get_or_try_init(|| async {
                (self.factory)(key.0.as_path(), msg, history, context.database)
                    .await
                    .map_err(|error| {
                        Error::Other(format!("conversation agent initialization failed: {error}"))
                    })
            })
            .await;
        let client = match initialized {
            Ok(client) => client.clone(),
            Err(error) => {
                self.finish_turn(&key, &cell, true);
                return Err(error);
            }
        };
        client
            .run_turn_with_context(context, msg, history, events)
            .await?;
        // The pending count stays armed through durable persistence and the
        // separate post-persist finalizer.
        Ok(())
    }

    async fn summarize_context(
        &self,
        context: crate::agent::TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::CompressionHistoryMessage],
        focus_topic: Option<&str>,
    ) -> Result<Option<String>> {
        self.summarize_context_with_memory(context, msg, history, focus_topic, None)
            .await
    }

    async fn summarize_context_with_memory(
        &self,
        context: crate::agent::TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::CompressionHistoryMessage],
        focus_topic: Option<&str>,
        memory_context: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(key) = Self::key(context, msg) else {
            return self
                .fallback
                .summarize_context_with_memory(context, msg, history, focus_topic, memory_context)
                .await;
        };
        let cell = self.checkout(
            &key,
            context.database,
            context.session_finalizable,
            Instant::now(),
        )?;
        let conversation_history = history
            .iter()
            .map(|item| item.message.clone())
            .collect::<Vec<_>>();
        let initialized = cell
            .get_or_try_init(|| async {
                (self.factory)(
                    key.0.as_path(),
                    msg,
                    &conversation_history,
                    context.database,
                )
                .await
                .map_err(|error| {
                    Error::Other(format!("conversation agent initialization failed: {error}"))
                })
            })
            .await;
        let client = match initialized {
            Ok(client) => client.clone(),
            Err(error) => {
                self.finish_turn(&key, &cell, true);
                return Err(error);
            }
        };
        let result = client
            .summarize_context_with_memory(context, msg, history, focus_topic, memory_context)
            .await;
        self.finish_turn(&key, &cell, result.is_err());
        result
    }

    async fn prepare_pre_compression_checkpoint(
        &self,
        context: crate::agent::TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::CompressionHistoryMessage],
        require_checkpoint: bool,
    ) -> Result<crate::agent::PreCompressionCheckpoint> {
        let Some(key) = Self::key(context, msg) else {
            return self
                .fallback
                .prepare_pre_compression_checkpoint(context, msg, history, require_checkpoint)
                .await;
        };
        let cell = self.checkout(
            &key,
            context.database,
            context.session_finalizable,
            Instant::now(),
        )?;
        let conversation_history = history
            .iter()
            .map(|item| item.message.clone())
            .collect::<Vec<_>>();
        let initialized = cell
            .get_or_try_init(|| async {
                (self.factory)(
                    key.0.as_path(),
                    msg,
                    &conversation_history,
                    context.database,
                )
                .await
                .map_err(|error| {
                    Error::Other(format!("conversation agent initialization failed: {error}"))
                })
            })
            .await;
        let client = match initialized {
            Ok(client) => client.clone(),
            Err(error) => {
                self.finish_turn(&key, &cell, true);
                return Err(error);
            }
        };
        let result = client
            .prepare_pre_compression_checkpoint(context, msg, history, require_checkpoint)
            .await;
        self.finish_turn(&key, &cell, result.is_err());
        result
    }

    async fn notify_compression_boundary(
        &self,
        context: crate::agent::TurnContext<'_>,
        old_session_id: &str,
        new_session_id: &str,
        in_place: bool,
    ) -> Result<()> {
        let Some(home) = context.home else {
            return self
                .fallback
                .notify_compression_boundary(context, old_session_id, new_session_id, in_place)
                .await;
        };
        let Some((old_key, cell, client)) = self.checkout_initialized_session(home, old_session_id)
        else {
            return Ok(());
        };
        let result = client
            .notify_compression_boundary(context, old_session_id, new_session_id, in_place)
            .await;
        self.finish_compression_boundary(
            old_key,
            (home.to_owned(), new_session_id.to_owned()),
            &cell,
            result.is_ok(),
        );
        result
    }

    async fn compression_preflight(
        &self,
        context: crate::agent::TurnContext<'_>,
        msg: &Message,
        history: &[crate::session_db::HistoryMessage],
    ) -> Result<Option<crate::agent::CompressionPreflight>> {
        let Some(key) = Self::key(context, msg) else {
            return self
                .fallback
                .compression_preflight(context, msg, history)
                .await;
        };
        let cell = self.checkout(
            &key,
            context.database,
            context.session_finalizable,
            Instant::now(),
        )?;
        let initialized = cell
            .get_or_try_init(|| async {
                (self.factory)(key.0.as_path(), msg, history, context.database)
                    .await
                    .map_err(|error| {
                        Error::Other(format!("conversation agent initialization failed: {error}"))
                    })
            })
            .await;
        let client = match initialized {
            Ok(client) => client.clone(),
            Err(error) => {
                self.finish_turn(&key, &cell, true);
                return Err(error);
            }
        };
        let result = client.compression_preflight(context, msg, history).await;
        self.finish_turn(&key, &cell, result.is_err());
        result
    }

    fn compression_structural_backoff_remaining(
        &self,
        context: crate::agent::TurnContext<'_>,
        session_id: &str,
    ) -> Option<Duration> {
        match self.initialized_client(context, session_id) {
            Some(client) => client.compression_structural_backoff_remaining(context, session_id),
            None if context.home.is_none() => self
                .fallback
                .compression_structural_backoff_remaining(context, session_id),
            None => None,
        }
    }

    fn record_compression_structural_no_op(
        &self,
        context: crate::agent::TurnContext<'_>,
        session_id: &str,
        reason: &str,
    ) {
        match self.initialized_client(context, session_id) {
            Some(client) => client.record_compression_structural_no_op(context, session_id, reason),
            None if context.home.is_none() => self
                .fallback
                .record_compression_structural_no_op(context, session_id, reason),
            None => {}
        }
    }

    fn clear_compression_structural_backoff(
        &self,
        context: crate::agent::TurnContext<'_>,
        session_id: &str,
    ) {
        match self.initialized_client(context, session_id) {
            Some(client) => client.clear_compression_structural_backoff(context, session_id),
            None if context.home.is_none() => self
                .fallback
                .clear_compression_structural_backoff(context, session_id),
            None => {}
        }
    }

    async fn finalize_turn_after_persist(
        &self,
        context: crate::agent::TurnContext<'_>,
        msg: &Message,
        reply: &str,
        succeeded: bool,
    ) -> Result<()> {
        let Some(key) = Self::key(context, msg) else {
            return self
                .fallback
                .finalize_turn_after_persist(context, msg, reply, succeeded)
                .await;
        };
        let cell = self
            .state
            .lock()
            .unwrap()
            .entries
            .get(&key)
            .map(|entry| entry.cell.clone());
        let result = match cell.as_ref().and_then(|cell| cell.get().cloned()) {
            Some(client) => {
                client
                    .finalize_turn_after_persist(context, msg, reply, succeeded)
                    .await
            }
            None => Ok(()),
        };
        if let Some(cell) = &cell {
            self.finish_turn(&key, cell, false);
        }
        result
    }

    fn retire_conversation(
        &self,
        context: crate::agent::TurnContext<'_>,
        session_id: &str,
    ) -> bool {
        context
            .home
            .is_some_and(|home| self.retire_session(home, session_id))
    }

    fn release_conversation(
        &self,
        context: crate::agent::TurnContext<'_>,
        session_id: &str,
    ) -> bool {
        context
            .home
            .is_some_and(|home| self.release_session(home, session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct RecordedAgent {
        label: String,
        calls: Arc<Mutex<Vec<String>>>,
        closes: Arc<Mutex<Vec<bool>>>,
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

        async fn summarize_context(
            &self,
            _context: crate::agent::TurnContext<'_>,
            _msg: &Message,
            _history: &[crate::session_db::CompressionHistoryMessage],
            _focus_topic: Option<&str>,
        ) -> Result<Option<String>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("summary:{}", self.label));
            Ok(Some("summary".into()))
        }

        async fn prepare_pre_compression_checkpoint(
            &self,
            _context: crate::agent::TurnContext<'_>,
            _msg: &Message,
            _history: &[crate::session_db::CompressionHistoryMessage],
            require_checkpoint: bool,
        ) -> Result<crate::agent::PreCompressionCheckpoint> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("checkpoint:{require_checkpoint}:{}", self.label));
            Ok(crate::agent::PreCompressionCheckpoint {
                checkpoint_supported: true,
                memory_context: Some("memory".into()),
            })
        }

        async fn summarize_context_with_memory(
            &self,
            _context: crate::agent::TurnContext<'_>,
            _msg: &Message,
            _history: &[crate::session_db::CompressionHistoryMessage],
            _focus_topic: Option<&str>,
            memory_context: Option<&str>,
        ) -> Result<Option<String>> {
            self.calls.lock().unwrap().push(format!(
                "summary-memory:{}:{}",
                memory_context.unwrap_or("none"),
                self.label
            ));
            Ok(Some("summary".into()))
        }

        async fn notify_compression_boundary(
            &self,
            _context: crate::agent::TurnContext<'_>,
            old_session_id: &str,
            new_session_id: &str,
            in_place: bool,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(format!(
                "boundary:{old_session_id}:{new_session_id}:{in_place}:{}",
                self.label
            ));
            Ok(())
        }

        async fn compression_preflight(
            &self,
            _context: crate::agent::TurnContext<'_>,
            _msg: &Message,
            _history: &[crate::session_db::HistoryMessage],
        ) -> Result<Option<crate::agent::CompressionPreflight>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("preflight:{}", self.label));
            Ok(Some(crate::agent::CompressionPreflight {
                model: self.label.clone(),
                context_length: 1_000,
                max_output_tokens: Some(100),
                request_tokens: 500,
                stale_thinking_on_wire: false,
            }))
        }

        async fn close_conversation(
            &self,
            session_messages: Option<&[serde_json::Value]>,
        ) -> Result<()> {
            self.closes.lock().unwrap().push(session_messages.is_some());
            Ok(())
        }
    }

    fn message(session_id: &str) -> Message {
        let mut message: Message = serde_json::from_value(serde_json::json!({
            "platform":"cli", "channel_id":"same", "sender_id":"user", "text":"hello"
        }))
        .unwrap();
        message.resolved_session_id = Some(session_id.into());
        message
    }

    fn context(home: &Path) -> crate::agent::TurnContext<'_> {
        crate::agent::TurnContext {
            home: Some(home),
            database: None,
            turn_lease_holder: None,
            session_finalizable: false,
        }
    }

    fn recorded_factory(
        calls: Arc<Mutex<Vec<String>>>,
        closes: Arc<Mutex<Vec<bool>>>,
        builds: Arc<AtomicUsize>,
    ) -> impl for<'a> Fn(
        &'a Path,
        &'a Message,
        &'a [crate::session_db::HistoryMessage],
        Option<&'a crate::session_db::SessionDb>,
    ) -> FactoryFuture<'a> {
        move |home, _, _, _| {
            let label = format!(
                "{}:{}",
                home.display(),
                builds.fetch_add(1, Ordering::SeqCst)
            );
            let calls = calls.clone();
            let closes = closes.clone();
            Box::pin(async move {
                Ok(Arc::new(RecordedAgent {
                    label,
                    calls,
                    closes,
                }) as Arc<dyn AgentClient>)
            })
        }
    }

    async fn turn(agent: &ConversationAgent, home: &Path, message: &Message) -> Result<()> {
        let (tx, _rx) = mpsc::channel(8);
        let context = context(home);
        agent
            .run_turn_with_context(context, message, &[], tx)
            .await?;
        agent
            .finalize_turn_after_persist(context, message, "answer", true)
            .await
    }

    async fn finalizable_turn(
        agent: &ConversationAgent,
        home: &Path,
        message: &Message,
    ) -> Result<()> {
        let (tx, _rx) = mpsc::channel(8);
        let context = context(home).with_session_finalizable(true);
        agent
            .run_turn_with_context(context, message, &[], tx)
            .await?;
        agent
            .finalize_turn_after_persist(context, message, "answer", true)
            .await
    }

    #[tokio::test]
    async fn structural_backoff_routes_to_the_initialized_conversation_client() {
        struct BackoffAgent {
            armed: Arc<AtomicBool>,
        }

        #[async_trait]
        impl AgentClient for BackoffAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                _: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                Ok(())
            }

            fn compression_structural_backoff_remaining(
                &self,
                _: crate::agent::TurnContext<'_>,
                _: &str,
            ) -> Option<Duration> {
                self.armed
                    .load(Ordering::SeqCst)
                    .then_some(Duration::from_secs(300))
            }

            fn record_compression_structural_no_op(
                &self,
                _: crate::agent::TurnContext<'_>,
                _: &str,
                _: &str,
            ) {
                self.armed.store(true, Ordering::SeqCst);
            }

            fn clear_compression_structural_backoff(
                &self,
                _: crate::agent::TurnContext<'_>,
                _: &str,
            ) {
                self.armed.store(false, Ordering::SeqCst);
            }
        }

        let armed = Arc::new(AtomicBool::new(false));
        let fallback = Arc::new(BackoffAgent {
            armed: Arc::new(AtomicBool::new(false)),
        });
        let factory_armed = armed.clone();
        let agent = ConversationAgent::new(
            fallback,
            move |_, _, _, _| {
                let armed = factory_armed.clone();
                Box::pin(
                    async move { Ok(Arc::new(BackoffAgent { armed }) as Arc<dyn AgentClient>) },
                )
            },
            AgentCacheBounds::default(),
        );
        let home = Path::new("structural-home");
        let message = message("structural-session");
        turn(&agent, home, &message).await.unwrap();
        let context = context(home);

        agent.record_compression_structural_no_op(
            context,
            "structural-session",
            "no complete window",
        );
        assert_eq!(
            agent.compression_structural_backoff_remaining(context, "structural-session"),
            Some(Duration::from_secs(300))
        );
        assert!(armed.load(Ordering::SeqCst));

        agent.clear_compression_structural_backoff(context, "structural-session");
        assert_eq!(
            agent.compression_structural_backoff_remaining(context, "structural-session"),
            None
        );
        assert!(!armed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn routed_clients_preserve_profile_and_conversation_identity() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicUsize::new(0));
        let fallback = Arc::new(RecordedAgent {
            label: "fallback".into(),
            calls: calls.clone(),
            closes: closes.clone(),
        });
        let agent = ConversationAgent::new(
            fallback,
            recorded_factory(calls.clone(), closes, builds.clone()),
            AgentCacheBounds::default(),
        );
        let one = message("one");
        for home in [Path::new("red"), Path::new("red"), Path::new("blue")] {
            turn(&agent, home, &one).await.unwrap();
        }
        let two = message("two");
        turn(&agent, Path::new("red"), &two).await.unwrap();
        turn(&agent, Path::new("red"), &one).await.unwrap();
        let (tx, _rx) = mpsc::channel(8);
        agent.run_turn(&one, &[], tx).await.unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["red:0", "red:0", "blue:1", "red:2", "red:0", "fallback"]
        );
        assert_eq!(builds.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn compression_boundary_rekeys_the_frozen_client_after_notification() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicUsize::new(0));
        let agent = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            recorded_factory(calls.clone(), closes.clone(), builds.clone()),
            AgentCacheBounds::default(),
        );
        let home = Path::new("rotation-home");
        turn(&agent, home, &message("parent")).await.unwrap();

        agent
            .notify_compression_boundary(context(home), "parent", "child", false)
            .await
            .unwrap();

        assert!(!agent.contains(home, "parent"));
        assert!(agent.contains(home, "child"));
        turn(&agent, home, &message("child")).await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "rotation-home:0",
                "boundary:parent:child:false:rotation-home:0",
                "rotation-home:0"
            ]
        );
        assert!(closes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn in_place_boundary_keeps_the_existing_cache_key() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicUsize::new(0));
        let agent = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            recorded_factory(calls.clone(), closes.clone(), builds.clone()),
            AgentCacheBounds::default(),
        );
        let home = Path::new("in-place-home");
        turn(&agent, home, &message("same")).await.unwrap();

        agent
            .notify_compression_boundary(context(home), "same", "same", true)
            .await
            .unwrap();

        assert!(agent.contains(home, "same"));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            ["in-place-home:0", "boundary:same:same:true:in-place-home:0"]
        );
        assert!(closes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_boundary_retires_the_stale_client_without_rekeying() {
        struct FailingBoundaryAgent {
            closes: Arc<Mutex<Vec<bool>>>,
        }

        #[async_trait]
        impl AgentClient for FailingBoundaryAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                _: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                Ok(())
            }

            async fn notify_compression_boundary(
                &self,
                _: crate::agent::TurnContext<'_>,
                _: &str,
                _: &str,
                _: bool,
            ) -> Result<()> {
                Err(Error::Other("extension host transport failed".into()))
            }

            async fn close_conversation(
                &self,
                messages: Option<&[serde_json::Value]>,
            ) -> Result<()> {
                self.closes.lock().unwrap().push(messages.is_some());
                Ok(())
            }
        }

        let closes = Arc::new(Mutex::new(Vec::new()));
        let fallback = Arc::new(FailingBoundaryAgent {
            closes: Arc::new(Mutex::new(Vec::new())),
        });
        let factory_closes = closes.clone();
        let agent = ConversationAgent::new(
            fallback,
            move |_, _, _, _| {
                let closes = factory_closes.clone();
                Box::pin(async move {
                    Ok(Arc::new(FailingBoundaryAgent { closes }) as Arc<dyn AgentClient>)
                })
            },
            AgentCacheBounds::default(),
        );
        let home = Path::new("failed-boundary-home");
        turn(&agent, home, &message("parent")).await.unwrap();

        let error = agent
            .notify_compression_boundary(context(home), "parent", "child", false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("transport failed"));
        tokio::task::yield_now().await;

        assert!(!agent.contains(home, "parent"));
        assert!(!agent.contains(home, "child"));
        assert_eq!(*closes.lock().unwrap(), [false]);
    }

    #[tokio::test]
    async fn occupied_child_key_wins_over_a_boundary_rekey() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicUsize::new(0));
        let agent = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            recorded_factory(calls.clone(), closes.clone(), builds.clone()),
            AgentCacheBounds::default(),
        );
        let home = Path::new("occupied-child-home");
        turn(&agent, home, &message("parent")).await.unwrap();
        turn(&agent, home, &message("child")).await.unwrap();

        agent
            .notify_compression_boundary(context(home), "parent", "child", false)
            .await
            .unwrap();
        tokio::task::yield_now().await;

        assert!(!agent.contains(home, "parent"));
        assert!(agent.contains(home, "child"));
        turn(&agent, home, &message("child")).await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(*closes.lock().unwrap(), [false]);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "occupied-child-home:0",
                "occupied-child-home:1",
                "boundary:parent:child:false:occupied-child-home:0",
                "occupied-child-home:1"
            ]
        );
    }

    #[tokio::test]
    async fn pending_hard_retirement_follows_a_successful_boundary_rekey() {
        struct BlockingBoundaryAgent {
            entered: Arc<Notify>,
            release: Arc<Notify>,
            closed: Arc<Notify>,
            closes: Arc<Mutex<Vec<bool>>>,
        }

        #[async_trait]
        impl AgentClient for BlockingBoundaryAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                _: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                Ok(())
            }

            async fn notify_compression_boundary(
                &self,
                _: crate::agent::TurnContext<'_>,
                _: &str,
                _: &str,
                _: bool,
            ) -> Result<()> {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(())
            }

            async fn close_conversation(
                &self,
                messages: Option<&[serde_json::Value]>,
            ) -> Result<()> {
                self.closes.lock().unwrap().push(messages.is_some());
                self.closed.notify_one();
                Ok(())
            }
        }

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let closed = Arc::new(Notify::new());
        let closes = Arc::new(Mutex::new(Vec::new()));
        let factory_entered = entered.clone();
        let factory_release = release.clone();
        let factory_closed = closed.clone();
        let factory_closes = closes.clone();
        let agent = Arc::new(ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: Arc::new(Mutex::new(Vec::new())),
                closes: Arc::new(Mutex::new(Vec::new())),
            }),
            move |_, _, _, _| {
                let entered = factory_entered.clone();
                let release = factory_release.clone();
                let closed = factory_closed.clone();
                let closes = factory_closes.clone();
                Box::pin(async move {
                    Ok(Arc::new(BlockingBoundaryAgent {
                        entered,
                        release,
                        closed,
                        closes,
                    }) as Arc<dyn AgentClient>)
                })
            },
            AgentCacheBounds::default(),
        ));
        let home = Path::new("retired-boundary-home");
        turn(&agent, home, &message("parent")).await.unwrap();

        let notifying_agent = agent.clone();
        let notification = tokio::spawn(async move {
            notifying_agent
                .notify_compression_boundary(
                    context(Path::new("retired-boundary-home")),
                    "parent",
                    "child",
                    false,
                )
                .await
        });
        entered.notified().await;

        assert_eq!(agent.pending(home, "parent"), Some(1));
        assert!(agent.retire_session(home, "parent"));
        assert!(agent.contains(home, "parent"));
        release.notify_one();
        notification.await.unwrap().unwrap();
        closed.notified().await;

        assert!(!agent.contains(home, "parent"));
        assert!(!agent.contains(home, "child"));
        assert_eq!(*closes.lock().unwrap(), [true]);
    }

    #[tokio::test]
    async fn summary_uses_the_conversation_client_and_release_is_not_session_end() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicUsize::new(0));
        let agent = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            recorded_factory(calls.clone(), closes.clone(), builds.clone()),
            AgentCacheBounds::default(),
        );
        let message = message("compressed");
        assert_eq!(
            agent
                .compression_preflight(context(Path::new("home")), &message, &[])
                .await
                .unwrap()
                .unwrap()
                .request_tokens,
            500
        );
        assert_eq!(
            agent
                .summarize_context(context(Path::new("home")), &message, &[], Some("focus"))
                .await
                .unwrap()
                .as_deref(),
            Some("summary")
        );
        let checkpoint = agent
            .prepare_pre_compression_checkpoint(context(Path::new("home")), &message, &[], true)
            .await
            .unwrap();
        assert!(checkpoint.checkpoint_supported);
        assert_eq!(checkpoint.memory_context.as_deref(), Some("memory"));
        assert_eq!(
            agent
                .summarize_context_with_memory(
                    context(Path::new("home")),
                    &message,
                    &[],
                    Some("focus"),
                    checkpoint.memory_context.as_deref(),
                )
                .await
                .unwrap()
                .as_deref(),
            Some("summary")
        );
        assert!(agent.contains(Path::new("home"), "compressed"));
        assert!(agent.release_conversation(context(Path::new("home")), "compressed"));
        tokio::task::yield_now().await;
        assert!(!agent.contains(Path::new("home"), "compressed"));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "preflight:home:0",
                "summary-memory:none:home:0",
                "checkpoint:true:home:0",
                "summary-memory:memory:home:0"
            ]
        );
        assert_eq!(*closes.lock().unwrap(), [false]);
    }

    #[tokio::test]
    async fn cap_is_lru_and_finalizer_protected() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicUsize::new(0));
        let bounds = AgentCacheBounds {
            max_size: Some(2),
            memory_high_mb: None,
            ..Default::default()
        };
        let agent = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            recorded_factory(calls, closes.clone(), builds),
            bounds,
        );
        turn(&agent, Path::new("home"), &message("a"))
            .await
            .unwrap();
        turn(&agent, Path::new("home"), &message("b"))
            .await
            .unwrap();
        turn(&agent, Path::new("home"), &message("c"))
            .await
            .unwrap();
        assert!(!agent.contains(Path::new("home"), "a"));
        assert!(agent.contains(Path::new("home"), "b"));
        assert!(agent.contains(Path::new("home"), "c"));
        tokio::task::yield_now().await;
        assert_eq!(*closes.lock().unwrap(), [false]);

        let (tx, _rx) = mpsc::channel(8);
        let active = message("b");
        agent
            .run_turn_with_context(context(Path::new("home")), &active, &[], tx)
            .await
            .unwrap();
        turn(&agent, Path::new("home"), &message("d"))
            .await
            .unwrap();
        assert_eq!(agent.len(), 2);
        assert!(agent.contains(Path::new("home"), "b"));
        assert!(agent.contains(Path::new("home"), "d"));
        assert!(!agent.contains(Path::new("home"), "c"));
        agent
            .finalize_turn_after_persist(context(Path::new("home")), &active, "answer", true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reset_retirement_waits_for_post_persist_finalizer() {
        struct FinalizerAgent {
            finalized: Arc<AtomicUsize>,
            closed: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl AgentClient for FinalizerAgent {
            async fn run_turn(
                &self,
                _msg: &Message,
                _history: &[crate::session_db::HistoryMessage],
                _events: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                Ok(())
            }
            async fn finalize_turn_after_persist(
                &self,
                _context: crate::agent::TurnContext<'_>,
                _msg: &Message,
                _reply: &str,
                _succeeded: bool,
            ) -> Result<()> {
                self.finalized.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn close_conversation(
                &self,
                _messages: Option<&[serde_json::Value]>,
            ) -> Result<()> {
                self.closed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let finalized = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let inner_finalized = finalized.clone();
        let inner_closed = closed.clone();
        let agent = ConversationAgent::new(
            Arc::new(FinalizerAgent {
                finalized: finalized.clone(),
                closed: closed.clone(),
            }),
            move |_, _, _, _| {
                let finalized = inner_finalized.clone();
                let closed = inner_closed.clone();
                Box::pin(async move {
                    Ok(Arc::new(FinalizerAgent { finalized, closed }) as Arc<dyn AgentClient>)
                })
            },
            AgentCacheBounds::default(),
        );
        let message = message("pending");
        let (tx, _rx) = mpsc::channel(8);
        agent
            .run_turn_with_context(context(Path::new("home")), &message, &[], tx)
            .await
            .unwrap();
        assert!(agent.retire_session(Path::new("home"), "pending"));
        assert!(agent.contains(Path::new("home"), "pending"));
        assert_eq!(closed.load(Ordering::SeqCst), 0);
        agent
            .finalize_turn_after_persist(context(Path::new("home")), &message, "answer", true)
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(finalized.load(Ordering::SeqCst), 1);
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert!(!agent.contains(Path::new("home"), "pending"));
    }

    #[tokio::test]
    async fn idle_and_pressure_use_distinct_modes_and_skip_pending_turns() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicUsize::new(0));
        let bounds = AgentCacheBounds {
            max_size: Some(10),
            idle_ttl_secs: Some(1.0),
            memory_high_mb: Some(10),
            max_evictions_per_pass: 2,
            protect_recent: 0,
        };
        let agent = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            recorded_factory(calls, closes.clone(), builds),
            bounds,
        );
        turn(&agent, Path::new("home"), &message("idle"))
            .await
            .unwrap();
        assert_eq!(
            agent.sweep_idle_at(Instant::now() + Duration::from_secs(2)),
            1
        );
        tokio::task::yield_now().await;
        assert_eq!(*closes.lock().unwrap(), [false]);

        finalizable_turn(&agent, Path::new("home"), &message("pressure"))
            .await
            .unwrap();
        assert_eq!(
            agent.sweep_idle_at(Instant::now() + Duration::from_secs(2)),
            0
        );
        let pending = message("pending");
        let (tx, _rx) = mpsc::channel(8);
        agent
            .run_turn_with_context(context(Path::new("home")), &pending, &[], tx)
            .await
            .unwrap();
        assert_eq!(agent.sweep_pressure_at(Some(10)), 1);
        assert!(agent.contains(Path::new("home"), "pending"));
        tokio::task::yield_now().await;
        assert_eq!(*closes.lock().unwrap(), [false, true]);
        agent
            .finalize_turn_after_persist(context(Path::new("home")), &pending, "answer", true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_initialization_is_retryable_without_empty_entry_leak() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let inner = attempts.clone();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let agent = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            move |_, _, _, _| {
                let attempt = inner.fetch_add(1, Ordering::SeqCst);
                let calls = calls.clone();
                let closes = closes.clone();
                Box::pin(async move {
                    if attempt == 0 {
                        anyhow::bail!("first build fails");
                    }
                    Ok(Arc::new(RecordedAgent {
                        label: "rebuilt".into(),
                        calls,
                        closes,
                    }) as Arc<dyn AgentClient>)
                })
            },
            AgentCacheBounds::default(),
        );
        let msg = message("retry");
        let (tx, _rx) = mpsc::channel(8);
        assert!(agent
            .run_turn_with_context(context(Path::new("home")), &msg, &[], tx)
            .await
            .is_err());
        assert_eq!(agent.len(), 0);
        turn(&agent, Path::new("home"), &msg).await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn expiry_waits_for_pending_finalizer_then_closes_and_marks_boundary() {
        let home = std::env::temp_dir().join(format!(
            "hermes-cache-expiry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = crate::config_gateway::GatewayConfig {
            sessions_dir: home.join("sessions"),
            default_reset_policy: crate::config_types::SessionResetPolicy {
                mode: serde_json::json!("idle"),
                idle_minutes: serde_json::json!(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let store = Arc::new(
            crate::session_store::SessionStore::open(
                config,
                home.clone(),
                home.clone(),
                "default".into(),
                |_| Ok(false),
            )
            .unwrap(),
        );
        let entry = store
            .get_or_create_session(
                &crate::session::SessionSource::new("telegram", "C"),
                false,
                false,
                3600.0,
                |_| Ok(false),
            )
            .unwrap();
        let db = store.database_for_key(&entry.session_key).unwrap();
        db.append_message(&entry.session_id, "user", "question")
            .unwrap();
        db.append_message(&entry.session_id, "assistant", "answer")
            .unwrap();
        let closes = Arc::new(Mutex::new(Vec::new()));
        let cache = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: Arc::new(Mutex::new(Vec::new())),
                closes: closes.clone(),
            }),
            recorded_factory(
                Arc::new(Mutex::new(Vec::new())),
                closes.clone(),
                Arc::new(AtomicUsize::new(0)),
            ),
            AgentCacheBounds::default(),
        );
        let message = message(&entry.session_id);
        let context =
            crate::agent::TurnContext::from_database(Some(&db)).with_session_finalizable(true);
        let (tx, _rx) = mpsc::channel(8);
        cache
            .run_turn_with_context(context, &message, &[], tx)
            .await
            .unwrap();
        let expired = crate::session_store::ExpiredSession {
            session_key: entry.session_key.clone(),
            session_id: entry.session_id.clone(),
            home: db.profile_home().unwrap().to_owned(),
        };
        assert!(!cache
            .expire_session(store.clone(), expired.clone())
            .await
            .unwrap());
        assert!(cache.contains(&expired.home, &expired.session_id));
        cache
            .finalize_turn_after_persist(context, &message, "answer", true)
            .await
            .unwrap();
        assert!(cache
            .expire_session(store.clone(), expired.clone())
            .await
            .unwrap());
        assert!(!cache.contains(&expired.home, &expired.session_id));
        assert_eq!(*closes.lock().unwrap(), [true]);
        let row = db.get_session(&entry.session_id).unwrap().unwrap();
        assert_eq!(row["expiry_finalized"], 1);
        assert_eq!(row["end_reason"], "session_reset");
        drop(db);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn shutdown_hard_closes_all_clients_and_rejects_new_checkouts() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let cache = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: calls.clone(),
                closes: closes.clone(),
            }),
            recorded_factory(calls, closes.clone(), Arc::new(AtomicUsize::new(0))),
            AgentCacheBounds::default(),
        );
        turn(&cache, Path::new("home"), &message("one"))
            .await
            .unwrap();
        turn(&cache, Path::new("home"), &message("two"))
            .await
            .unwrap();
        assert_eq!(cache.shutdown(Duration::from_secs(1)).await, 2);
        assert_eq!(cache.len(), 0);
        assert_eq!(*closes.lock().unwrap(), [true, true]);

        let (tx, _rx) = mpsc::channel(8);
        let error = cache
            .run_turn_with_context(context(Path::new("home")), &message("three"), &[], tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("shutting down"));
    }

    #[tokio::test]
    async fn shutdown_forces_pending_turns_after_the_bounded_grace() {
        let closes = Arc::new(Mutex::new(Vec::new()));
        let cache = ConversationAgent::new(
            Arc::new(RecordedAgent {
                label: "fallback".into(),
                calls: Arc::new(Mutex::new(Vec::new())),
                closes: closes.clone(),
            }),
            recorded_factory(
                Arc::new(Mutex::new(Vec::new())),
                closes.clone(),
                Arc::new(AtomicUsize::new(0)),
            ),
            AgentCacheBounds::default(),
        );
        let msg = message("pending");
        let context = context(Path::new("home"));
        let (tx, _rx) = mpsc::channel(8);
        cache
            .run_turn_with_context(context, &msg, &[], tx)
            .await
            .unwrap();
        assert_eq!(cache.pending(Path::new("home"), "pending"), Some(1));

        assert_eq!(cache.shutdown(Duration::ZERO).await, 1);
        assert_eq!(*closes.lock().unwrap(), [true]);
        cache
            .finalize_turn_after_persist(context, &msg, "late", true)
            .await
            .unwrap();
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn failed_turn_is_released_once_by_the_post_persist_finalizer() {
        struct FailingAgent;
        #[async_trait]
        impl AgentClient for FailingAgent {
            async fn run_turn(
                &self,
                _: &Message,
                _: &[crate::session_db::HistoryMessage],
                _: mpsc::Sender<StreamEvent>,
            ) -> Result<()> {
                Err(Error::Other("model failed".into()))
            }
        }
        let cache = ConversationAgent::new(
            Arc::new(FailingAgent),
            |_, _, _, _| Box::pin(async { Ok(Arc::new(FailingAgent) as Arc<dyn AgentClient>) }),
            AgentCacheBounds::default(),
        );
        let message = message("failed");
        let context = context(Path::new("home"));
        let (tx, _rx) = mpsc::channel(8);
        assert!(cache
            .run_turn_with_context(context, &message, &[], tx)
            .await
            .is_err());
        assert_eq!(cache.pending(Path::new("home"), "failed"), Some(1));
        cache
            .finalize_turn_after_persist(context, &message, "", false)
            .await
            .unwrap();
        assert_eq!(cache.pending(Path::new("home"), "failed"), Some(0));
    }
}
