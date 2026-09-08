//! Routing-index loading and recovery from gateway/session.py. This state is
//! owned by the session store; callers must serialize access to its entries.
#![allow(dead_code)]

use crate::{session::SessionSource, session_db::SessionDb, session_entry::SessionEntry};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Overlapping calls share a transition, including force-new calls. A lease
/// that merely runs them sequentially would create a fresh session per caller.
#[derive(Default)]
pub struct SessionFlights {
    entries: Mutex<BTreeMap<String, Arc<SessionFlight>>>,
}

type FlightResult = Result<SessionEntry, Arc<anyhow::Error>>;

#[derive(Default)]
pub struct SessionFlight {
    result: Mutex<Option<FlightResult>>,
    ready: std::sync::Condvar,
}

pub enum SessionFlightTicket {
    Owner(SessionFlightOwner),
    Waiter(Arc<SessionFlight>),
}

pub struct SessionFlightOwner {
    registry: Arc<SessionFlights>,
    key: String,
    flight: Arc<SessionFlight>,
    finished: bool,
}

impl SessionFlights {
    /// Brief election under the map lock. No database work or waiting occurs
    /// here; the owning coordinator performs its transition after this returns.
    pub fn join(self: &Arc<Self>, key: &str) -> SessionFlightTicket {
        let mut entries = self.entries.lock().unwrap();
        match entries.entry(key.to_owned()) {
            std::collections::btree_map::Entry::Occupied(slot) => {
                SessionFlightTicket::Waiter(slot.get().clone())
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                let flight = Arc::new(SessionFlight::default());
                slot.insert(flight.clone());
                SessionFlightTicket::Owner(SessionFlightOwner {
                    registry: self.clone(),
                    key: key.to_owned(),
                    flight,
                    finished: false,
                })
            }
        }
    }
}

impl SessionFlight {
    /// Blocking wait for the synchronous session coordinator. Async gateway
    /// callers must enter that coordinator through spawn_blocking.
    pub fn wait(&self) -> FlightResult {
        let mut result = self.result.lock().unwrap();
        while result.is_none() {
            result = self.ready.wait(result).unwrap();
        }
        result.as_ref().unwrap().clone()
    }
}

impl SessionFlightOwner {
    pub fn complete(mut self, result: anyhow::Result<SessionEntry>) -> FlightResult {
        let result = result.map_err(Arc::new);
        self.publish(result.clone());
        result
    }

    fn publish(&mut self, result: FlightResult) {
        *self.flight.result.lock().unwrap() = Some(result);
        self.flight.ready.notify_all();
        let mut entries = self.registry.entries.lock().unwrap();
        if entries
            .get(&self.key)
            .is_some_and(|slot| Arc::ptr_eq(slot, &self.flight))
        {
            entries.remove(&self.key);
        }
        self.finished = true;
    }
}

impl Drop for SessionFlightOwner {
    fn drop(&mut self) {
        if !self.finished {
            // Unwinding/cancellation must not strand joiners. Failed flights
            // are removed too, so the next inbound call can retry creation.
            self.publish(Err(Arc::new(anyhow::anyhow!(
                "session transition owner was dropped"
            ))));
        }
    }
}

/// Inputs already resolved by the owning session store. The policy and process
/// status belong to this conversation, while timezone=None uses local time.
pub struct RecoveryRequest<'a> {
    pub key: &'a str,
    pub source: &'a SessionSource,
    pub policy: &'a crate::config_types::SessionResetPolicy,
    pub now: chrono::NaiveDateTime,
    pub timezone: Option<chrono::FixedOffset>,
    pub active_processes: bool,
    pub multiplex_profiles: bool,
    pub active_profile: &'a str,
    pub group_per_user: bool,
    pub thread_per_user: bool,
}

/// Shared recovery state is independent of the routing map lock. Clone its
/// Arc while inspecting routes, then release the map lock before querying DBs.
#[derive(Default)]
pub struct SessionRecovery {
    claimed_legacy_slack_keys: Mutex<BTreeSet<String>>,
}

impl SessionRecovery {
    /// Rebuild a route from durable history. Resolve the database for each key:
    /// multiplexed profiles may use different stores, including a legacy key.
    /// Lookup errors propagate so pruning can retain an uncertain route.
    pub fn recover_from_db<D: std::ops::Deref<Target = SessionDb>>(
        &self,
        request: &RecoveryRequest<'_>,
        database_for_key: impl Fn(&str) -> Option<D>,
    ) -> anyhow::Result<Option<SessionEntry>> {
        let Some((entry, migrated)) = self.select_recovery(request, &database_for_key, true)?
        else {
            return Ok(None);
        };
        let db = database_for_key(request.key);
        if let Some(reason) = crate::session_reset::reset_reason(
            request.policy,
            entry.updated_at.local,
            request.now,
            request.active_processes,
        )? {
            if let Some(db) = db {
                db.promote_to_session_reset(&entry.session_id, reason);
            }
            return Ok(None);
        }
        if let Some(db) = db {
            // A failed reopen must not discard otherwise recoverable history.
            if let Err(error) = db.reopen_session(&entry.session_id) {
                tracing::debug!(%error, "recovered session reopen failed");
            }
            if migrated {
                Self::record_recovered_peer(&db, request, &entry);
            }
        }
        Ok(Some(entry))
    }

    /// Find a predecessor for a live turn without reopening or resetting it.
    /// The caller evaluates policy and publishes under its own session lock.
    /// Lookup failures are treated as misses individually, allowing the legacy
    /// Slack lookup to proceed even if the primary lookup failed.
    pub fn query_recoverable<D: std::ops::Deref<Target = SessionDb>>(
        &self,
        request: &RecoveryRequest<'_>,
        database_for_key: impl Fn(&str) -> Option<D>,
    ) -> anyhow::Result<Option<SessionEntry>> {
        let Some((entry, migrated)) = self.select_recovery(request, &database_for_key, false)?
        else {
            return Ok(None);
        };
        if migrated {
            if let Some(db) = database_for_key(request.key) {
                Self::record_recovered_peer(&db, request, &entry);
            }
        }
        Ok(Some(entry))
    }

    pub(crate) fn legacy_key(request: &RecoveryRequest<'_>) -> Option<String> {
        let source = request.source;

        if source.platform == "slack" && source.scope().is_some_and(|s| !s.is_empty()) {
            let mut legacy = source.clone();
            legacy.scope_id = None;
            legacy.guild_id = None;
            let profile = request.multiplex_profiles.then(|| {
                source
                    .profile
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(request.active_profile)
            });
            Some(crate::session::build_session_key(
                &legacy,
                request.group_per_user,
                request.thread_per_user,
                profile,
            ))
        } else {
            None
        }
    }

    pub(crate) fn claim_legacy(&self, key: &str) -> bool {
        !key.is_empty()
            && self
                .claimed_legacy_slack_keys
                .lock()
                .unwrap()
                .insert(key.to_owned())
    }

