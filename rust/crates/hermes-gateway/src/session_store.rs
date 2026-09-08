//! Owning gateway session coordinator. Initialize the routing index before
//! exposing it to concurrent turn workers; session history stays profile-owned.
#![allow(dead_code)]

use crate::{
    config_gateway::GatewayConfig,
    config_schema::Platform,
    session_db_recovery::SessionDatabases,
    session_routing::{RecoveryRequest, RoutingIndex, SessionFlights, SessionRecovery},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct SessionStore {
    pub(crate) index: Mutex<RoutingIndex>,
    pub(crate) databases: SessionDatabases,
    pub(crate) recovery: Arc<SessionRecovery>,
    pub(crate) flights: Arc<SessionFlights>,
    config: GatewayConfig,
    home: PathBuf,
    active_profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredSession {
    pub session_key: String,
    pub session_id: String,
    pub home: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ExplicitSessionReset {
    pub entry: crate::session_entry::SessionEntry,
    pub predecessor_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExplicitSessionSwitch {
    pub entry: crate::session_entry::SessionEntry,
    pub predecessor_id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompressionRanges {
    pub prefix_end_id: Option<i64>,
    pub tail_start_id: Option<i64>,
    pub watermark: i64,
}

struct SessionOrigin<'a> {
    source: &'a crate::session::SessionSource,
    legacy: Option<(&'a str, &'a str)>,
}

impl SessionStore {
    pub fn session_key_for_source(&self, source: &crate::session::SessionSource) -> String {
        let profile = self.config.multiplex_profiles.then(|| {
            source
                .profile
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or(&self.active_profile)
        });
        crate::session::build_session_key(
            source,
            self.config.group_sessions_per_user,
            self.config.thread_sessions_per_user,
            profile,
        )
    }

    /// Snapshot the current route for a source. The returned entry carries an
    /// object-generation token suitable for a later compare-and-swap reset.
    pub fn current_entry_for_source(
        &self,
        source: &crate::session::SessionSource,
    ) -> Option<crate::session_entry::SessionEntry> {
        let key = self.session_key_for_source(source);
        let routing = self.databases.routing();
        self.reconcile(routing.as_deref());
        self.index.lock().unwrap().entries.get(&key).cloned()
    }

    /// Refresh one route directly from SQLite after a cross-process wait.
    /// Steady-state recovery is intentionally one-shot, so a queued turn must
    /// explicitly observe rotations committed by another live process.
    pub fn refresh_current_entry_from_database(
        &self,
        source: &crate::session::SessionSource,
    ) -> anyhow::Result<Option<crate::session_entry::SessionEntry>> {
        let key = self.session_key_for_source(source);
        let Some(database) = self.databases.routing() else {
            return Ok(self.index.lock().unwrap().entries.get(&key).cloned());
        };
        let scope = self.index.lock().unwrap().scope().to_owned();
        let Some(raw) = database.load_gateway_routing_entry(&scope, &key)? else {
            return Ok(self.index.lock().unwrap().entries.get(&key).cloned());
        };
        let value = serde_json::from_str(&raw)
            .map_err(|error| anyhow::anyhow!("invalid durable route for {key}: {error}"))?;
        let durable = crate::session_entry::SessionEntry::from_dict(&value)?;
        let mut index = self.index.lock().unwrap();
        match index.entries.get(&key) {
            Some(current) if current.session_id == durable.session_id => Ok(Some(current.clone())),
            _ => {
                index.entries.insert(key.clone(), durable);
                Ok(index.entries.get(&key).cloned())
            }
        }
    }

    /// True only while the route still names the entry observed before an
    /// asynchronous lease wait.
    pub fn route_matches(&self, observed: &crate::session_entry::SessionEntry) -> bool {
        self.index
            .lock()
            .unwrap()
            .entries
            .get(&observed.session_key)
            .is_some_and(|current| {
                current.same_instance(observed) && current.session_id == observed.session_id
            })
    }

    /// Explicit user-driven reset with a compare-and-swap publication fence.
    /// The caller holds the predecessor's turn lease when one exists. `None`
    /// means the route changed since `expected` was observed and the caller
    /// must retry against the new route.
    pub fn reset_session(
        &self,
        source: &crate::session::SessionSource,
        expected: Option<&crate::session_entry::SessionEntry>,
    ) -> anyhow::Result<Option<ExplicitSessionReset>> {
        use crate::session_entry::{CreationContext, SessionEntry};

        let key = self.session_key_for_source(source);
        let routing = self.databases.routing();
        self.reconcile(routing.as_deref());
        let now = chrono::Local::now().naive_local();
        let predecessor_id = expected.map(|entry| entry.session_id.clone());
        let context = CreationContext {
            is_fresh_reset: true,
            prev_session_id: predecessor_id.clone(),
            ..Default::default()
        };
        let creation_source = expected
            .and_then(|entry| entry.origin.as_ref())
            .unwrap_or(source);
        let mut candidate = SessionEntry::new_candidate(&key, creation_source, now, &context)?;
        if creation_source.chat_name.is_none() {
            if let Some(display_name) =
                expected.and_then(|entry| entry.fields["display_name"].as_str())
            {
                candidate
                    .fields
                    .insert("display_name".into(), serde_json::json!(display_name));
            }
        }
        let (entry, won) = self
            .index
            .lock()
            .unwrap()
            .publish_forced_candidate(candidate, expected);
        if !won {
            return Ok(None);
        }

        self.persist_full(routing.as_deref())?;
        if let Some(db) = self.databases.for_key(&key, &self.home) {
            if let Some(parent) = predecessor_id.as_deref() {
                if !db.promote_to_session_reset(parent, "session_reset") {
                    tracing::warn!(%parent, "explicit reset predecessor promotion did not update a row");
                }
            }
            let origin = creation_source.to_dict().to_string();
            let model_config = predecessor_id
                .as_ref()
                .map(|parent| serde_json::json!({"_reset_from": parent}));
            let create = crate::session_db::SessionCreate {
                peer: crate::session_db::GatewayPeer {
                    source: &creation_source.platform,
                    session_key: Some(&key),
                    user_id: creation_source.user_id.as_deref(),
                    chat_id: Some(&creation_source.chat_id),
                    chat_type: Some(&creation_source.chat_type),
                    thread_id: creation_source.thread_id.as_deref(),
                },
                profile_name: creation_source.profile.as_deref(),
                origin_json: Some(&origin),
                display_name: entry.fields["display_name"].as_str(),
                parent_session_id: predecessor_id.as_deref(),
                model_config: model_config.as_ref(),
                ..Default::default()
            };
            if let Err(error) = db.create_session(&entry.session_id, &create) {
                tracing::warn!(%error, "explicit reset session creation deferred to peer repair");
            } else {
                self.refresh_peer(&entry);
            }
        }
        Ok(Some(ExplicitSessionReset {
            entry,
            predecessor_id,
        }))
    }

    /// Repoint a stable route to an existing transcript. The caller must hold
    /// the route plus both transcript leases. SQLite owns the atomic durable
    /// transition; memory is published only after that transaction commits.
    pub fn switch_session(
        &self,
        source: &crate::session::SessionSource,
        expected: &crate::session_entry::SessionEntry,
        target_session_id: &str,
    ) -> anyhow::Result<Option<ExplicitSessionSwitch>> {
        let key = self.session_key_for_source(source);
        self.reconcile(self.databases.routing().as_deref());
        let db = self
            .databases
            .for_key(&key, &self.home)
            .ok_or_else(|| anyhow::anyhow!("session database is unavailable"))?;
        let (entry, writer, snapshot) = {
            let mut index = self.index.lock().unwrap();
            let Some(current) = index.entries.get(&key) else {
                return Ok(None);
            };
            if !current.same_instance(expected) || current.session_id != expected.session_id {
                return Ok(None);
            }
            if target_session_id == current.session_id {
                return Ok(Some(ExplicitSessionSwitch {
                    entry: current.clone(),
                    predecessor_id: current.session_id.clone(),
                }));
            }
            let candidate = crate::session_entry::SessionEntry::resumed_candidate(
                current,
                target_session_id,
                chrono::Local::now().naive_local(),
            )?;
            let entry_json = candidate.to_dict().to_string();
            let origin_json = source.to_dict().to_string();
            let display_name = candidate.fields["display_name"].as_str();
            let switched = db.switch_gateway_session(&crate::session_db::GatewaySessionSwitch {
                scope: index.scope(),
                session_key: &key,
                entry_json: &entry_json,
                outgoing_id: &current.session_id,
                target_id: target_session_id,
                peer: crate::session_db::GatewayPeer {
                    source: &source.platform,
                    session_key: Some(&key),
                    user_id: source.user_id.as_deref(),
                    chat_id: Some(&source.chat_id),
                    chat_type: Some(&source.chat_type),
                    thread_id: source.thread_id.as_deref(),
                },
                display_name,
                origin_json: Some(&origin_json),
            })?;
            if !switched {
                return Ok(None);
            }
            index.entries.insert(key.clone(), candidate.clone());
            let writer = index.writer.clone();
            let snapshot = index.snapshot_loaded();
            (candidate, writer, snapshot)
        };
        writer.accept_database_commit(snapshot, self.config.write_sessions_json);
        Ok(Some(ExplicitSessionSwitch {
            entry,
            predecessor_id: expected.session_id.clone(),
        }))
    }

    /// Publish a rotation-mode compression child. The database transaction
    /// contains the child transcript, stable routing row and parent closure;
    /// only a successful commit becomes visible in the in-memory index.
    pub fn publish_compression(
        &self,
        source: &crate::session::SessionSource,
        expected: &crate::session_entry::SessionEntry,
        compacted_messages: &[crate::session_db::HistoryMessage],
        ranges: CompressionRanges,
        turn_lease_holder: Option<&str>,
    ) -> anyhow::Result<Option<ExplicitSessionSwitch>> {
        let key = self.session_key_for_source(source);
        self.reconcile(self.databases.routing().as_deref());
        let db = self
            .databases
            .for_key(&key, &self.home)
            .ok_or_else(|| anyhow::anyhow!("session database is unavailable"))?;
        let (entry, writer, snapshot) = {
            let mut index = self.index.lock().unwrap();
            let Some(current) = index.entries.get(&key) else {
                return Ok(None);
            };
            if !current.same_instance(expected) || current.session_id != expected.session_id {
                return Ok(None);
            }
            let candidate = crate::session_entry::SessionEntry::compression_candidate(
                current,
                chrono::Local::now().naive_local(),
            )?;
            let entry_json = candidate.to_dict().to_string();
            let published =
                db.publish_gateway_compression(&crate::session_db::GatewayCompressionPublish {
                    scope: index.scope(),
                    session_key: &key,
                    entry_json: &entry_json,
                    parent_id: &current.session_id,
                    child_id: &candidate.session_id,
                    compacted_messages,
                    prefix_end_id: ranges.prefix_end_id,
                    tail_start_id: ranges.tail_start_id,
                    watermark: ranges.watermark,
                    turn_lease_holder,
                })?;
            if !published {
                return Ok(None);
            }
            index.entries.insert(key.clone(), candidate.clone());
            let writer = index.writer.clone();
            let snapshot = index.snapshot_loaded();
            (candidate, writer, snapshot)
        };
        writer.accept_database_commit(snapshot, self.config.write_sessions_json);
        Ok(Some(ExplicitSessionSwitch {
            entry,
            predecessor_id: expected.session_id.clone(),
        }))
    }

    /// Publish an in-place compaction without changing route identity. The
    /// database owns the soft archive and live-set replacement atomically.
    pub fn publish_in_place_compression(
        &self,
        source: &crate::session::SessionSource,
        expected: &crate::session_entry::SessionEntry,
        compacted_messages: &[crate::session_db::HistoryMessage],
        ranges: CompressionRanges,
        turn_lease_holder: Option<&str>,
    ) -> anyhow::Result<bool> {
        let key = self.session_key_for_source(source);
        let db = self
            .databases
            .for_key(&key, &self.home)
            .ok_or_else(|| anyhow::anyhow!("session database is unavailable"))?;
        let index = self.index.lock().unwrap();
        let Some(current) = index.entries.get(&key) else {
            return Ok(false);
        };
        if !current.same_instance(expected) || current.session_id != expected.session_id {
            return Ok(false);
        }
        Ok(db.publish_gateway_in_place_compression(
            &crate::session_db::GatewayInPlaceCompressionPublish {
                scope: index.scope(),
                session_key: &key,
                session_id: &current.session_id,
                compacted_messages,
                prefix_end_id: ranges.prefix_end_id,
                tail_start_id: ranges.tail_start_id,
                watermark: ranges.watermark,
                turn_lease_holder,
            },
        )?)
    }

    pub fn publish_tool_prune(
        &self,
        source: &crate::session::SessionSource,
        expected: &crate::session_entry::SessionEntry,
        original_messages: &[crate::session_db::CompressionHistoryMessage],
        pruned_messages: &[crate::session_db::CompressionHistoryMessage],
        rearm_tokens: u64,
        turn_lease_holder: Option<&str>,
    ) -> anyhow::Result<bool> {
        let key = self.session_key_for_source(source);
        let db = self
            .databases
            .for_key(&key, &self.home)
            .ok_or_else(|| anyhow::anyhow!("session database is unavailable"))?;
        let index = self.index.lock().unwrap();
        let Some(current) = index.entries.get(&key) else {
            return Ok(false);
        };
        if !current.same_instance(expected) || current.session_id != expected.session_id {
            return Ok(false);
        }
        Ok(
            db.publish_gateway_tool_prune(&crate::session_db::GatewayToolPrunePublish {
                scope: index.scope(),
                session_key: &key,
                session_id: &current.session_id,
                original_messages,
                pruned_messages,
                rearm_tokens,
                turn_lease_holder,
            })?,
        )
    }

    pub fn lookup_by_session_id(
        &self,
        session_id: &str,
    ) -> Option<crate::session_entry::SessionEntry> {
        self.reconcile(self.databases.routing().as_deref());
        self.index
            .lock()
            .unwrap()
            .entries
            .values()
            .find(|entry| entry.session_id == session_id)
            .cloned()
    }

    pub fn database_for_key(&self, key: &str) -> Option<Arc<crate::session_db::SessionDb>> {
        self.databases.for_key(key, &self.home)
    }

    /// Ensure a route-only entry has its durable SQLite row. Explicit metadata
    /// commands use this before the first model turn so their writes and later
    /// ownership checks do not depend on a future transcript append.
    pub fn materialize_session_entry(
        &self,
        entry: &crate::session_entry::SessionEntry,
        source: &crate::session::SessionSource,
    ) -> anyhow::Result<Arc<crate::session_db::SessionDb>> {
        let database = self
            .database_for_key(&entry.session_key)
            .ok_or_else(|| anyhow::anyhow!("session database is unavailable"))?;
        let origin = source.to_dict().to_string();
        database.create_session(
            &entry.session_id,
            &crate::session_db::SessionCreate {
                peer: crate::session_db::GatewayPeer {
                    source: &source.platform,
                    session_key: Some(&entry.session_key),
                    user_id: source.user_id.as_deref(),
                    chat_id: Some(&source.chat_id),
                    chat_type: Some(&source.chat_type),
                    thread_id: source.thread_id.as_deref(),
                },
                profile_name: source.profile.as_deref(),
                origin_json: Some(&origin),
                display_name: entry
                    .fields
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .or(source.chat_name.as_deref()),
                ..Default::default()
            },
        )?;
        self.refresh_peer(entry);
        Ok(database)
    }

    fn policy_for_entry(
        &self,
        entry: &crate::session_entry::SessionEntry,
    ) -> crate::config_types::SessionResetPolicy {
        let platform = entry
            .origin
            .as_ref()
            .map(|source| source.platform.as_str())
            .or_else(|| entry.fields["platform"].as_str());
        let chat_type = entry
            .origin
            .as_ref()
            .map(|source| source.chat_type.as_str())
            .or_else(|| entry.fields["chat_type"].as_str());
        self.config
            .get_reset_policy(platform.and_then(Platform::from_value), chat_type)
            .clone()
    }

    /// Whether the expiry watcher will eventually establish a real session
    /// boundary. Only the explicit `none` policy has no future boundary.
    pub fn is_session_finalizable(&self, entry: &crate::session_entry::SessionEntry) -> bool {
        self.policy_for_entry(entry).mode != "none"
    }

    /// Consume the one-shot predecessor marker published by an automatic
    /// reset. Object identity prevents an older resolver from clearing a newer
    /// route. The durable routing update happens outside the index lock.
    pub fn take_auto_reset_predecessor(
        &self,
        observed: &crate::session_entry::SessionEntry,
    ) -> anyhow::Result<Option<String>> {
        let database = self.database_for_key(&observed.session_key);
        let captured = {
            let mut index = self.index.lock().unwrap();
            let Some(entry) = index
                .entries
                .get_mut(&observed.session_key)
                .filter(|entry| entry.same_instance(observed))
            else {
                return Ok(None);
            };
            if !crate::python_value::truthy(&entry.fields["was_auto_reset"]) {
                return Ok(None);
            }
            let previous = entry.fields["prev_session_id"]
                .as_str()
                .filter(|previous| *previous != entry.session_id)
                .map(str::to_owned);
            entry
                .fields
                .insert("was_auto_reset".into(), serde_json::json!(false));
            let (data, revision) = index.capture_entry(&observed.session_key).unwrap();
            (index.writer.clone(), data, revision, previous)
        };
        if !captured.0.persist_entry(
            &observed.session_key,
            &captured.1,
            captured.2,
            database.as_deref(),
        )? {
            self.persist_full(database.as_deref())?;
        }
        Ok(captured.3)
    }

    /// Capture expired routing entries without holding the routing lock during
    /// later provider teardown or database writes.
    pub fn expired_sessions(&self) -> Vec<ExpiredSession> {
        self.expired_sessions_at(chrono::Local::now().naive_local())
    }

    fn expired_sessions_at(&self, now: chrono::NaiveDateTime) -> Vec<ExpiredSession> {
        let expired: Vec<_> = self
            .index
            .lock()
            .unwrap()
            .entries
            .values()
            .filter(|entry| !crate::python_value::truthy(&entry.fields["expiry_finalized"]))
            .filter(|entry| {
                crate::session_reset::reset_reason(
                    &self.policy_for_entry(entry),
                    entry.updated_at.local,
                    now,
                    false,
                )
                .unwrap_or(None)
                .is_some()
            })
            .map(|entry| (entry.session_key.clone(), entry.session_id.clone()))
            .collect();
        expired
            .into_iter()
            .map(|(session_key, session_id)| ExpiredSession {
                home: self
                    .database_for_key(&session_key)
                    .and_then(|db| db.profile_home().map(PathBuf::from))
                    .unwrap_or_else(|| self.home.clone()),
                session_key,
                session_id,
            })
            .collect()
    }

    /// Record a completed expiry boundary in SQLite first, then mirror the
    /// routing entry. SQLite changes share one short transaction; provider I/O
    /// has already completed before this method is called.
    pub fn finalize_expired_session(&self, expired: &ExpiredSession) -> anyhow::Result<()> {
        let database = self.database_for_key(&expired.session_key);
        if let Some(database) = database.as_deref() {
            database.finalize_session_expiry(&expired.session_id)?;
        }
        let captured = {
            let mut index = self.index.lock().unwrap();
            let Some(entry) = index
                .entries
                .get_mut(&expired.session_key)
                .filter(|entry| entry.session_id == expired.session_id)
            else {
                return Ok(());
            };
            entry
                .fields
                .insert("expiry_finalized".into(), serde_json::json!(true));
            entry.fields.remove("model_override");
            let (data, revision) = index.capture_entry(&expired.session_key).unwrap();
            (index.writer.clone(), data, revision)
        };
        if !captured.0.persist_entry(
            &expired.session_key,
            &captured.1,
            captured.2,
            database.as_deref(),
        )? {
            self.persist_full(database.as_deref())?;
        }
        Ok(())
    }

    fn recovery_request<'a>(
        &'a self,
        key: &'a str,
        source: &'a crate::session::SessionSource,
        now: chrono::NaiveDateTime,
        active: bool,
    ) -> RecoveryRequest<'a> {
        RecoveryRequest {
            key,
            source,
            now,
            timezone: None,
            active_processes: active,
            policy: self.config.get_reset_policy(
                Platform::from_value(&source.platform),
                Some(&source.chat_type),
            ),
            multiplex_profiles: self.config.multiplex_profiles,
            active_profile: &self.active_profile,
            group_per_user: self.config.group_sessions_per_user,
            thread_per_user: self.config.thread_sessions_per_user,
        }
    }

    /// Synchronous transition owner/waiter boundary. Async callers must run this
    /// through spawn_blocking, since SQLite and flight waiting can block.
    pub fn get_or_create_session(
        &self,
        source: &crate::session::SessionSource,
        force_new: bool,
        touch_activity: bool,
        freshness_seconds: f64,
        active_processes: impl FnMut(&str) -> anyhow::Result<bool>,
    ) -> Result<crate::session_entry::SessionEntry, Arc<anyhow::Error>> {
        self.get_or_create_with_legacy(
            source,
            None,
            force_new,
            touch_activity,
            freshness_seconds,
            active_processes,
        )
    }

    /// Ingress may supply the exact old Rust (session ID, source) pair. Only
    /// the flight owner considers it, after ordinary recovery has missed.
    pub fn get_or_create_with_legacy(
        &self,
        source: &crate::session::SessionSource,
        legacy: Option<(&str, &str)>,
        force_new: bool,
        touch_activity: bool,
        freshness_seconds: f64,
        mut active_processes: impl FnMut(&str) -> anyhow::Result<bool>,
    ) -> Result<crate::session_entry::SessionEntry, Arc<anyhow::Error>> {
        let key = self.session_key_for_source(source);
        match self.flights.join(&key) {
            crate::session_routing::SessionFlightTicket::Owner(owner) => {
                owner.complete(self.transition(
                    &key,
                    SessionOrigin { source, legacy },
                    force_new,
                    touch_activity,
                    freshness_seconds,
                    &mut active_processes,
                ))
            }
            crate::session_routing::SessionFlightTicket::Waiter(waiter) => {
                let result = waiter.wait()?;
                if touch_activity {
                    self.update_session(&key, None, true).map_err(Arc::new)?;
                }
                let index = self.index.lock().unwrap();
                Ok(index
                    .entries
                    .get(&key)
                    .filter(|current| current.same_instance(&result))
                    .cloned()
                    .unwrap_or(result))
            }
        }
    }

    fn transition(
        &self,
        key: &str,
        origin: SessionOrigin<'_>,
        force_new: bool,
        touch_activity: bool,
        freshness_seconds: f64,
        active_processes: &mut impl FnMut(&str) -> anyhow::Result<bool>,
    ) -> anyhow::Result<crate::session_entry::SessionEntry> {
        use crate::session_entry::{CreationContext, EntryTimestamp, SessionEntry};
        let SessionOrigin { source, legacy } = origin;
        let now = chrono::Local::now().naive_local();
        let routing = self.databases.routing();
        self.reconcile(routing.as_deref());
        let request = self.recovery_request(key, source, now, false);
        if !force_new {
            if let Some(legacy) = SessionRecovery::legacy_key(&request) {
                let migrated = {
                    let mut index = self.index.lock().unwrap();
                    let adopt = !index.entries.contains_key(key)
                        && index.entries.get(&legacy).is_some_and(|entry| {
                            match entry.origin.as_ref().and_then(|origin| origin.scope()) {
                                Some(scope) => Some(scope) == source.scope(),
                                None => source.chat_type == "dm",
                            }
                        });
                    if adopt && self.recovery.claim_legacy(&legacy) {
                        let mut entry = index.entries.remove(&legacy).unwrap();
                        entry.session_key = key.to_owned();
                        entry.origin = Some(source.clone());
                        entry
                            .fields
                            .insert("platform".into(), serde_json::json!(source.platform));
                        entry
                            .fields
                            .insert("chat_type".into(), serde_json::json!(source.chat_type));
                        index.entries.insert(key.to_owned(), entry.clone());
                        Some(entry)
                    } else {
                        None
                    }
                };
                if let Some(migrated) = migrated {
                    self.persist_full(routing.as_deref())?;
                    self.refresh_peer(&migrated);
                }
            }
        }
        let initial = self.index.lock().unwrap().entries.get(key).cloned();
        let db = self.databases.for_key(key, &self.home);
        let tip = if !force_new {
            initial.as_ref().and_then(|entry| {
                db.as_ref()
                    .and_then(|db| db.get_compression_tip(&entry.session_id).ok())
            })
        } else {
            None
        };
        let observed = self.index.lock().unwrap().entries.get(key).cloned();
        let active = !force_new && active_processes(key).unwrap_or(true);
        let request = self.recovery_request(key, source, now, active);
        let stale = !force_new
            && observed.as_ref().is_some_and(|entry| {
                db.as_ref()
                    .and_then(|db| db.get_session(&entry.session_id).ok().flatten())
                    .is_some_and(|row| !row["end_reason"].is_null())
            });
        let reset = if !force_new {
            observed
                .as_ref()
                .map(|entry| {
                    // Python holds a reference to the observed entry across
                    // process/DB probes. Refresh its fields after that I/O so
                    // a concurrent activity touch is visible to reset policy.
                    // A replacement entry is a different object and must not
                    // supply policy fields for the old observed session.
                    let policy_entry = self
                        .index
                        .lock()
                        .unwrap()
                        .entries
                        .get(key)
                        .filter(|current| current.same_instance(entry))
                        .cloned()
                        .unwrap_or_else(|| entry.clone());
                    crate::session_reset::existing_reset_reason(
                        &policy_entry,
                        request.policy,
                        now,
                        active,
                        freshness_seconds,
                    )
                })
                .transpose()?
                .flatten()
        } else {
            None
        };
        let mut context = CreationContext::default();
        let mut needs_save = false;
        let mut metadata_only = false;
        let mut entry = None;
        if !force_new {
            let mut index = self.index.lock().unwrap();
            if let Some(current) = index.entries.get_mut(key) {
                let healed = current.heal_compression_tip(
                    initial.as_ref().map(|entry| entry.session_id.as_str()),
                    tip.as_deref(),
                );
                let same_id = observed
                    .as_ref()
                    .is_some_and(|entry| current.session_id == entry.session_id);
                if same_id && (stale || reset.is_some()) {
                    if let Some(reason) = reset {
                        let tokens = &current.fields["last_prompt_tokens"];
                        let tokens = tokens
                            .as_f64()
                            .or_else(|| tokens.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
                            .ok_or_else(|| anyhow::anyhow!("last_prompt_tokens must be numeric"))?;
                        context = CreationContext {
                            is_fresh_reset: false,
                            was_auto_reset: true,
                            auto_reset_reason: Some(reason.into()),
                            reset_had_activity: tokens > 0.0,
                            prev_session_id: Some(current.session_id.clone()),
                        };
                    }
                    index.entries.remove(key);
                } else {
                    if touch_activity {
                        current.updated_at = EntryTimestamp {
                            local: now,
                            offset_micros: None,
                        };
                    }
                    needs_save = touch_activity || healed;
                    metadata_only = touch_activity && !healed;
                    entry = Some(current.clone());
                }
            }
        }
        if entry.is_none() && !force_new && context.prev_session_id.is_none() {
            let mut recovered = self
                .recovery
                .query_recoverable(&request, |key| self.databases.for_key(key, &self.home))?;
            if recovered.is_none() {
                if let Some((id, old_source)) = legacy {
                    if let Some(db) = self.database_for_key(key) {
                        // Old transport IDs could contain path separators.
                        // Do not mutate one into routing state that the entry
                        // codec cannot load, or silently abandon its history.
                        anyhow::ensure!(
                            !crate::session::is_path_unsafe(id) || db.get_session(id)?.is_none(),
                            "legacy transcript requires a path-safe ID migration"
                        );
                        let peer = crate::session_db::GatewayPeer {
                            source: &source.platform,
                            session_key: Some(key),
                            user_id: source.user_id.as_deref(),
                            chat_id: Some(&source.chat_id),
                            chat_type: Some(&source.chat_type),
                            thread_id: source.thread_id.as_deref(),
                        };
                        // Commit ownership before publishing a route. A crash
                        // after this write is recoverable by the normal finder.
                        // Claim errors propagate so a transient DB failure does
                        // not strand history behind a newly created session.
                        db.claim_legacy_gateway_session(id, old_source, &peer)?;
                        recovered = self
                            .recovery
                            .query_recoverable(&request, |key| self.database_for_key(key))?;
                    }
                }
            }
            if let Some(recovered) = recovered {
                if let Some(reason) = crate::session_reset::reset_reason(
                    request.policy,
                    recovered.updated_at.local,
                    now,
                    active,
                )? {
                    context = CreationContext {
                        is_fresh_reset: false,
                        was_auto_reset: true,
                        auto_reset_reason: Some(reason.into()),
                        reset_had_activity: crate::python_value::truthy(
                            &recovered.fields["reset_had_activity"],
                        ),
                        prev_session_id: Some(recovered.session_id),
                    };
                } else {
                    if let Some(db) = self.databases.for_key(key, &self.home) {
                        if let Err(error) = db.reopen_session(&recovered.session_id) {
                            tracing::debug!(%error,"session reopen failed");
                        }
                    }
                    entry = Some(self.index.lock().unwrap().publish_candidate(recovered).0);
                    needs_save = true;
                }
            }
        }
        let mut create = false;
        if entry.is_none() {
            let candidate = SessionEntry::new_candidate(key, source, now, &context)?;
            let (published, won) = self.index.lock().unwrap().publish_forced_candidate(
                candidate,
                if force_new { observed.as_ref() } else { None },
            );
            entry = Some(published);
            create = won;
            needs_save = true;
        }
        let entry = entry.unwrap();
        if needs_save {
            if metadata_only {
                self.persist_metadata(key, routing.as_deref())?;
            } else {
                self.persist_full(routing.as_deref())?;
            }
        }
        if let Some(db) = self.databases.for_key(key, &self.home) {
            if let Some(parent) = &context.prev_session_id {
                if !db.promote_to_session_reset(
                    parent,
                    context
                        .auto_reset_reason
                        .as_deref()
                        .unwrap_or("session_reset"),
                ) {
                    tracing::warn!(%parent,"predecessor reset promotion did not update a row");
                }
            }
            if create {
                let origin = source.to_dict().to_string();
                let model_config = context
                    .prev_session_id
                    .as_ref()
                    .map(|id| serde_json::json!({"_reset_from":id}));
                let create = crate::session_db::SessionCreate {
                    peer: crate::session_db::GatewayPeer {
                        source: &source.platform,
                        session_key: Some(key),
                        user_id: source.user_id.as_deref(),
                        chat_id: Some(&source.chat_id),
                        chat_type: Some(&source.chat_type),
                        thread_id: source.thread_id.as_deref(),
                    },
                    profile_name: source.profile.as_deref(),
                    origin_json: Some(&origin),
                    display_name: source.chat_name.as_deref(),
                    parent_session_id: context.prev_session_id.as_deref(),
                    model_config: model_config.as_ref(),
                    ..Default::default()
                };
                if let Err(error) = db.create_session(&entry.session_id, &create) {
                    tracing::warn!(%error,"session creation deferred to peer repair");
                } else {
                    self.refresh_peer(&entry);
                }
            }
        }
        Ok(entry)
    }

    fn persist_metadata(
        &self,
        key: &str,
        db: Option<&crate::session_db::SessionDb>,
    ) -> anyhow::Result<()> {
        let captured = {
            let mut index = self.index.lock().unwrap();
            index
                .capture_entry(key)
                .map(|(data, revision)| (index.writer.clone(), data, revision))
        };
        if let Some((writer, data, revision)) = captured {
            if !writer.persist_entry(key, &data, revision, db)? {
                self.persist_full(db)?;
            }
        }
        Ok(())
    }

    /// Recover DB-only routing rows outside the entry lock, then merge against
    /// the current in-memory state so concurrent outage edits remain authoritative.
    fn reconcile(&self, db: Option<&crate::session_db::SessionDb>) {
        let scope = self.index.lock().unwrap().recovery_scope();
        if let (Some(scope), Some(db)) = (scope, db) {
            match db.load_gateway_routing_entries(&scope) {
                Ok(rows) => self.index.lock().unwrap().merge_recovered(rows),
                Err(error) => tracing::warn!(%error, "routing recovery read failed"),
            }
        }
    }

    fn persist_full(&self, db: Option<&crate::session_db::SessionDb>) -> anyhow::Result<()> {
        self.reconcile(db);
        let (writer, snapshot) = {
            let mut index = self.index.lock().unwrap();
            (index.writer.clone(), index.snapshot_loaded())
        };
        writer.persist(snapshot, db, self.config.write_sessions_json)
    }

    /// Touch the published route and optionally update token metadata. Peer
    /// identity is captured coherently with the change; all writes run unlocked.
    pub fn update_session(
        &self,
        key: &str,
        last_prompt_tokens: Option<serde_json::Value>,
        touch_activity: bool,
    ) -> anyhow::Result<()> {
        let routing = self.databases.routing();
        self.reconcile(routing.as_deref());
        let (writer, data, revision, peer) = {
            let mut index = self.index.lock().unwrap();
            let Some(entry) = index.entries.get_mut(key) else {
                return Ok(());
            };
            if touch_activity {
                entry.updated_at = crate::session_entry::EntryTimestamp {
                    local: chrono::Local::now().naive_local(),
                    offset_micros: None,
                };
            }
            if let Some(tokens) = last_prompt_tokens.filter(|value| !value.is_null()) {
                entry.fields.insert("last_prompt_tokens".into(), tokens);
            }
            let peer = entry.clone();
            let (data, revision) = index.capture_entry(key).unwrap();
            (index.writer.clone(), data, revision, peer)
        };
        if !writer.persist_entry(key, &data, revision, routing.as_deref())? {
            self.persist_full(routing.as_deref())?;
        }
        self.refresh_peer(&peer);
        Ok(())
    }

    fn refresh_peer(&self, peer: &crate::session_entry::SessionEntry) {
        let key = &peer.session_key;
        if let (Some(source), Some(db)) = (&peer.origin, self.databases.for_key(key, &self.home)) {
            let origin = source.to_dict().to_string();
            let record = crate::session_db::GatewayPeerRecord {
                peer: crate::session_db::GatewayPeer {
                    source: &source.platform,
                    session_key: Some(key),
                    user_id: source.user_id.as_deref(),
                    chat_id: Some(&source.chat_id),
                    chat_type: Some(&source.chat_type),
                    thread_id: source.thread_id.as_deref(),
                },
                origin_json: Some(&origin),
                display_name: peer.fields["display_name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or(source.chat_name.as_deref()),
                include_compression_ancestors: false,
            };
            if let Err(error) = db.record_gateway_session_peer(&peer.session_id, &record) {
                tracing::debug!(%error, "session metadata peer refresh failed");
            }
        }
    }

    /// Run startup loading/pruning before publishing the store to turn workers.
    /// A failed liveness probe pins recovery, matching the safe process boundary.
    /// The fixed home owns routing rows; named keys resolve their own history DB.
    pub fn open(
        config: GatewayConfig,
        root: PathBuf,
        home: PathBuf,
        active_profile: String,
        mut active_processes: impl FnMut(&str) -> anyhow::Result<bool>,
    ) -> anyhow::Result<Self> {
        let databases = SessionDatabases::new(root, home.clone(), config.multiplex_profiles);
        let routing = databases.routing();
        let mut index = RoutingIndex::new(config.sessions_dir.clone())?;
        index.ensure_loaded(routing.as_deref());
        let changed = index.prune_stale(
            |key| databases.for_key(key, &home),
            |index, key, source| {
                let policy = config.get_reset_policy(
                    Platform::from_value(&source.platform),
                    Some(&source.chat_type),
                );
                let request = RecoveryRequest {
                    key,
                    source,
                    policy,
                    now: chrono::Local::now().naive_local(),
                    timezone: None,
                    active_processes: active_processes(key).unwrap_or(true),
                    multiplex_profiles: config.multiplex_profiles,
                    active_profile: &active_profile,
                    group_per_user: config.group_sessions_per_user,
                    thread_per_user: config.thread_sessions_per_user,
                };
                index.recover_from_db(&request, |key| databases.for_key(key, &home))
            },
        );
        if changed {
            index.save(routing.as_deref(), config.write_sessions_json)?;
        }
        let recovery = index.recovery.clone();
        Ok(Self {
            index: Mutex::new(index),
            databases,
            recovery,
            flights: Arc::new(SessionFlights::default()),
            config,
            home,
            active_profile,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session::SessionSource,
        session_db::{GatewayPeer, SessionCreate, SessionDb},
        session_entry::{CreationContext, SessionEntry},
    };

    fn transition_home(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hermes-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn legacy_history_adoption_uses_normal_recovery_and_idle_lineage() {
        let home = transition_home("legacy_history");
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            default_reset_policy: crate::config_types::SessionResetPolicy {
                mode: serde_json::json!("idle"),
                idle_minutes: serde_json::json!(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let store = SessionStore::open(
            config.clone(),
            home.clone(),
            home.clone(),
            "default".into(),
            |_| Ok(false),
        )
        .unwrap();
        let db = store.databases.routing().unwrap();
        for channel in ["C", "old", "forced"] {
            let id = format!("cli:{channel}");
            db.ensure_session(&id, "cli", None, Some(channel), Some("dm"))
                .unwrap();
            db.append_message(&id, "user", "legacy history").unwrap();
        }
        let mut source = SessionSource::new("local", "C");
        source.user_id = Some("U".into());
        let adopted = store
            .get_or_create_with_legacy(&source, Some(("cli:C", "cli")), false, true, 3600.0, |_| {
                Ok(false)
            })
            .unwrap();
        assert_eq!(adopted.session_id, "cli:C");
        assert_eq!(
            db.load_history(&adopted.session_id, 0).unwrap()[0].content,
            "legacy history"
        );
        drop(store);
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        assert_eq!(
            store
                .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
                .unwrap()
                .session_id,
            "cli:C"
        );

        // Claiming must not make an expired transcript look newly active.
        let conn = rusqlite::Connection::open(home.join("state.db")).unwrap();
        conn.execute(
            "UPDATE sessions SET started_at=1, last_activity_at=1 WHERE id='cli:old'",
            [],
        )
        .unwrap();
        drop(conn);
        source.chat_id = "old".into();
        let reset = store
            .get_or_create_with_legacy(
                &source,
                Some(("cli:old", "cli")),
                false,
                true,
                3600.0,
                |_| Ok(false),
            )
            .unwrap();
        assert_ne!(reset.session_id, "cli:old");
        assert_eq!(reset.fields["prev_session_id"], "cli:old");
        assert_eq!(
            db.get_session("cli:old").unwrap().unwrap()["end_reason"],
            "idle"
        );
        assert_eq!(db.load_history("cli:old", 0).unwrap().len(), 1);

        source.chat_id = "forced".into();
        let forced = store
            .get_or_create_with_legacy(
                &source,
                Some(("cli:forced", "cli")),
                true,
                true,
                3600.0,
                |_| panic!("force-new must skip probes"),
            )
            .unwrap();
        assert_ne!(forced.session_id, "cli:forced");
        assert!(db.get_session("cli:forced").unwrap().unwrap()["session_key"].is_null());
        drop(store);
        drop(db);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn live_transition_creates_resumes_heals_and_survives_restart() {
        let home = transition_home("live");
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            ..Default::default()
        };
        let store = SessionStore::open(
            config.clone(),
            home.clone(),
            home.clone(),
            "default".into(),
            |_| Ok(false),
        )
        .unwrap();
        let source = SessionSource::new("telegram", "C");
        let first = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        let db = store.databases.routing().unwrap();
        assert_eq!(
            db.get_session(&first.session_id).unwrap().unwrap()["session_key"],
            first.session_key
        );
        let reused = store
            .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
            .unwrap();
        assert_eq!(reused.session_id, first.session_id);
        assert_eq!(reused.updated_at.isoformat(), first.updated_at.isoformat());
        db.end_session(&first.session_id, "agent_close").unwrap();
        let resumed = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        assert_eq!(resumed.session_id, first.session_id);
        assert!(db.get_session(&first.session_id).unwrap().unwrap()["ended_at"].is_null());
        db.end_session(&first.session_id, "compression").unwrap();
        db.create_session(
            "compressed",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "telegram",
                    ..Default::default()
                },
                parent_session_id: Some(&first.session_id),
                ..Default::default()
            },
        )
        .unwrap();
        let healed = store
            .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
            .unwrap();
        assert_eq!(healed.session_id, "compressed");
        drop(store);
        let restarted =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        assert_eq!(
            restarted
                .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
                .unwrap()
                .session_id,
            "compressed"
        );
        let forced = restarted
            .get_or_create_session(&source, true, true, 3600.0, |_| {
                panic!("force-new must not evaluate reset policy")
            })
            .unwrap();
        assert_ne!(forced.session_id, "compressed");
        assert!(db.get_session(&forced.session_id).unwrap().is_some());
        drop(restarted);
        drop(db);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn activity_updated_during_process_probe_prevents_idle_reset() {
        let home = transition_home("probe_activity");
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            default_reset_policy: crate::config_types::SessionResetPolicy {
                mode: serde_json::json!("idle"),
                idle_minutes: serde_json::json!(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        let source = SessionSource::new("telegram", "C");
        let first = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        store
            .index
            .lock()
            .unwrap()
            .entries
            .get_mut(&first.session_key)
            .unwrap()
            .updated_at =
            crate::session_entry::EntryTimestamp::parse(&serde_json::json!("2000-01-01")).unwrap();

        // A finishing turn can touch activity while the resolver is outside
        // the index lock. Python's entry reference sees that write before its
        // idle-policy check; a detached Rust snapshot must not lose it.
        let resumed = store
            .get_or_create_session(&source, false, false, 3600.0, |key| {
                store.update_session(key, None, true)?;
                Ok(false)
            })
            .unwrap();
        assert_eq!(resumed.session_id, first.session_id);
        assert_ne!(resumed.updated_at.isoformat(), "2000-01-01T00:00:00");
        let row = store
            .database_for_key(&first.session_key)
            .unwrap()
            .get_session(&first.session_id)
            .unwrap()
            .unwrap();
        assert!(row["end_reason"].is_null());
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn live_idle_reset_preserves_predecessor_and_persists_boundary() {
        let home = transition_home("idle");
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            default_reset_policy: crate::config_types::SessionResetPolicy {
                mode: serde_json::json!("idle"),
                idle_minutes: serde_json::json!(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        let source = SessionSource::new("telegram", "C");
        let first = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        {
            let mut index = store.index.lock().unwrap();
            let entry = index.entries.get_mut(&first.session_key).unwrap();
            entry.updated_at =
                crate::session_entry::EntryTimestamp::parse(&serde_json::json!("2000-01-01"))
                    .unwrap();
            entry
                .fields
                .insert("last_prompt_tokens".into(), serde_json::json!(9));
        }
        let next = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        assert_ne!(next.session_id, first.session_id);
        assert_eq!(next.fields["prev_session_id"], first.session_id);
        assert_eq!(next.fields["auto_reset_reason"], "idle");
        assert_eq!(next.fields["reset_had_activity"], true);
        assert_eq!(
            store.take_auto_reset_predecessor(&next).unwrap(),
            Some(first.session_id.clone())
        );
        assert_eq!(store.take_auto_reset_predecessor(&next).unwrap(), None);
        assert_eq!(
            store.index.lock().unwrap().entries[&next.session_key].fields["was_auto_reset"],
            false
        );
        let db = store.databases.routing().unwrap();
        assert_eq!(
            db.get_session(&first.session_id).unwrap().unwrap()["end_reason"],
            "idle"
        );
        let row = db.get_session(&next.session_id).unwrap().unwrap();
        assert_eq!(row["parent_session_id"], first.session_id);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(row["model_config"].as_str().unwrap())
                .unwrap()["_reset_from"],
            first.session_id
        );
        drop(db);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn explicit_reset_is_cas_fenced_and_persists_lineage() {
        let home = transition_home("explicit_reset");
        let store = SessionStore::open(
            GatewayConfig {
                sessions_dir: home.join("sessions"),
                ..Default::default()
            },
            home.clone(),
            home.clone(),
            "default".into(),
            |_| Ok(false),
        )
        .unwrap();
        let source = SessionSource::new("telegram", "C");
        let first = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        let reset = store.reset_session(&source, Some(&first)).unwrap().unwrap();
        assert_ne!(reset.entry.session_id, first.session_id);
        assert_eq!(
            reset.predecessor_id.as_deref(),
            Some(first.session_id.as_str())
        );
        assert_eq!(reset.entry.fields["is_fresh_reset"], true);
        assert_eq!(reset.entry.fields["was_auto_reset"], false);

        // Replaying a command against the stale observed generation must not
        // overwrite the route that already won.
        assert!(store
            .reset_session(&source, Some(&first))
            .unwrap()
            .is_none());
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            reset.entry.session_id
        );

        let db = store.database_for_key(&reset.entry.session_key).unwrap();
        assert_eq!(
            db.get_session(&first.session_id).unwrap().unwrap()["end_reason"],
            "session_reset"
        );
        let child = db.get_session(&reset.entry.session_id).unwrap().unwrap();
        assert_eq!(child["parent_session_id"], first.session_id);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(child["model_config"].as_str().unwrap())
                .unwrap()["_reset_from"],
            first.session_id
        );
        let connection = rusqlite::Connection::open(home.join("state.db")).unwrap();
        let generation: i64 = connection
            .query_row(
                "SELECT generation FROM conversation_generations WHERE source=? AND session_key=?",
                rusqlite::params!["telegram", reset.entry.session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(generation, 1);
        drop(connection);
        drop(db);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn explicit_reset_on_an_empty_route_creates_no_phantom_parent() {
        let home = transition_home("explicit_reset_empty");
        let store = SessionStore::open(
            GatewayConfig {
                sessions_dir: home.join("sessions"),
                ..Default::default()
            },
            home.clone(),
            home.clone(),
            "default".into(),
            |_| Ok(false),
        )
        .unwrap();
        let source = SessionSource::new("local", "first-command");
        let reset = store.reset_session(&source, None).unwrap().unwrap();
        assert!(reset.predecessor_id.is_none());
        assert_eq!(reset.entry.fields["is_fresh_reset"], true);
        let row = store
            .database_for_key(&reset.entry.session_key)
            .unwrap()
            .get_session(&reset.entry.session_id)
            .unwrap()
            .unwrap();
        assert!(row["parent_session_id"].is_null());
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn explicit_resume_commits_route_boundary_and_reopen_together() {
        let home = transition_home("explicit_resume");
        let store = SessionStore::open(
            GatewayConfig {
                sessions_dir: home.join("sessions"),
                ..Default::default()
            },
            home.clone(),
            home.clone(),
            "default".into(),
            |_| Ok(false),
        )
        .unwrap();
        let mut source = SessionSource::new("telegram", "C");
        source.user_id = Some("U".into());
        let current = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        let db = store.database_for_key(&current.session_key).unwrap();
        db.create_session(
            "old-target",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "telegram",
                    session_key: Some(&current.session_key),
                    user_id: Some("U"),
                    chat_id: Some("C"),
                    chat_type: Some("dm"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        db.end_session("old-target", "agent_close").unwrap();

        let switched = store
            .switch_session(&source, &current, "old-target")
            .unwrap()
            .unwrap();
        assert_eq!(switched.entry.session_id, "old-target");
        assert_eq!(switched.predecessor_id, current.session_id);
        assert_eq!(
            store.current_entry_for_source(&source).unwrap().session_id,
            "old-target"
        );
        assert_eq!(
            db.get_session(&current.session_id).unwrap().unwrap()["end_reason"],
            "session_switch"
        );
        let target = db.get_session("old-target").unwrap().unwrap();
        assert!(target["ended_at"].is_null());
        assert_eq!(target["session_key"], current.session_key);
        assert!(store
            .switch_session(&source, &current, &current.session_id)
            .unwrap()
            .is_none());
        let live = store.current_entry_for_source(&source).unwrap();
        assert!(store
            .switch_session(&source, &live, "missing")
            .unwrap()
            .is_none());
        drop(db);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn expiry_candidates_follow_policy_and_finalize_both_durable_copies() {
        let home = transition_home("expiry");
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            default_reset_policy: crate::config_types::SessionResetPolicy {
                mode: serde_json::json!("idle"),
                idle_minutes: serde_json::json!(30),
                ..Default::default()
            },
            ..Default::default()
        };
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        let source = SessionSource::new("telegram", "C");
        let entry = store
            .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
            .unwrap();
        assert!(store.is_session_finalizable(&entry));
        {
            let mut index = store.index.lock().unwrap();
            let current = index.entries.get_mut(&entry.session_key).unwrap();
            current.updated_at = crate::session_entry::EntryTimestamp::parse(&serde_json::json!(
                "2026-09-08T00:00:00"
            ))
            .unwrap();
            current.fields.insert(
                "model_override".into(),
                serde_json::json!({"model":"temporary"}),
            );
        }
        let before = chrono::NaiveDate::from_ymd_opt(2026, 9, 8)
            .unwrap()
            .and_hms_opt(0, 29, 59)
            .unwrap();
        assert!(store.expired_sessions_at(before).is_empty());
        let after = chrono::NaiveDate::from_ymd_opt(2026, 9, 8)
            .unwrap()
            .and_hms_opt(0, 30, 1)
            .unwrap();
        let expired = store.expired_sessions_at(after);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].session_id, entry.session_id);
        store.finalize_expired_session(&expired[0]).unwrap();
        assert!(store.expired_sessions_at(after).is_empty());
        let current = store
            .index
            .lock()
            .unwrap()
            .entries
            .get(&entry.session_key)
            .unwrap()
            .to_dict();
        assert_eq!(current["expiry_finalized"], true);
        assert!(current.get("model_override").is_none());
        let row = store
            .database_for_key(&entry.session_key)
            .unwrap()
            .get_session(&entry.session_id)
            .unwrap()
            .unwrap();
        assert_eq!(row["expiry_finalized"], 1);
        assert_eq!(row["end_reason"], "session_reset");
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn mode_none_sessions_are_not_finalizable_or_expirable() {
        let home = transition_home("expiry_none");
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            default_reset_policy: crate::config_types::SessionResetPolicy {
                mode: serde_json::json!("none"),
                ..Default::default()
            },
            ..Default::default()
        };
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        let entry = store
            .get_or_create_session(
                &SessionSource::new("telegram", "C"),
                false,
                false,
                3600.0,
                |_| Ok(false),
            )
            .unwrap();
        assert!(!store.is_session_finalizable(&entry));
        assert!(store
            .expired_sessions_at(chrono::Local::now().naive_local() + chrono::TimeDelta::days(365))
            .is_empty());
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn live_slack_migration_moves_unscoped_dm_and_retains_metadata() {
        let home = transition_home("slack_migration");
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            ..Default::default()
        };
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        let mut source = SessionSource::new("slack", "C");
        let original = store
            .get_or_create_session(&source, false, true, 3600.0, |_| Ok(false))
            .unwrap();
        store
            .index
            .lock()
            .unwrap()
            .entries
            .get_mut(&original.session_key)
            .unwrap()
            .fields
            .insert("metadata".into(), serde_json::json!({"queued":true}));
        source.scope_id = Some("T".into());
        let migrated = store
            .get_or_create_session(&source, false, false, 3600.0, |_| Ok(false))
            .unwrap();
        assert_eq!(migrated.session_id, original.session_id);
        assert_ne!(migrated.session_key, original.session_key);
        assert_eq!(migrated.fields["metadata"]["queued"], true);
        assert!(!store
            .index
            .lock()
            .unwrap()
            .entries
            .contains_key(&original.session_key));
        let db = store.databases.routing().unwrap();
        assert_eq!(
            db.get_session(&original.session_id).unwrap().unwrap()["session_key"],
            migrated.session_key
        );
        drop(db);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn metadata_update_preserves_internal_activity_clock_and_repairs_peer() {
        let home = std::env::temp_dir().join(format!(
            "hermes-store-update-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = GatewayConfig {
            sessions_dir: home.join("sessions"),
            ..Default::default()
        };
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        let source = SessionSource::new("telegram", "C");
        let key = "agent:main:telegram:dm:C";
        let old = chrono::NaiveDateTime::parse_from_str("2000-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S")
            .unwrap();
        let entry =
            SessionEntry::new_candidate(key, &source, old, &CreationContext::default()).unwrap();
        let id = entry.session_id.clone();
        store.index.lock().unwrap().publish_candidate(entry);
        store
            .update_session(key, Some(serde_json::json!(42)), false)
            .unwrap();
        let (scope, first) = {
            let index = store.index.lock().unwrap();
            (index.scope().to_owned(), index.entries[key].to_dict())
        };
        assert_eq!(first["updated_at"], "2000-01-01T00:00:00");
        assert_eq!(first["last_prompt_tokens"], 42);
        let db = store.databases.routing().unwrap();
        let rows = db.load_gateway_routing_entries(&scope).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&rows[key]).unwrap(),
            first
        );
        let row = db.get_session(&id).unwrap().unwrap();
        assert_eq!(row["session_key"], key);
        assert_eq!(row["chat_id"], "C");
        store
            .update_session(key, Some(serde_json::Value::Null), true)
            .unwrap();
        let index = store.index.lock().unwrap();
        assert_eq!(index.entries[key].fields["last_prompt_tokens"], 42);
        assert!(index.entries[key].updated_at.local > old);
        drop(index);
        store
            .update_session("missing", Some(serde_json::json!(99)), true)
            .unwrap();
        assert!(!store.index.lock().unwrap().entries.contains_key("missing"));
        drop(db);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn startup_recovers_accidental_closure_and_preserves_missing_profile_route() {
        let home = std::env::temp_dir().join(format!(
            "hermes-store-startup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sessions = home.join("sessions");
        let db = SessionDb::open_shared(home.join("state.db")).unwrap();
        let source = SessionSource::new("telegram", "C");
        let key = "agent:main:telegram:dm:C";
        db.create_session(
            "persisted",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "telegram",
                    session_key: Some(key),
                    chat_id: Some("C"),
                    chat_type: Some("dm"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        db.end_session("persisted", "agent_close").unwrap();
        let mut index = RoutingIndex::new(sessions.clone()).unwrap();
        index.ensure_loaded(Some(&db));
        let mut entry = SessionEntry::new_candidate(
            key,
            &source,
            chrono::Local::now().naive_local(),
            &CreationContext::default(),
        )
        .unwrap();
        entry.session_id = "persisted".into();
        entry
            .fields
            .insert("metadata".into(), serde_json::json!({"queued":true}));
        let original = entry.to_dict();
        index.entries.insert(key.into(), entry.clone());
        entry.session_key = "agent:unprovisioned:telegram:dm:C".into();
        index.entries.insert(entry.session_key.clone(), entry);
        index.save(Some(&db), true).unwrap();
        drop(index);
        let config = GatewayConfig {
            sessions_dir: sessions,
            multiplex_profiles: true,
            ..Default::default()
        };
        let store =
            SessionStore::open(config, home.clone(), home.clone(), "default".into(), |_| {
                Ok(false)
            })
            .unwrap();
        let index = store.index.lock().unwrap();
        assert_eq!(index.entries[key].to_dict(), original);
        assert!(index
            .entries
            .contains_key("agent:unprovisioned:telegram:dm:C"));
        assert!(db.get_session("persisted").unwrap().unwrap()["ended_at"].is_null());
        assert!(!home.join("profiles/unprovisioned").exists());
        drop(index);
        drop(store);
        drop(db);
        std::fs::remove_dir_all(home).unwrap();
    }
}
