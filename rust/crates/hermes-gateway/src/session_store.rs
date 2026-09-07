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

struct SessionOrigin<'a> {
    source: &'a crate::session::SessionSource,
    legacy: Option<(&'a str, &'a str)>,
}

impl SessionStore {
    pub fn database_for_key(&self, key: &str) -> Option<Arc<crate::session_db::SessionDb>> {
        self.databases.for_key(key, &self.home)
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
        let profile = self.config.multiplex_profiles.then(|| {
            source
                .profile
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.active_profile)
        });
        let key = crate::session::build_session_key(
            source,
            self.config.group_sessions_per_user,
            self.config.thread_sessions_per_user,
            profile,
        );
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