    fn select_recovery<D: std::ops::Deref<Target = SessionDb>>(
        &self,
        request: &RecoveryRequest<'_>,
        database_for_key: &impl Fn(&str) -> Option<D>,
        strict_lookup: bool,
    ) -> anyhow::Result<Option<(SessionEntry, bool)>> {
        let source = request.source;
        let legacy_key = Self::legacy_key(request);
        let find = |key: &str, fallback: bool| -> rusqlite::Result<Option<Value>> {
            let Some(db) = database_for_key(key) else {
                return Ok(None);
            };
            let result = db.find_latest_gateway_session_for_peer(&crate::session_db::GatewayPeer {
                source: &source.platform,
                session_key: Some(key),
                user_id: source.user_id.as_deref(),
                chat_id: fallback.then_some(source.chat_id.as_str()),
                chat_type: fallback.then_some(source.chat_type.as_str()),
                thread_id: source.thread_id.as_deref(),
            });
            match result {
                Err(error) if !strict_lookup => {
                    tracing::debug!(%error, %key, "recovery lookup failed");
                    Ok(None)
                }
                result => result,
            }
        };
        let mut row = find(request.key, legacy_key.is_none())?;
        let mut migrated = false;
        if row.is_none() {
            if let Some(key) = legacy_key {
                // Reserve before querying. Failed or rejected migrations must
                // not let another workspace claim the ambiguous legacy key.
                let claimed = self.claim_legacy(&key);
                if claimed {
                    row = find(&key, false)?;
                    migrated = row.is_some();
                }
            }
        }
        let Some(row) = row else {
            return Ok(None);
        };
        if !recovered_scope_matches(&row, source)
            || !recovered_profile_allowed(
                request.key,
                &row,
                request.multiplex_profiles,
                request.active_profile,
            )
        {
            return Ok(None);
        }
        let entry = SessionEntry::from_recovered_row(&row, request.key, source, request.timezone)?;
        Ok(Some((entry, migrated)))
    }

    fn record_recovered_peer(db: &SessionDb, request: &RecoveryRequest<'_>, entry: &SessionEntry) {
        let source = request.source;
        let origin = source.to_dict().to_string();
        let record = crate::session_db::GatewayPeerRecord {
            peer: crate::session_db::GatewayPeer {
                source: &source.platform,
                session_key: Some(request.key),
                user_id: source.user_id.as_deref(),
                chat_id: Some(&source.chat_id),
                chat_type: Some(&source.chat_type),
                thread_id: source.thread_id.as_deref(),
            },
            display_name: source.chat_name.as_deref(),
            origin_json: Some(&origin),
            include_compression_ancestors: false,
        };
        if let Err(error) = db.record_gateway_session_peer(&entry.session_id, &record) {
            tracing::debug!(%error, "legacy session peer migration failed");
        }
    }
}

pub struct RoutingIndex {
    pub entries: BTreeMap<String, SessionEntry>,
    pub writer: Arc<RoutingWriter>,
    pub recovery: Arc<SessionRecovery>,
    generation: u64,
    sessions_dir: PathBuf,
    scope: String,
    loaded: bool,
    database_loaded: bool,
    fallback_baseline: Option<BTreeMap<String, Value>>,
}

impl RoutingIndex {
    pub fn new(sessions_dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&sessions_dir)?;
        // Resolve after creating the directory, preserving profile separation
        // while making symlink aliases identify the same routing store.
        let scope: String = std::fs::canonicalize(&sessions_dir)
            .unwrap_or_else(|_| sessions_dir.clone())
            .to_string_lossy()
            .into_owned();
        Ok(Self {
            entries: BTreeMap::new(),
            writer: Arc::new(RoutingWriter {
                sessions_dir: sessions_dir.clone(),
                scope: scope.clone(),
                state: Mutex::new(WriteState::default()),
            }),
            recovery: Arc::new(SessionRecovery::default()),
            generation: 0,
            sessions_dir,
            scope,
            loaded: false,
            database_loaded: false,
            fallback_baseline: None,
        })
    }

    /// Publish ordinary create/recovery candidates while holding the owning
    /// store lock. The winner alone may create a new durable session row.
    /// Force-new uses a separate observed-entry replacement decision.
    pub fn publish_candidate(&mut self, candidate: SessionEntry) -> (SessionEntry, bool) {
        self.publish_forced_candidate(candidate, None)
    }

    /// A force-new candidate may replace only the entry originally observed.
    /// Compare object identity, not session ID or values: another worker may
    /// have reloaded a distinct entry with the same durable session ID.
    pub fn publish_forced_candidate(
        &mut self,
        candidate: SessionEntry,
        observed: Option<&SessionEntry>,
    ) -> (SessionEntry, bool) {
        match self.entries.entry(candidate.session_key.clone()) {
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if observed.is_some_and(|observed| slot.get().same_instance(observed)) {
                    slot.insert(candidate);
                    (slot.get().clone(), true)
                } else {
                    (slot.get().clone(), false)
                }
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                (slot.insert(candidate).clone(), true)
            }
        }
    }

    pub fn recover_from_db<D: std::ops::Deref<Target = SessionDb>>(
        &self,
        request: &RecoveryRequest<'_>,
        database_for_key: impl Fn(&str) -> Option<D>,
    ) -> anyhow::Result<Option<SessionEntry>> {
        self.recovery.recover_from_db(request, database_for_key)
    }

    pub fn query_recoverable<D: std::ops::Deref<Target = SessionDb>>(
        &self,
        request: &RecoveryRequest<'_>,
        database_for_key: impl Fn(&str) -> Option<D>,
    ) -> anyhow::Result<Option<SessionEntry>> {
        self.recovery.query_recoverable(request, database_for_key)
    }

    /// Prune only after a complete scan. The owner supplies its per-key DB
    /// resolver and recovery policy; recovery errors preserve that route.
    /// Returns whether a snapshot must be persisted by the owning store.
    pub fn prune_stale<D: std::ops::Deref<Target = SessionDb>>(
        &mut self,
        database_for_key: impl Fn(&str) -> Option<D>,
        mut recover: impl FnMut(&mut Self, &str, &SessionSource) -> anyhow::Result<Option<SessionEntry>>,
    ) -> bool {
        let mut stale = Vec::new();
        let mut changed = false;
        // Recovery may replace an entry, so iterate keys independently of the
        // map borrow. Existing entries survive same-ID recovery intact.
        let keys: Vec<_> = self.entries.keys().cloned().collect();
        for key in keys {
            let Some(db) = database_for_key(&key) else {
                continue;
            };
            let entry = &self.entries[&key];
            let row = match db.get_session(&entry.session_id) {
                Ok(row) => row,
                Err(error) => {
                    // Match startup's all-or-nothing deletion pass. Earlier
                    // repoints remain in memory, but no snapshot is requested.
                    tracing::warn!(%error, "stale route scan aborted");
                    return false;
                }
            };
            if row.is_none_or(|row| row["end_reason"].is_null()) {
                continue;
            }
            let old_id = entry.session_id.clone();
            let origin = entry.origin.clone();
            let recovered = match origin {
                Some(source) => match recover(self, &key, &source) {
                    Ok(entry) => entry,
                    Err(error) => {
                        tracing::debug!(%error, %key, "keeping route after recovery failure");
                        continue;
                    }
                },
                None => None,
            };
            match recovered {
                Some(entry) if entry.session_id != old_id => {
                    self.entries.insert(key, entry);
                    changed = true;
                }
                Some(_) => {}
                None => stale.push(key),
            }
        }
        changed |= !stale.is_empty();
        for key in stale {
            self.entries.remove(&key);
        }
        changed
    }

    fn next_revision(&mut self) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("routing revision exhausted");
        self.generation
    }

    /// Capture under the owning store lock, then persist outside that lock.
    pub fn snapshot(&mut self, database: Option<&SessionDb>) -> RoutingSnapshot {
        self.ensure_loaded(database);
        self.snapshot_loaded()
    }

    /// Capture already initialized state without any filesystem or DB access.
    pub(crate) fn snapshot_loaded(&mut self) -> RoutingSnapshot {
        assert!(self.loaded, "routing index must be initialized");
        RoutingSnapshot {
            data: self.serialized(),
            revision: self.next_revision(),
        }
    }

    pub fn save(&mut self, database: Option<&SessionDb>, mirror: bool) -> anyhow::Result<()> {
        let snapshot = self.snapshot(database);
        self.writer.persist(snapshot, database, mirror)
    }

    /// Metadata-only fast path. Structural mapping changes use save(). A
    /// candidate can be made durable before the caller publishes it in memory.
    pub fn save_entry(
        &mut self,
        key: &str,
        candidate: Option<Value>,
        database: Option<&SessionDb>,
        mirror: bool,
    ) -> anyhow::Result<()> {
        // Python's metadata callers ensure the store is loaded before invoking
        // _save_entry. Enforce that boundary here too: candidate fallback can
        // replace the whole index and must include recovered database-only rows.
        self.ensure_loaded(database);
        let Some(entry) = self.entries.get(key) else {
            return Ok(());
        };
        let data = candidate.clone().unwrap_or_else(|| entry.to_dict());
        let revision = self.next_revision();
        if self.writer.persist_entry(key, &data, revision, database)? {
            return Ok(());
        }
        if let Some(candidate) = candidate {
            let mut data = self.serialized();
            data.insert(key.to_owned(), candidate);
            self.writer
                .persist(RoutingSnapshot { data, revision }, database, mirror)
        } else {
            self.save(database, mirror)
        }
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Database entries take precedence. Invalid rows do not prevent other
    /// entries from loading, and a bad database row can fall back to the mirror.
    pub fn ensure_loaded(&mut self, database: Option<&SessionDb>) {
        if self.loaded {
            self.reconcile(database);
            return;
        }
        if let Some(database) = database {
            match database.load_gateway_routing_entries(&self.scope) {
                Ok(rows) => {
                    for (key, raw) in rows {
                        if let Some(entry) = parse_row(&raw) {
                            self.entries.insert(key, entry);
                        }
                    }
                    self.database_loaded = true;
                }
                Err(error) => tracing::warn!(%error, "routing database load failed"),
            }
        }
        let path = self.sessions_dir.join("sessions.json");
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Object(rows)) => {
                    for (key, data) in rows {
                        if key.starts_with('_') || self.entries.contains_key(&key) {
                            continue;
                        }
                        if let Some(entry) = parse_entry(&data) {
                            self.entries.insert(key, entry);
                        }
                    }
                }
                _ => tracing::warn!("invalid legacy routing index"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => tracing::warn!(%error, "legacy routing index load failed"),
        }
        self.loaded = true;
        if !self.database_loaded {
            self.fallback_baseline = Some(self.serialized());
        }
    }

    pub fn serialized(&self) -> BTreeMap<String, Value> {
        self.entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.to_dict()))
            .collect()
    }

    pub(crate) fn recovery_scope(&self) -> Option<String> {
        (!self.database_loaded && self.fallback_baseline.is_some()).then(|| self.scope.clone())
    }

    pub(crate) fn capture_entry(&mut self, key: &str) -> Option<(Value, u64)> {
        assert!(self.loaded, "routing index must be initialized");
        let data = self.entries.get(key)?.to_dict();
        Some((data, self.next_revision()))
    }

    /// A recovered primary wins only over untouched fallback entries. Local
    /// edits and deletions made during the outage must survive reconciliation.
    fn reconcile(&mut self, database: Option<&SessionDb>) {
        if self.database_loaded || self.fallback_baseline.is_none() {
            return;
        }
        let Some(database) = database else {
            return;
        };
        match database.load_gateway_routing_entries(&self.scope) {
            Ok(rows) => self.merge_recovered(rows),
            Err(error) => tracing::warn!(%error, "recovered routing database load failed"),
        }
    }

    pub(crate) fn merge_recovered(&mut self, rows: BTreeMap<String, String>) {
        let Some(baseline) = self.fallback_baseline.as_ref() else {
            return;
        };
        for (key, raw) in rows {
            let Some(durable) = parse_row(&raw) else {
                continue;
            };
            if !baseline.contains_key(&key) {
                self.entries.entry(key).or_insert(durable);
            } else if let Some(current) = self.entries.get(&key) {
                if crate::python_value::python_equal(&current.to_dict(), &baseline[&key]) {
                    self.entries.insert(key, durable);
                }
            }
        }
        self.database_loaded = true;
        self.fallback_baseline = None;
    }
}

/// Immutable captures and the writer share one revision sequence. Keeping the
/// durable write lock separate allows callers to release the entry lock for I/O.
pub struct RoutingSnapshot {
    data: BTreeMap<String, Value>,
    revision: u64,
}

#[derive(Default)]
struct WriteState {
    persisted: u64,
    fast: BTreeMap<String, (u64, Value)>,
}

pub struct RoutingWriter {
    sessions_dir: PathBuf,
    scope: String,
    state: Mutex<WriteState>,
}

impl RoutingWriter {
    pub fn persist(
        &self,
        mut snapshot: RoutingSnapshot,
        database: Option<&SessionDb>,
        mirror: bool,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        if snapshot.revision <= state.persisted {
            return Ok(());
        }
        for (key, (revision, entry)) in &state.fast {
            if *revision > snapshot.revision {
                snapshot.data.insert(key.clone(), entry.clone());
            }
        }
        let rows = snapshot
            .data
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string()))
            .collect();
        let db_saved = database.is_some_and(|db| {
            match db.replace_gateway_routing_entries(&self.scope, &rows) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(%error, "routing database save failed");
                    false
                }
            }
        });
        if mirror || !db_saved {
            if let Err(error) = self.save_mirror(snapshot.data) {
                if !db_saved {
                    return Err(error);
                }
                tracing::warn!(%error, "legacy routing mirror failed after database commit");
            }
        }
        // Failed primary and fallback writes do not consume the revision.
        state.persisted = snapshot.revision;
        state
            .fast
            .retain(|_, (revision, _)| *revision > snapshot.revision);
        Ok(())
    }

    /// True means saved or superseded; false requests a full-save fallback.
    pub(crate) fn persist_entry(
        &self,
        key: &str,
        entry: &Value,
        revision: u64,
        database: Option<&SessionDb>,
    ) -> anyhow::Result<bool> {
        let Some(database) = database else {
            return Ok(false);
        };
        let mut state = self.state.lock().unwrap();
        if state.persisted >= revision || state.fast.get(key).is_some_and(|(r, _)| *r >= revision) {
            return Ok(true);
        }
        match database.save_gateway_routing_entry(&self.scope, key, &entry.to_string()) {
            Ok(()) => {
                state.fast.insert(key.to_owned(), (revision, entry.clone()));
                Ok(true)
            }
            Err(error) => {
                tracing::warn!(%error, "routing upsert failed; falling back to full save");
                Ok(false)
            }
        }
    }

    fn save_mirror(&self, entries: BTreeMap<String, Value>) -> anyhow::Result<()> {
        let mut data = serde_json::Map::new();
        data.insert("_README".into(), Value::String(LEGACY_NOTE.into()));
        data.extend(entries);
        crate::atomic_file::write(
            &self.sessions_dir.join("sessions.json"),
            &serde_json::to_vec_pretty(&data)?,
        )?;
        Ok(())
    }
}

const LEGACY_NOTE: &str = "LEGACY MIRROR of the gateway routing index (the primary copy lives in the gateway_routing table in ~/.hermes/state.db). Maps messaging session keys (agent:main:<platform>:...) to active session IDs. This is NOT the session list. ALL sessions (CLI, TUI, and gateway) live in ~/.hermes/state.db and are shown by `hermes sessions list` and `/sessions`. Disable this file with `gateway.write_sessions_json: false` in config.yaml.";

fn parse_entry(data: &Value) -> Option<SessionEntry> {
    if !data.is_object() {
        return None;
    }
    match SessionEntry::from_dict(data) {
        Ok(entry) => Some(entry),
        Err(error) => {
            tracing::warn!(%error, "skipping invalid routing entry");
            None
        }
    }
}

/// Profile isolation from SessionStore's recovery guard. Legacy keyless rows
/// remain adoptable; multiplexed gateways use the requested key's namespace.
pub fn recovered_profile_allowed(
    requested_key: &str,
    row: &Value,
    multiplex: bool,
    active_profile: &str,
) -> bool {
    let value = &row["session_key"];
    if !crate::python_value::truthy(value) {
        return true;
    }
    let recovered_key = match value {
        Value::String(s) => s.clone(),
        other => crate::python_value::python_repr(other),
    };
    if recovered_key == requested_key {
        return true;
    }
    let Some(recovered_profile) = profile_from_key(&recovered_key) else {
        return true;
    };
    if multiplex {
        profile_from_key(requested_key).is_none_or(|requested| requested == recovered_profile)
    } else {
        recovered_profile == active_profile
    }
}

pub(crate) fn profile_from_key(key: &str) -> Option<&str> {
    let mut parts = key.split(':');
    if parts.next()? != "agent" {
        return None;
    }
    match parts.next()? {
        "" | "main" => Some("default"),
        name => Some(name),
    }
}

/// A scoped Slack group must prove its workspace through durable origin data.
/// An explicit null scope does not fall back to the deprecated guild_id alias.
pub fn recovered_scope_matches(row: &Value, source: &SessionSource) -> bool {
    let Some(scope) = source.scope().filter(|s| !s.is_empty()) else {
        return true;
    };
    if source.platform != "slack" || source.chat_type == "dm" {
        return true;
    }
    let Some(origin) = row["origin_json"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(Value::is_object)
    else {
        return false;
    };
    origin
        .get("scope_id")
        .or_else(|| origin.get("guild_id"))
        .and_then(Value::as_str)
        == Some(scope)
}

fn parse_row(raw: &str) -> Option<SessionEntry> {
    match serde_json::from_str::<Value>(raw) {
        Ok(data) => parse_entry(&data),
        Err(_) => {
            tracing::warn!("skipping malformed routing JSON");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn directory(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hermes-routing-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    fn entry(id: &str) -> Value {
        json!({"session_key":"agent:main:slack:group:C", "session_id":id,
            "created_at":"2026-09-06", "updated_at":"2026-09-06"})
    }

    #[test]
    fn recovery_migrates_scoped_slack_once_and_persists_reopen() {
        let dir = directory("recover_legacy");
        let path = dir.join("state.db");
        let db = SessionDb::open(path.clone()).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("INSERT INTO sessions (id,source,session_key,started_at,ended_at,end_reason,origin_json) VALUES ('old','slack','agent:main:slack:group:C',0,1,'agent_close','{\"scope_id\":\"T\"}')", []).unwrap();
        let index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let mut source = SessionSource::new("slack", "C");
        source.chat_type = "group".into();
        source.scope_id = Some("T".into());
        let policy = crate::config_types::SessionResetPolicy::default();
        let request = RecoveryRequest {
            key: "agent:main:slack:group:T:C",
            source: &source,
            policy: &policy,
            now: chrono::DateTime::from_timestamp(100, 0)
                .unwrap()
                .naive_utc(),
            timezone: chrono::FixedOffset::east_opt(0),
            active_processes: false,
            multiplex_profiles: false,
            active_profile: "default",
            group_per_user: false,
            thread_per_user: false,
        };
        let recovered = index
            .recover_from_db(&request, |_| Some(&db))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.session_id, "old");
        let row = db.get_session("old").unwrap().unwrap();
        assert!(row["ended_at"].is_null());
        assert_eq!(row["session_key"], request.key);
        assert_eq!(
            serde_json::from_str::<Value>(row["origin_json"].as_str().unwrap()).unwrap()
                ["scope_id"],
            "T"
        );
        // Even if a legacy row reappears, the store cannot migrate it twice.
        conn.execute(
            "UPDATE sessions SET session_key='agent:main:slack:group:C'",
            [],
        )
        .unwrap();
        assert!(index
            .recover_from_db(&request, |_| Some(&db))
            .unwrap()
            .is_none());
        drop(conn);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_respects_reset_boundary_and_propagates_lookup_failure() {
        let dir = directory("recover_reset");
        let path = dir.join("state.db");
        let db = SessionDb::open(path.clone()).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("INSERT INTO sessions (id,source,session_key,started_at,ended_at,end_reason) VALUES ('old','telegram','key',0,1,'agent_close')", []).unwrap();
        let index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let source = SessionSource::new("telegram", "C");
        let policy = crate::config_types::SessionResetPolicy {
            mode: json!("idle"),
            idle_minutes: json!(1),
            ..Default::default()
        };
        let mut request = RecoveryRequest {
            key: "key",
            source: &source,
            policy: &policy,
            now: chrono::DateTime::from_timestamp(100, 0)
                .unwrap()
                .naive_utc(),
            timezone: chrono::FixedOffset::east_opt(0),
            active_processes: true,
            multiplex_profiles: false,
            active_profile: "default",
            group_per_user: true,
            thread_per_user: false,
        };
        assert!(index
            .recover_from_db(&request, |_| Some(&db))
            .unwrap()
            .is_some());
        assert!(db.get_session("old").unwrap().unwrap()["ended_at"].is_null());
        request.active_processes = false;
        assert!(index
            .recover_from_db(&request, |_| Some(&db))
            .unwrap()
            .is_none());
        assert_eq!(
            db.get_session("old").unwrap().unwrap()["end_reason"],
            "idle"
        );
        let generation: i64 = conn.query_row("SELECT generation FROM conversation_generations WHERE source='telegram' AND session_key='key'", [], |r| r.get(0)).unwrap();
        assert_eq!(generation, 1);
        conn.execute("DROP TABLE sessions", []).unwrap();
        assert!(index.recover_from_db(&request, |_| Some(&db)).is_err());
        assert!(index
            .recover_from_db(&request, |_| None::<&SessionDb>)
            .unwrap()
            .is_none());
        drop(conn);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_pruning_preserves_same_id_state_and_isolates_profile_stores() {
        let dir = directory("prune");
        let db = SessionDb::open(dir.join("state.db")).unwrap();
        let work = SessionDb::open(dir.join("profiles/work/state.db")).unwrap();
        let conn = rusqlite::Connection::open(dir.join("state.db")).unwrap();
        conn.execute("INSERT INTO sessions (id,source,started_at,end_reason) VALUES ('same','telegram',0,'agent_close'),('parent','telegram',0,'compression'),('dead','telegram',0,'session_reset'),('uncertain','telegram',0,'agent_close'),('shared','telegram',0,'session_reset')", []).unwrap();
        work.ensure_session("shared", "telegram", None, None, None)
            .unwrap();
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        for (key, id) in [
            ("same", "same"),
            ("rotated", "parent"),
            ("dead", "dead"),
            ("uncertain", "uncertain"),
            ("work", "shared"),
            ("legacy", "missing"),
        ] {
            let mut value = entry(id);
            value["session_key"] = json!(key);
            value["metadata"] = json!({"queued":"keep"});
            let mut parsed = SessionEntry::from_dict(&value).unwrap();
            parsed.origin = Some(SessionSource::new("telegram", "C"));
            index.entries.insert(key.into(), parsed);
        }
        let original = index.entries["same"].to_dict();
        let changed = index.prune_stale(
            |key| Some(if key == "work" { &work } else { &db }),
            |_, key, _| match key {
                "same" => Ok(Some(SessionEntry::from_dict(&entry("same")).unwrap())),
                "rotated" => Ok(Some(SessionEntry::from_dict(&entry("child")).unwrap())),
                "dead" => Ok(None),
                "uncertain" => anyhow::bail!("transient recovery error"),
                other => panic!("live or absent row must not recover: {other}"),
            },
        );
        assert!(changed);
        assert_eq!(index.entries["same"].to_dict(), original);
        assert_eq!(index.entries["rotated"].session_id, "child");
        assert!(!index.entries.contains_key("dead"));
        assert!(index.entries.contains_key("uncertain"));
        assert!(index.entries.contains_key("work"));
        assert!(index.entries.contains_key("legacy"));
        drop(conn);
        drop(db);
        drop(work);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_scan_failure_keeps_pending_deletions_and_does_not_request_save() {
        let dir = directory("prune_error");
        let path = dir.join("state.db");
        let db = SessionDb::open(path.clone()).unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute("INSERT INTO sessions (id,source,started_at,end_reason) VALUES ('old','telegram',0,'compression')", []).unwrap();
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        for key in ["a_delete", "b_repoint", "c_error"] {
            let mut parsed = SessionEntry::from_dict(&entry("old")).unwrap();
            parsed.origin = Some(SessionSource::new("telegram", "C"));
            index.entries.insert(key.into(), parsed);
        }
        let changed = index.prune_stale(
            |_| Some(&db),
            |_, key, _| {
                if key == "a_delete" {
                    return Ok(None);
                }
                assert_eq!(key, "b_repoint");
                conn.execute("DROP TABLE sessions", []).unwrap();
                Ok(Some(SessionEntry::from_dict(&entry("new")).unwrap()))
            },
        );
        assert!(!changed);
        assert!(index.entries.contains_key("a_delete"));
        assert_eq!(index.entries["b_repoint"].session_id, "new");
        assert!(index.entries.contains_key("c_error"));
        drop(conn);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn query_recovery_keeps_expired_predecessor_and_migrates_before_policy() {
        for query_only in [false, true] {
            let dir = directory("query_predecessor");
            let path = dir.join("state.db");
            let db = SessionDb::open(path.clone()).unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            let mut source = SessionSource::new("slack", "C");
            source.chat_type = "group".into();
            let legacy = crate::session::build_session_key(&source, false, false, None);
            source.scope_id = Some("T".into());
            let key = crate::session::build_session_key(&source, false, false, None);
            conn.execute("INSERT INTO sessions (id,source,session_key,started_at,ended_at,end_reason,origin_json) VALUES ('old','slack',?1,0,1,'agent_close','{\"scope_id\":\"T\"}')", [&legacy]).unwrap();
            let policy = crate::config_types::SessionResetPolicy {
                mode: json!("idle"),
                idle_minutes: json!(1),
                ..Default::default()
            };
            let request = RecoveryRequest {
                key: &key,
                source: &source,
                policy: &policy,
                now: chrono::DateTime::from_timestamp(100, 0)
                    .unwrap()
                    .naive_utc(),
                timezone: chrono::FixedOffset::east_opt(0),
                active_processes: false,
                multiplex_profiles: false,
                active_profile: "default",
                group_per_user: false,
                thread_per_user: false,
            };
            let index = RoutingIndex::new(dir.join("sessions")).unwrap();
            let entry = if query_only {
                index.query_recoverable(&request, |_| Some(&db))
            } else {
                index.recover_from_db(&request, |_| Some(&db))
            }
            .unwrap();
            let row = db.get_session("old").unwrap().unwrap();
            if query_only {
                assert_eq!(entry.unwrap().session_id, "old");
                assert_eq!(row["ended_at"], 1.0);
                assert_eq!(row["end_reason"], "agent_close");
                assert_eq!(row["session_key"], key);
            } else {
                assert!(entry.is_none());
                assert_eq!(row["end_reason"], "idle");
                assert_eq!(row["session_key"], legacy);
            }
            assert!(index.entries.is_empty(), "owner must publish the candidate");
            drop(conn);
            drop(db);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn query_recovery_tries_legacy_after_primary_database_failure() {
        let dir = directory("query_error_fallback");
        let primary = SessionDb::open(dir.join("primary/state.db")).unwrap();
        let legacy = SessionDb::open(dir.join("legacy/state.db")).unwrap();
        let primary_conn = rusqlite::Connection::open(dir.join("primary/state.db")).unwrap();
        primary_conn.execute("DROP TABLE sessions", []).unwrap();
        let conn = rusqlite::Connection::open(dir.join("legacy/state.db")).unwrap();
        let mut source = SessionSource::new("slack", "C");
        let old_key = crate::session::build_session_key(&source, true, false, None);
        source.scope_id = Some("T".into());
        let key = crate::session::build_session_key(&source, true, false, None);
        conn.execute("INSERT INTO sessions (id,source,session_key,started_at,ended_at,end_reason) VALUES ('old','slack',?1,0,1,'agent_close')", [&old_key]).unwrap();
        let policy = crate::config_types::SessionResetPolicy::default();
        let request = RecoveryRequest {
            key: &key,
            source: &source,
            policy: &policy,
            now: chrono::DateTime::from_timestamp(100, 0)
                .unwrap()
                .naive_utc(),
            timezone: chrono::FixedOffset::east_opt(0),
            active_processes: false,
            multiplex_profiles: false,
            active_profile: "default",
            group_per_user: true,
            thread_per_user: false,
        };
        let resolve = |requested: &str| Some(if requested == key { &primary } else { &legacy });
        let index = RoutingIndex::new(dir.join("sessions")).unwrap();
        assert!(index.recover_from_db(&request, resolve).is_err());
        let recovered = index.query_recoverable(&request, resolve).unwrap().unwrap();
        assert_eq!(recovered.session_id, "old");
        assert_eq!(legacy.get_session("old").unwrap().unwrap()["ended_at"], 1.0);
        drop(conn);
        drop(primary_conn);
        drop(primary);
        drop(legacy);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn blocked_legacy_lookup_leaves_route_lock_free_and_claim_is_exclusive() {
        let dir = directory("recovery_unlocked");
        let index = Arc::new(Mutex::new(RoutingIndex::new(dir.join("sessions")).unwrap()));
        let recovery = index.lock().unwrap().recovery.clone();
        let worker_recovery = recovery.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut source = SessionSource::new("slack", "C");
            let legacy = crate::session::build_session_key(&source, true, false, None);
            source.scope_id = Some("T1".into());
            let key = crate::session::build_session_key(&source, true, false, None);
            let policy = crate::config_types::SessionResetPolicy::default();
            let request = RecoveryRequest {
                key: &key,
                source: &source,
                policy: &policy,
                now: chrono::DateTime::from_timestamp(100, 0)
                    .unwrap()
                    .naive_utc(),
                timezone: chrono::FixedOffset::east_opt(0),
                active_processes: false,
                multiplex_profiles: false,
                active_profile: "default",
                group_per_user: true,
                thread_per_user: false,
            };
            worker_recovery
                .query_recoverable(&request, |requested| {
                    if requested == legacy {
                        entered_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                    }
                    None::<&SessionDb>
                })
                .unwrap()
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // Observe both locks while the worker is inside its DB resolver.
        assert!(index.try_lock().is_ok());
        assert!(recovery.claimed_legacy_slack_keys.try_lock().is_ok());
        let mut source = SessionSource::new("slack", "C");
        source.scope_id = Some("T2".into());
        let key = crate::session::build_session_key(&source, true, false, None);
        let policy = crate::config_types::SessionResetPolicy::default();
        let request = RecoveryRequest {
            key: &key,
            source: &source,
            policy: &policy,
            now: chrono::DateTime::from_timestamp(100, 0)
                .unwrap()
                .naive_utc(),
            timezone: chrono::FixedOffset::east_opt(0),
            active_processes: false,
            multiplex_profiles: false,
            active_profile: "default",
            group_per_user: true,
            thread_per_user: false,
        };
        assert!(recovery
            .query_recoverable(&request, |requested| {
                assert_eq!(requested, key, "legacy key was already reserved by T1");
                None::<&SessionDb>
            })
            .unwrap()
            .is_none());
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_candidates_publish_one_identity_and_preserve_reset_context() {
        let dir = directory("candidate_publication");
        let index = Arc::new(Mutex::new(RoutingIndex::new(dir.join("sessions")).unwrap()));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let index = index.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut source = SessionSource::new("telegram", "C");
                    source.chat_name = Some("chat".into());
                    let now = chrono::NaiveDateTime::parse_from_str(
                        "2026-09-06 12:34:56.123456",
                        "%Y-%m-%d %H:%M:%S%.f",
                    )
                    .unwrap();
                    let context = crate::session_entry::CreationContext {
                        is_fresh_reset: false,
                        was_auto_reset: true,
                        auto_reset_reason: Some("idle".into()),
                        reset_had_activity: true,
                        prev_session_id: Some("parent".into()),
                    };
                    let candidate =
                        SessionEntry::new_candidate("key", &source, now, &context).unwrap();
                    barrier.wait();
                    let published = index.lock().unwrap().publish_candidate(candidate);
                    published
                })
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|(_, won)| *won).count(), 1);
        let id = &results[0].0.session_id;
        assert!(results.iter().all(|(entry, _)| &entry.session_id == id));
        assert!(id.starts_with("20260906_123456_"));
        assert_eq!(id.len(), 24);
        let value = results[0].0.to_dict();
        assert_eq!(value["updated_at"], "2026-09-06T12:34:56.123456");
        assert_eq!(value["created_at"], value["updated_at"]);
        assert_eq!(value["prev_session_id"], "parent");
        assert_eq!(value["auto_reset_reason"], "idle");
        assert_eq!(value["reset_had_activity"], true);
        assert_eq!(value["display_name"], "chat");
        assert_eq!(value["input_tokens"], 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn force_new_checks_observed_identity_without_serializing_it() {
        let dir = directory("force_publication");
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let original = SessionEntry::from_dict(&entry("old")).unwrap();
        let key = original.session_key.clone();
        index.publish_candidate(original.clone());
        // Normal metadata mutation must not invalidate an observed entry.
        index
            .entries
            .get_mut(&key)
            .unwrap()
            .fields
            .insert("input_tokens".into(), json!(42));
        let candidate = SessionEntry::from_dict(&entry("first")).unwrap();
        let (published, won) = index.publish_forced_candidate(candidate, Some(&original));
        assert!(won);
        assert_eq!(published.session_id, "first");
        let (published, won) = index.publish_forced_candidate(
            SessionEntry::from_dict(&entry("second")).unwrap(),
            Some(&original),
        );
        assert!(!won);
        assert_eq!(published.session_id, "first");
        // Reloading equal persisted data creates a different object generation.
        let equal_reload = SessionEntry::from_dict(&published.to_dict()).unwrap();
        assert!(!equal_reload.same_instance(&published));
        assert_eq!(equal_reload.to_dict(), published.to_dict());
        index.entries.insert(key.clone(), equal_reload);
        let (_, won) = index.publish_forced_candidate(
            SessionEntry::from_dict(&entry("third")).unwrap(),
            Some(&published),
        );
        assert!(!won);
        index.entries.remove(&key);
        let (published, won) = index.publish_forced_candidate(
            SessionEntry::from_dict(&entry("fourth")).unwrap(),
            Some(&original),
        );
        assert!(won);
        assert_eq!(published.session_id, "fourth");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn session_flights_share_results_without_blocking_other_keys() {
        let flights = Arc::new(SessionFlights::default());
        let SessionFlightTicket::Owner(owner) = flights.join("key") else {
            panic!("first caller must own");
        };
        let SessionFlightTicket::Waiter(waiter) = flights.join("key") else {
            panic!("overlapping caller must join");
        };
        let SessionFlightTicket::Owner(other) = flights.join("other") else {
            panic!("unrelated key must progress");
        };
        let other_entry = SessionEntry::from_dict(&entry("other")).unwrap();
        other.complete(Ok(other_entry)).unwrap();
        let worker = std::thread::spawn(move || waiter.wait().unwrap());
        let result = owner
            .complete(Ok(SessionEntry::from_dict(&entry("winner")).unwrap()))
            .unwrap();
        let joined = worker.join().unwrap();
        assert!(result.same_instance(&joined));
        assert_eq!(joined.session_id, "winner");
        assert!(flights.entries.lock().unwrap().is_empty());
        assert!(matches!(flights.join("key"), SessionFlightTicket::Owner(_)));
    }

    #[test]
    fn failed_or_abandoned_session_flights_wake_waiters_and_allow_retry() {
        let flights = Arc::new(SessionFlights::default());
        let SessionFlightTicket::Owner(owner) = flights.join("key") else {
            unreachable!()
        };
        let SessionFlightTicket::Waiter(waiter) = flights.join("key") else {
            unreachable!()
        };
        let error = owner
            .complete(Err(anyhow::anyhow!("write failed")))
            .unwrap_err();
        assert!(Arc::ptr_eq(&error, &waiter.wait().unwrap_err()));
        let SessionFlightTicket::Owner(owner) = flights.join("key") else {
            unreachable!()
        };
        let SessionFlightTicket::Waiter(waiter) = flights.join("key") else {
            unreachable!()
        };
        let worker = std::thread::spawn(move || waiter.wait().unwrap_err());
        drop(owner);
        assert!(worker
            .join()
            .unwrap()
            .to_string()
            .contains("owner was dropped"));
        assert!(flights.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn profile_handles_recover_and_persist_stale_routes_in_the_fixed_index_store() {
        use crate::session_db::{GatewayPeer, SessionCreate};
        let dir = directory("profile_recovery_integration");
        let work_home = dir.join("profiles/work");
        std::fs::create_dir_all(&work_home).unwrap();
        let databases =
            crate::session_db_recovery::SessionDatabases::new(dir.clone(), dir.clone(), true);
        let key = "agent:work:telegram:dm:C";
        let routing = databases.routing().unwrap();
        let work = databases.for_key(key, &dir).unwrap();
        let mut source = SessionSource::new("telegram", "C");
        source.profile = Some("work".into());
        work.create_session(
            "parent",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "telegram",
                    session_key: Some(key),
                    chat_id: Some("C"),
                    chat_type: Some("dm"),
                    ..Default::default()
                },
                profile_name: Some("work"),
                ..Default::default()
            },
        )
        .unwrap();
        work.end_session("parent", "compression").unwrap();
        work.create_session(
            "child",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "telegram",
                    ..Default::default()
                },
                parent_session_id: Some("parent"),
                ..Default::default()
            },
        )
        .unwrap();
        // A conflicting root copy must not decide the named profile's fate.
        routing
            .ensure_session("parent", "telegram", Some(key), Some("C"), Some("dm"))
            .unwrap();
        routing.end_session("parent", "session_reset").unwrap();
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        index.ensure_loaded(Some(&routing));
        let mut old = SessionEntry::from_dict(&entry("parent")).unwrap();
        old.session_key = key.into();
        old.origin = Some(source);
        index.entries.insert(key.into(), old);
        let policy = crate::config_types::SessionResetPolicy::default();
        let changed = index.prune_stale(
            |key| databases.for_key(key, &dir),
            |index, key, source| {
                let request = RecoveryRequest {
                    key,
                    source,
                    policy: &policy,
                    now: chrono::Local::now().naive_local(),
                    timezone: None,
                    active_processes: false,
                    multiplex_profiles: true,
                    active_profile: "default",
                    group_per_user: true,
                    thread_per_user: false,
                };
                index.recover_from_db(&request, |key| databases.for_key(key, &dir))
            },
        );
        assert!(changed);
        assert_eq!(index.entries[key].session_id, "child");
        index.save(Some(&routing), false).unwrap();
        let rows = routing.load_gateway_routing_entries(&index.scope).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&rows[key]).unwrap()["session_id"],
            "child"
        );
        assert!(work
            .load_gateway_routing_entries(&index.scope)
            .unwrap()
            .is_empty());
        assert!(routing.get_session("child").unwrap().is_none());
        assert_eq!(
            routing.get_session("parent").unwrap().unwrap()["end_reason"],
            "session_reset"
        );
        drop(index);
        drop(work);
        drop(routing);
        drop(databases);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovered_profile_and_workspace_gates_match_python() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../tools/session-recovery-isolation-goldens.json"
        ))
        .unwrap();
        for case in cases["profiles"].as_array().unwrap() {
            assert_eq!(
                recovered_profile_allowed(
                    case["requested"].as_str().unwrap(),
                    &json!({"session_key":case["recovered"]}),
                    case["multiplex"].as_bool().unwrap(),
                    case["active"].as_str().unwrap()
                ),
                case["result"].as_bool().unwrap(),
                "{case}"
            );
        }
        for case in cases["scopes"].as_array().unwrap() {
            assert_eq!(
                recovered_scope_matches(
                    &json!({"origin_json":case["origin"]}),
                    &SessionSource::from_dict(&case["source"])
                ),
                case["result"].as_bool().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn candidate_fallback_recovers_database_only_routes_before_replacement() {
        let dir = directory("candidate-recovery");
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        std::fs::write(
            dir.join("sessions/sessions.json"),
            json!({"key":entry("same-id")}).to_string(),
        )
        .unwrap();
        index.ensure_loaded(None);
        let db_path = dir.join("state.db");
        let db = SessionDb::open(db_path.clone()).unwrap();
        db.save_gateway_routing_entry(index.scope(), "key", &entry("same-id").to_string())
            .unwrap();
        db.save_gateway_routing_entry(
            index.scope(),
            "database-only",
            &entry("retained").to_string(),
        )
        .unwrap();
        let connection = rusqlite::Connection::open(db_path).unwrap();
        // Reject the upsert's UPDATE, but allow the fallback DELETE+INSERT.
        connection.execute_batch("CREATE TRIGGER reject_upsert BEFORE UPDATE ON gateway_routing BEGIN SELECT RAISE(ABORT, 'injected upsert failure'); END;").unwrap();
        let mut candidate = index.entries["key"].to_dict();
        candidate["resume_pending"] = json!(true);
        index
            .save_entry("key", Some(candidate), Some(&db), false)
            .unwrap();
        let rows = db.load_gateway_routing_entries(index.scope()).unwrap();
        assert!(rows.contains_key("database-only"));
        assert_eq!(
            serde_json::from_str::<Value>(&rows["key"]).unwrap()["resume_pending"],
            true
        );
        assert_eq!(index.entries["key"].fields["resume_pending"], false);
        assert!(index.database_loaded);
        drop(connection);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cold_candidate_save_loads_its_route_before_saving() {
        let dir = directory("cold-candidate");
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let db = SessionDb::open(dir.join("state.db")).unwrap();
        db.save_gateway_routing_entry(index.scope(), "key", &entry("same-id").to_string())
            .unwrap();
        let mut candidate = SessionEntry::from_dict(&entry("same-id"))
            .unwrap()
            .to_dict();
        candidate["resume_pending"] = json!(true);
        index
            .save_entry("key", Some(candidate), Some(&db), false)
            .unwrap();
        let rows = db.load_gateway_routing_entries(index.scope()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&rows["key"]).unwrap()["resume_pending"],
            true
        );
        assert!(index.database_loaded);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writer_decisions_match_python_with_real_database_and_files() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-routing-writer-goldens.json"
        ))
        .unwrap();
        for (number, case) in cases.iter().enumerate() {
            let dir = directory(&format!("writer-oracle-{number}"));
            let index = RoutingIndex::new(dir.join("sessions")).unwrap();
            let db_path = dir.join("state.db");
            let db = SessionDb::open(db_path.clone()).unwrap();
            if case["db_ok"] == false {
                rusqlite::Connection::open(&db_path).unwrap().execute_batch(
                    "CREATE TRIGGER reject_write BEFORE INSERT ON gateway_routing BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
                ).unwrap();
            }
            let mirror_path = dir.join("sessions/sessions.json");
            if case["mirror_ok"] == false {
                std::fs::create_dir(&mirror_path).unwrap();
            }
            {
                let mut state = index.writer.state.lock().unwrap();
                state.persisted = case["persisted"].as_u64().unwrap();
                let fast = case["fast_revision"].as_u64().unwrap();
                if fast != 0 {
                    state
                        .fast
                        .insert("key".into(), (fast, case["fast_data"].clone()));
                }
            }
            let snapshot = RoutingSnapshot {
                revision: case["revision"].as_u64().unwrap(),
                data: BTreeMap::from([("key".into(), case["data"].clone())]),
            };
            let result = index.writer.persist(
                snapshot,
                Some(&db),
                case["mirror_enabled"].as_bool().unwrap(),
            );
            assert_eq!(
                result.is_err(),
                case["error"].as_bool().unwrap(),
                "case {number}"
            );
            let state = index.writer.state.lock().unwrap();
            assert_eq!(
                state.persisted,
                case["expected_persisted"].as_u64().unwrap(),
                "case {number}"
            );
            assert_eq!(
                !state.fast.is_empty(),
                case["expected_fast"].as_bool().unwrap(),
                "case {number}"
            );
            let actual: BTreeMap<String, Value> = db
                .load_gateway_routing_entries(index.scope())
                .unwrap()
                .into_iter()
                .map(|(k, v)| (k, serde_json::from_str(&v).unwrap()))
                .collect();
            let expected = case["calls"].get("database").cloned().unwrap_or(json!({}));
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                expected,
                "case {number}"
            );
            if let Some(expected) = case["calls"].get("mirror") {
                let mut actual: Value =
                    serde_json::from_slice(&std::fs::read(&mirror_path).unwrap()).unwrap();
                actual.as_object_mut().unwrap().remove("_README");
                assert_eq!(&actual, expected, "case {number}");
            } else {
                assert!(!mirror_path.is_file(), "case {number}");
            }
            drop(state);
            drop(db);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn delayed_snapshots_and_fast_writes_cannot_regress_routes() {
        let dir = directory("ordered");
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let db = SessionDb::open(dir.join("state.db")).unwrap();
        index.ensure_loaded(Some(&db));
        index.entries.insert(
            "key".into(),
            SessionEntry::from_dict(&entry("same-id")).unwrap(),
        );
        let delayed = index.snapshot(Some(&db));
        index
            .entries
            .get_mut("key")
            .unwrap()
            .fields
            .insert("input_tokens".into(), json!(10));
        index.save_entry("key", None, Some(&db), true).unwrap();
        // A fast save only updates the primary, even when mirroring is enabled.
        assert!(!dir.join("sessions/sessions.json").exists());
        index.writer.persist(delayed, Some(&db), true).unwrap();
        let rows = db.load_gateway_routing_entries(index.scope()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&rows["key"]).unwrap()["input_tokens"],
            10
        );
        let mirror: Value =
            serde_json::from_slice(&std::fs::read(dir.join("sessions/sessions.json")).unwrap())
                .unwrap();
        assert_eq!(mirror["key"]["input_tokens"], 10);
        assert_eq!(mirror["_README"], LEGACY_NOTE);
        // A delayed fast write also loses to the newer fast revision.
        index
            .writer
            .persist_entry("key", &entry("obsolete"), 1, Some(&db))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(
                &db.load_gateway_routing_entries(index.scope()).unwrap()["key"]
            )
            .unwrap()["input_tokens"],
            10
        );
        let older = index.snapshot(Some(&db));
        index.entries.remove("key");
        index.save(Some(&db), true).unwrap();
        index.writer.persist(older, Some(&db), true).unwrap();
        index
            .writer
            .persist_entry("key", &entry("obsolete"), 2, Some(&db))
            .unwrap();
        assert!(db
            .load_gateway_routing_entries(index.scope())
            .unwrap()
            .is_empty());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fallback_candidate_is_persisted_before_live_publication() {
        let dir = directory("candidate");
        let mut index = RoutingIndex::new(dir.clone()).unwrap();
        index.ensure_loaded(None);
        index.entries.insert(
            "key".into(),
            SessionEntry::from_dict(&entry("same-id")).unwrap(),
        );
        let mut candidate = index.entries["key"].to_dict();
        candidate["resume_pending"] = json!(true);
        index
            .save_entry("key", Some(candidate), None, false)
            .unwrap();
        assert_eq!(index.entries["key"].fields["resume_pending"], false);
        let mut reopened = RoutingIndex::new(dir.clone()).unwrap();
        reopened.ensure_loaded(None);
        assert_eq!(reopened.entries["key"].fields["resume_pending"], true);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_mirror_only_fails_when_primary_also_failed() {
        let dir = directory("mirror-failure");
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let db = SessionDb::open(dir.join("state.db")).unwrap();
        index.ensure_loaded(Some(&db));
        index.entries.insert(
            "key".into(),
            SessionEntry::from_dict(&entry("durable")).unwrap(),
        );
        // Rename over a directory fails on the real filesystem.
        std::fs::create_dir(dir.join("sessions/sessions.json")).unwrap();
        index.save(Some(&db), true).unwrap();
        assert!(db
            .load_gateway_routing_entries(index.scope())
            .unwrap()
            .contains_key("key"));
        let persisted = index.writer.state.lock().unwrap().persisted;
        assert!(index.save(None, true).is_err());
        assert_eq!(index.writer.state.lock().unwrap().persisted, persisted);
        assert!(std::fs::read_dir(dir.join("sessions")).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        std::fs::remove_dir(dir.join("sessions/sessions.json")).unwrap();
        index.save(None, false).unwrap();
        assert!(dir.join("sessions/sessions.json").is_file());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_decisions_match_python() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-routing-recovery-goldens.json"
        ))
        .unwrap();
        let dir = directory("oracle");
        for case in cases {
            let mut index = RoutingIndex::new(dir.clone()).unwrap();
            index.fallback_baseline =
                Some(serde_json::from_value(case["baseline"].clone()).unwrap());
            for (key, data) in case["current"].as_object().unwrap() {
                index
                    .entries
                    .insert(key.clone(), SessionEntry::from_dict(data).unwrap());
            }
            index.merge_recovered(serde_json::from_value(case["durable"].clone()).unwrap());
            assert_eq!(
                serde_json::to_value(index.serialized()).unwrap(),
                case["expected"],
                "{case}"
            );
            assert!(index.database_loaded);
            assert!(index.fallback_baseline.is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_database_load_retries_recovery_without_reloading_legacy() {
        let dir = directory("failed-db");
        let db_path = dir.join("state.db");
        let db = SessionDb::open(db_path.clone()).unwrap();
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute("DROP TABLE gateway_routing", [])
            .unwrap();
        std::fs::write(
            dir.join("sessions/sessions.json"),
            json!({"key":entry("fallback")}).to_string(),
        )
        .unwrap();
        index.ensure_loaded(Some(&db));
        assert!(!index.database_loaded);
        assert!(index.fallback_baseline.is_some());
        // A second failed read must leave the recovery baseline intact.
        index.ensure_loaded(Some(&db));
        assert!(!index.database_loaded);
        std::fs::write(
            dir.join("sessions/sessions.json"),
            json!({"legacy-only":entry("late")}).to_string(),
        )
        .unwrap();
        let repaired = SessionDb::open(db_path).unwrap();
        repaired
            .save_gateway_routing_entry(index.scope(), "key", &entry("recovered").to_string())
            .unwrap();
        index.ensure_loaded(Some(&db));
        assert_eq!(index.entries["key"].session_id, "recovered");
        assert!(!index.entries.contains_key("legacy-only"));
        assert!(index.database_loaded);
        drop(connection);
        drop(repaired);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn database_wins_and_invalid_rows_do_not_abort_legacy_import() {
        let dir = directory("load");
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        let db = SessionDb::open(dir.join("state.db")).unwrap();
        db.save_gateway_routing_entry(index.scope(), "shared", &entry("primary").to_string())
            .unwrap();
        db.save_gateway_routing_entry(index.scope(), "bad-db", "invalid-json")
            .unwrap();
        std::fs::write(
            dir.join("sessions/sessions.json"),
            json!({
                "_README":entry("sentinel"), "shared":entry("legacy"), "bad-db":entry("fallback"),
                "broken":false, "bad-id":entry("../escape"), "later":entry("valid")
            })
            .to_string(),
        )
        .unwrap();
        index.ensure_loaded(Some(&db));
        assert_eq!(index.entries.len(), 3);
        assert_eq!(index.entries["shared"].session_id, "primary");
        assert_eq!(index.entries["bad-db"].session_id, "fallback");
        assert_eq!(index.entries["later"].session_id, "valid");
        assert!(index.database_loaded);
        assert!(index.fallback_baseline.is_none());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_preserves_outage_edits_deletions_and_creations() {
        let dir = directory("recover");
        let mut index = RoutingIndex::new(dir.join("sessions")).unwrap();
        std::fs::write(
            dir.join("sessions/sessions.json"),
            json!({
                "untouched":entry("old"), "edited":entry("old"), "deleted":entry("old")
            })
            .to_string(),
        )
        .unwrap();
        index.ensure_loaded(None);
        index.entries.get_mut("edited").unwrap().session_id = "local-edit".into();
        index.entries.remove("deleted");
        index.entries.insert(
            "created".into(),
            SessionEntry::from_dict(&entry("local-new")).unwrap(),
        );
        let db = SessionDb::open(dir.join("state.db")).unwrap();
        for key in ["untouched", "edited", "deleted", "created", "db-only"] {
            db.save_gateway_routing_entry(index.scope(), key, &entry("durable").to_string())
                .unwrap();
        }
        index.ensure_loaded(Some(&db));
        assert_eq!(index.entries["untouched"].session_id, "durable");
        assert_eq!(index.entries["edited"].session_id, "local-edit");
        assert!(!index.entries.contains_key("deleted"));
        assert_eq!(index.entries["created"].session_id, "local-new");
        assert_eq!(index.entries["db-only"].session_id, "durable");
        assert!(index.fallback_baseline.is_none());
        // Recovery happens once. A later database update cannot clobber memory.
        db.save_gateway_routing_entry(index.scope(), "edited", &entry("later").to_string())
            .unwrap();
        index.ensure_loaded(Some(&db));
        assert_eq!(index.entries["edited"].session_id, "local-edit");
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
