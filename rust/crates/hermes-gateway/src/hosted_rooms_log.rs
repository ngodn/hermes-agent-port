//! Port of the room-log authority layer of gateway/hosted_rooms.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! This is the append-only room-log and authority slice of hosted_rooms.py:
//! hosted-room identity plus its ordered event log, the sibling tables the log
//! transaction touches, and the validation/connection helpers the sibling
//! modules (`hosted_room_replicas`, `hosted_room_policy_checkpoint`,
//! `hosted_room_discussion`) import from `gateway.hosted_rooms`. It ports the
//! full shared SQLite schema (all seven `hosted_room*` tables plus the
//! `idx_hosted_room_events_cursor` index and the remote-run PK migration), the
//! identifier/name/member/actor/JSON validators, the WAL connection + BEGIN
//! IMMEDIATE transaction helpers, `create_room`, `append_event` (idempotent
//! ingest, authority-epoch fencing, capacity accounting) and `read_events`
//! (monotonic cursor deltas with page-byte trimming). The room-LINK record
//! store slice (`hosted_room_links` list/upsert/status) lives separately in
//! `hosted_rooms.rs` and is not duplicated here; this module only (re)creates
//! the shared `hosted_room_links` table as part of the byte-for-byte schema so
//! the shared `state.db` opens identically from either module. Serialized JSON
//! columns (members, actor, payload, catalog) stay as `serde_json::Value` /
//! `String`, exactly as Python stores them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use fancy_regex::Regex;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Constants (mirror the module-level constants in gateway/hosted_rooms.py).
// ---------------------------------------------------------------------------

pub const PROTOCOL_VERSION: i64 = 2;
pub const MAX_ROOM_ID_CHARS: usize = 128;
pub const MAX_EVENT_ID_CHARS: usize = 128;
pub const MAX_ROOM_NAME_CHARS: usize = 200;
pub const MAX_EVENT_KIND_CHARS: usize = 64;
pub const MAX_ACTOR_ID_CHARS: usize = 128;
pub const MAX_ACTOR_LABEL_CHARS: usize = 200;
pub const MAX_MEMBERS: usize = 128;
pub const MAX_MEMBERS_JSON_BYTES: usize = 128 * 1024;
pub const MAX_EVENT_JSON_BYTES: usize = 256 * 1024;
pub const MAX_LOG_LIMIT: i64 = 500;
pub const MAX_LOG_PAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ROOM_LIST_LIMIT: i64 = 500;
pub const MAX_ACTIVE_ROOMS: i64 = 256;
pub const MAX_DISBANDED_ROOM_TOMBSTONES: i64 = 512;
pub const DISBANDED_ROOM_RETENTION_SECONDS: f64 = (90 * 24 * 60 * 60) as f64;
pub const MAX_EVENTS_PER_ROOM: i64 = 50_000;
pub const MAX_ROOM_EVENT_BYTES: i64 = 256 * 1024 * 1024;
// Leave substantial headroom below the pre-update state.db snapshot ceiling.
pub const MAX_GATEWAY_EVENT_BYTES: i64 = 16 * 1024 * 1024;
pub const CONTROL_EVENT_COUNT_RESERVE: i64 = 64;
pub const CONTROL_EVENT_BYTE_RESERVE: i64 = 1024 * 1024;

// Actor JSON has its own small ceiling everywhere it is canonicalized.
const ACTOR_JSON_MAX_BYTES: usize = 4 * 1024;

// Columns _schema_is_current requires to be present on each table.
const ROOM_SCHEMA_COLUMNS: &[&str] = &[
    "room_id",
    "name",
    "members_json",
    "authority_gateway_id",
    "authority_epoch",
    "next_seq",
    "event_bytes",
    "revision",
    "created_at",
    "updated_at",
    "disbanded_at",
];
const EVENT_SCHEMA_COLUMNS: &[&str] = &[
    "room_id",
    "seq",
    "event_id",
    "kind",
    "actor_json",
    "authority_epoch",
    "payload_json",
    "created_at",
];
const RETIRED_ROOM_SCHEMA_COLUMNS: &[&str] = &["room_id", "retired_at"];
const LINK_SCHEMA_COLUMNS: &[&str] = &[
    "room_id",
    "member_id",
    "target_url",
    "target_profile",
    "grant",
    "catalog_json",
    "cancellation_scope_id",
    "trace_id",
    "transport_security",
    "status",
    "updated_at",
];
const REMOTE_RUN_SCHEMA_COLUMNS: &[&str] = &[
    "room_id",
    "home_install_id",
    "authority_gateway_id",
    "authority_epoch",
    "member_id",
    "task_id",
    "execution_generation",
    "target_install_id",
    "target_profile",
    "run_id",
    "session_id",
    "created_at",
    "updated_at",
];
// Order matters: this is the exact remote-run primary key _schema_is_current
// and _migrate_remote_run_schema compare against.
const REMOTE_RUN_IDENTITY_COLUMNS: &[&str] = &[
    "room_id",
    "home_install_id",
    "authority_gateway_id",
    "authority_epoch",
    "member_id",
    "target_install_id",
    "target_profile",
    "task_id",
    "execution_generation",
];
const REVOKED_GRANT_SCHEMA_COLUMNS: &[&str] = &["scope_key", "expires_at", "revoked_before"];
const PEER_RESERVATION_SCHEMA_COLUMNS: &[&str] = &[
    "room_id",
    "member_id",
    "target_profile",
    "authority_gateway_id",
    "authority_epoch",
    "expires_at",
    "revoked_at",
    "created_at",
    "updated_at",
];

/// Actor kinds and the event kinds each may append. Mirrors
/// `_EVENT_KINDS_BY_ACTOR`.
fn event_kinds_for_actor(actor_kind: &str) -> Option<&'static [&'static str]> {
    match actor_kind {
        "user" => Some(&["message.user"]),
        "member" => Some(&["message.member"]),
        "gateway" => Some(&[
            "member.unavailable",
            "room.activity",
            "room.stop_requested",
            "turn.deferred",
            "turn.reassigned",
            "turn.cancelled",
            "turn.failed",
            "turn.settled",
            "turn.started",
        ]),
        "system" => Some(&[
            "authority.claimed",
            "authority.lost",
            "room.created",
            "room.disbanded",
            "room.members_changed",
            "room.renamed",
        ]),
        _ => None,
    }
}

const ACTOR_FIELDS: &[&str] = &["kind", "id", "display_name", "profile", "connection_id"];

fn identifier_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // \A..\z gives Python `fullmatch` semantics (no trailing-newline slack that
    // a bare `$` would allow). The character classes match _IDENTIFIER_RE.
    RE.get_or_init(|| Regex::new(r"\A[A-Za-z0-9][A-Za-z0-9._:-]*\z").unwrap())
}

fn event_kind_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\A[a-z][a-z0-9_.-]*\z").unwrap())
}

// ---------------------------------------------------------------------------
// Errors (mirror the HostedRoomError exception hierarchy).
// ---------------------------------------------------------------------------

/// One error from the room-log layer. Variants mirror the Python exception
/// subclasses; `Invalid` is the base `HostedRoomError` (a `ValueError`). The
/// subclass relationships that Python callers reach via `isinstance` are
/// exposed through [`HostedRoomError::is_room_not_found`] and
/// [`HostedRoomError::is_authority_conflict`], and the `.reason` attributes
/// through [`HostedRoomError::reason`].
#[derive(Debug)]
pub enum HostedRoomError {
    /// Base `HostedRoomError` / `ValueError` with a message.
    Invalid(String),
    /// `RoomNotFoundError`.
    RoomNotFound(String),
    /// `RoomHistoryExpiredError` (a `RoomNotFoundError`, reason
    /// "room_history_expired").
    RoomHistoryExpired(String),
    /// `RoomConflictError`.
    RoomConflict(String),
    /// `RoomProbeUnavailableError`.
    RoomProbeUnavailable(String),
    /// `EventConflictError`.
    EventConflict(String),
    /// `AuthorityConflictError` (reason "authority_conflict").
    AuthorityConflict(String),
    /// `AuthoritySupersededError` (an `AuthorityConflictError`).
    AuthoritySuperseded(String),
    /// Any underlying SQLite failure, surfaced best-effort.
    Sqlite(rusqlite::Error),
}

impl HostedRoomError {
    /// True for `RoomNotFoundError` and its subclass `RoomHistoryExpiredError`,
    /// matching a Python `isinstance(exc, RoomNotFoundError)` check.
    pub fn is_room_not_found(&self) -> bool {
        matches!(
            self,
            HostedRoomError::RoomNotFound(_) | HostedRoomError::RoomHistoryExpired(_)
        )
    }

    /// True for `AuthorityConflictError` and its subclass
    /// `AuthoritySupersededError`.
    pub fn is_authority_conflict(&self) -> bool {
        matches!(
            self,
            HostedRoomError::AuthorityConflict(_) | HostedRoomError::AuthoritySuperseded(_)
        )
    }

    /// The Python `.reason` attribute where one exists, else None.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            HostedRoomError::RoomHistoryExpired(_) => Some("room_history_expired"),
            HostedRoomError::AuthorityConflict(_) | HostedRoomError::AuthoritySuperseded(_) => {
                Some("authority_conflict")
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for HostedRoomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostedRoomError::Invalid(m)
            | HostedRoomError::RoomNotFound(m)
            | HostedRoomError::RoomHistoryExpired(m)
            | HostedRoomError::RoomConflict(m)
            | HostedRoomError::RoomProbeUnavailable(m)
            | HostedRoomError::EventConflict(m)
            | HostedRoomError::AuthorityConflict(m)
            | HostedRoomError::AuthoritySuperseded(m) => f.write_str(m),
            HostedRoomError::Sqlite(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for HostedRoomError {}

impl From<rusqlite::Error> for HostedRoomError {
    fn from(err: rusqlite::Error) -> Self {
        HostedRoomError::Sqlite(err)
    }
}

type Result<T> = std::result::Result<T, HostedRoomError>;

fn invalid(msg: impl Into<String>) -> HostedRoomError {
    HostedRoomError::Invalid(msg.into())
}

// ---------------------------------------------------------------------------
// Output structs (mirror the dicts _room_from_row / _event_from_row build).
// ---------------------------------------------------------------------------

/// One durable event row. Mirrors the dict from `_event_from_row`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub room_id: String,
    pub seq: i64,
    pub event_id: String,
    pub kind: String,
    pub actor: Value,
    pub authority_epoch: Option<i64>,
    pub payload: Value,
    pub created_at: f64,
    pub idempotent: bool,
}

/// Room identity/authority state. Mirrors the dict from `_room_from_row`
/// (plus the create-time `adopted`/`claim_event` extras). `latest_seq` and
/// `disbanded_at` are `None` where the Python dict would omit the key.
#[derive(Debug, Clone, PartialEq)]
pub struct Room {
    pub room_id: String,
    pub name: String,
    pub members: Value,
    pub authority_gateway_id: String,
    pub authority_epoch: i64,
    pub revision: i64,
    pub created_at: f64,
    pub updated_at: f64,
    pub idempotent: bool,
    pub disbanded_at: Option<f64>,
    pub latest_seq: Option<i64>,
    pub adopted: bool,
    pub claim_event: Option<Event>,
}

/// One bounded monotonic page from `read_events`.
#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    pub events: Vec<Event>,
    pub cursor: i64,
    pub latest_seq: i64,
    pub has_more: bool,
    pub authority_gateway_id: String,
    pub authority_epoch: i64,
}

// ---------------------------------------------------------------------------
// Time + install identity.
// ---------------------------------------------------------------------------

/// Seconds since the Unix epoch, matching Python's `time.time()`.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Stable install id for this host.
///
/// Mirrors the Python `hermes_cli.install_identity.get_install_id`: the stable
/// opaque id for this physical install (or `None` when it cannot be read or
/// minted, so [`local_authority_gateway_id`] fails closed exactly like Python).
fn install_id() -> Option<String> {
    crate::install_identity::get_install_id()
}

/// The stable server-owned identity for hosted-room authority
/// (`install:<install_id>`). Mirrors `local_authority_gateway_id`.
pub fn local_authority_gateway_id() -> Result<String> {
    let id = install_id()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("stable gateway install identity is unavailable"))?;
    validate_identifier(
        &format!("install:{id}"),
        "authority_gateway_id",
        MAX_ACTOR_ID_CHARS,
    )
}

// ---------------------------------------------------------------------------
// Validation helpers.
// ---------------------------------------------------------------------------

/// Canonical JSON: sorted keys, compact separators, no ASCII escaping, with a
/// byte-length ceiling. Mirrors `_canonical_json`. serde_json's default `Map`
/// is a `BTreeMap` (no `preserve_order` feature enabled in this workspace), so
/// `to_string` already emits sorted keys with `(",", ":")` separators and
/// leaves non-ASCII as UTF-8, matching Python's
/// `sort_keys=True, separators=(",",":"), ensure_ascii=False`.
pub fn canonical_json(value: &Value, label: &str, max_bytes: usize) -> Result<String> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| invalid(format!("{label} must be JSON-serializable")))?;
    if encoded.len() > max_bytes {
        return Err(invalid(format!("{label} is too large")));
    }
    Ok(encoded)
}

/// Validate and normalize an identifier. Length is counted in Unicode code
/// points to match Python `len(str)`. Mirrors `_validate_identifier`.
pub fn validate_identifier(value: &str, label: &str, max_chars: usize) -> Result<String> {
    let trimmed = value.trim();
    let too_long = trimmed.chars().count() > max_chars;
    let matches = identifier_re().is_match(trimmed).unwrap_or(false);
    if trimmed.is_empty() || too_long || !matches {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(trimmed.to_string())
}

/// Map a client retry key into the server-owned user-event namespace
/// (`user:<sha256 hex>`). Mirrors `user_event_id`.
pub fn user_event_id(client_event_id: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let normalized = validate_identifier(client_event_id, "event_id", MAX_EVENT_ID_CHARS)?;
    let digest = Sha256::digest(normalized.as_bytes());
    Ok(format!("user:{:x}", digest))
}

/// Validate and normalize a room name. Mirrors `_validate_room_name`.
pub fn validate_room_name(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_ROOM_NAME_CHARS {
        return Err(invalid("invalid room name"));
    }
    Ok(trimmed.to_string())
}

/// Validate a members list and return the normalized list plus its canonical
/// JSON. Mirrors `_validate_members`: each member must be a JSON object, the
/// list is size-capped, and the canonical encoding is byte-capped.
pub fn validate_members(value: &Value) -> Result<(Vec<Value>, String)> {
    let list = value
        .as_array()
        .ok_or_else(|| invalid("members must be a list"))?;
    if list.len() > MAX_MEMBERS {
        return Err(invalid("too many room members"));
    }
    let mut members: Vec<Value> = Vec::with_capacity(list.len());
    for member in list {
        if !member.is_object() {
            return Err(invalid("each room member must be an object"));
        }
        members.push(member.clone());
    }
    let encoded = canonical_json(
        &Value::Array(members.clone()),
        "members",
        MAX_MEMBERS_JSON_BYTES,
    )?;
    Ok((members, encoded))
}

/// Validate and normalize an event kind. Mirrors `_validate_event_kind`.
fn validate_event_kind(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let too_long = trimmed.chars().count() > MAX_EVENT_KIND_CHARS;
    let matches = event_kind_re().is_match(trimmed).unwrap_or(false);
    if trimmed.is_empty() || too_long || !matches {
        return Err(invalid("invalid event kind"));
    }
    Ok(trimmed.to_string())
}

/// Read one optional string actor field. Mirrors `_optional_actor_field`:
/// absent/null -> "", must be a string, length in code points.
fn optional_actor_field(
    actor: &Map<String, Value>,
    field: &str,
    max_chars: usize,
) -> Result<String> {
    match actor.get(field) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.chars().count() > max_chars {
                return Err(invalid(format!("actor.{field} is too long")));
            }
            Ok(trimmed.to_string())
        }
        Some(_) => Err(invalid(format!("actor.{field} must be a string"))),
    }
}

/// Validate an actor object for a given event kind. Returns the normalized
/// actor and its canonical JSON. Mirrors `_validate_actor`.
fn validate_actor(value: &Value, kind: &str) -> Result<(Value, String)> {
    let actor = value
        .as_object()
        .ok_or_else(|| invalid("actor must be an object"))?;

    let allowed: HashSet<&str> = ACTOR_FIELDS.iter().copied().collect();
    let mut unknown: Vec<String> = actor
        .keys()
        .filter(|k| !allowed.contains(k.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(invalid(format!(
            "unknown actor fields: {}",
            unknown.join(", ")
        )));
    }

    let actor_kind = match actor.get("kind") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(invalid("invalid actor.kind")),
    };
    let permitted = match event_kinds_for_actor(&actor_kind) {
        Some(kinds) => kinds,
        None => return Err(invalid("invalid actor.kind")),
    };
    if !permitted.contains(&kind) {
        return Err(invalid(format!(
            "actor kind '{actor_kind}' cannot append '{kind}'"
        )));
    }

    let id_value = actor.get("id").cloned().unwrap_or(Value::Null);
    let id_str = match &id_value {
        Value::String(s) => s.clone(),
        // Non-string id: _validate_identifier raises "must be a string" first,
        // but the public message contract collapses to invalid actor.id.
        _ => return Err(invalid("invalid actor.id")),
    };
    let actor_id = validate_identifier(&id_str, "actor.id", MAX_ACTOR_ID_CHARS)?;

    let mut normalized = Map::new();
    normalized.insert("kind".to_string(), Value::String(actor_kind));
    normalized.insert("id".to_string(), Value::String(actor_id));
    for (field, max_chars) in [
        ("display_name", MAX_ACTOR_LABEL_CHARS),
        ("profile", MAX_ACTOR_ID_CHARS),
        ("connection_id", MAX_ACTOR_ID_CHARS),
    ] {
        let field_value = optional_actor_field(actor, field, max_chars)?;
        if !field_value.is_empty() {
            normalized.insert(field.to_string(), Value::String(field_value));
        }
    }
    let normalized_value = Value::Object(normalized);
    let encoded = canonical_json(&normalized_value, "actor", ACTOR_JSON_MAX_BYTES)?;
    Ok((normalized_value, encoded))
}

// ---------------------------------------------------------------------------
// Connection + schema.
// ---------------------------------------------------------------------------

/// Production room store path: `$HERMES_HOME/state.db`, the same root database
/// the Python gateway uses. Callers may pass any path to isolate tests, exactly
/// like the Python functions that take an explicit `db_path`.
///
/// The link-store sibling (`hosted_rooms.rs`) resolves the same path the same
/// way, so both modules open one shared `state.db`.
pub fn default_db_path() -> PathBuf {
    crate::config_file::hermes_home().join("state.db")
}

/// Open the shared rooms DB and ensure the full room-log schema.
///
/// Mirrors `_connect`: create the parent dir, open the file with a 10s busy
/// timeout, enable WAL, turn foreign keys on, and if the schema is not already
/// current run `_initialize_schema` inside one `BEGIN IMMEDIATE` DDL/data
/// transaction so a crash rolls the whole migration back. WAL is best-effort
/// here, matching the sibling ports (`hosted_rooms.rs`, `delivery_ledger.rs`):
/// a filesystem that refuses WAL falls back to the default journal mode instead
/// of failing the open. Python additionally retries the WAL pragma under a
/// transient "database is locked"; the best-effort pragma here simply ignores
/// that class, which is safe because the schema work runs under its own lock.
///
/// This is the port of Python's `_connect`, which `hosted_room_replicas`
/// imports. Its `_transaction` companion has no standalone Rust analog because a
/// rusqlite `Transaction` borrows its `Connection`; callers get the same effect
/// with `connect(db_path)?` then
/// `conn.transaction_with_behavior(TransactionBehavior::Immediate | Deferred)`,
/// exactly as the functions in this module do.
pub fn connect(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(db_path)?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(10));
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute_batch("PRAGMA foreign_keys=ON")?;
    if schema_is_current(&conn)? {
        return Ok(conn);
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match initialize_schema(&conn) {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(conn)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// Column-name set for a table.
fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut out = HashSet::new();
    for name in rows {
        out.insert(name?);
    }
    Ok(out)
}

/// Primary-key column names in key order. Mirrors `_primary_key_columns`:
/// PRAGMA table_info rows where the `pk` field is non-zero, ordered by it.
fn primary_key_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    // (pk_index, name)
    let rows = stmt.query_map([], |row| {
        let name: String = row.get(1)?;
        let pk: i64 = row.get(5)?;
        Ok((pk, name))
    })?;
    let mut cols: Vec<(i64, String)> = Vec::new();
    for row in rows {
        let (pk, name) = row?;
        if pk != 0 {
            cols.push((pk, name));
        }
    }
    cols.sort_by_key(|(pk, _)| *pk);
    Ok(cols.into_iter().map(|(_, name)| name).collect())
}

fn is_subset(required: &[&str], present: &HashSet<String>) -> bool {
    required.iter().all(|c| present.contains(*c))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let row: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            params![table],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.is_some())
}

/// Whether every ported table, the remote-run PK, and the events cursor index
/// are present. Mirrors `_schema_is_current`.
fn schema_is_current(conn: &Connection) -> Result<bool> {
    let room = table_columns(conn, "hosted_rooms")?;
    let event = table_columns(conn, "hosted_room_events")?;
    let retired = table_columns(conn, "hosted_room_retired_ids")?;
    let link = table_columns(conn, "hosted_room_links")?;
    let remote_run = table_columns(conn, "hosted_room_remote_runs")?;
    let revoked = table_columns(conn, "hosted_room_revoked_grants")?;
    let peer = table_columns(conn, "hosted_room_peer_reservations")?;

    if !is_subset(ROOM_SCHEMA_COLUMNS, &room)
        || !is_subset(EVENT_SCHEMA_COLUMNS, &event)
        || !is_subset(RETIRED_ROOM_SCHEMA_COLUMNS, &retired)
        || !is_subset(LINK_SCHEMA_COLUMNS, &link)
        || !is_subset(REMOTE_RUN_SCHEMA_COLUMNS, &remote_run)
    {
        return Ok(false);
    }
    if primary_key_columns(conn, "hosted_room_remote_runs")? != REMOTE_RUN_IDENTITY_COLUMNS {
        return Ok(false);
    }
    if !is_subset(REVOKED_GRANT_SCHEMA_COLUMNS, &revoked)
        || !is_subset(PEER_RESERVATION_SCHEMA_COLUMNS, &peer)
    {
        return Ok(false);
    }
    let index: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_hosted_room_events_cursor'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(index.is_some())
}

/// Fence legacy remote-run receipts behind the complete authority-lineage key.
/// Mirrors `_migrate_remote_run_schema`. On a freshly created DB the table is
/// already current, so this is a no-op.
fn migrate_remote_run_schema(conn: &Connection) -> Result<()> {
    let columns = table_columns(conn, "hosted_room_remote_runs")?;
    if is_subset(REMOTE_RUN_SCHEMA_COLUMNS, &columns)
        && primary_key_columns(conn, "hosted_room_remote_runs")? == REMOTE_RUN_IDENTITY_COLUMNS
    {
        return Ok(());
    }

    conn.execute_batch("DROP TABLE IF EXISTS hosted_room_remote_runs_migrating")?;
    conn.execute_batch(
        "CREATE TABLE hosted_room_remote_runs_migrating (
            room_id TEXT NOT NULL,
            home_install_id TEXT NOT NULL,
            authority_gateway_id TEXT NOT NULL,
            authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 1),
            member_id TEXT NOT NULL,
            task_id TEXT NOT NULL,
            execution_generation INTEGER NOT NULL CHECK (execution_generation >= 1),
            target_install_id TEXT NOT NULL,
            target_profile TEXT NOT NULL,
            run_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL,
            PRIMARY KEY (
                room_id, home_install_id, authority_gateway_id, authority_epoch,
                member_id, target_install_id, target_profile, task_id,
                execution_generation
            )
        )",
    )?;
    if !columns.is_empty() {
        let home = if columns.contains("home_install_id") {
            "home_install_id"
        } else {
            "'legacy'"
        };
        let gateway = if columns.contains("authority_gateway_id") {
            "authority_gateway_id"
        } else {
            "'legacy'"
        };
        let epoch = if columns.contains("authority_epoch") {
            "authority_epoch"
        } else {
            "1"
        };
        conn.execute_batch(&format!(
            "INSERT OR IGNORE INTO hosted_room_remote_runs_migrating(
                    room_id, home_install_id, authority_gateway_id,
                    authority_epoch, member_id, task_id,
                    execution_generation, target_install_id, target_profile,
                    run_id, session_id, created_at, updated_at
                )
                SELECT room_id, {home}, {gateway}, {epoch}, member_id, task_id,
                       execution_generation, target_install_id, target_profile,
                       run_id, session_id, created_at, updated_at
                  FROM hosted_room_remote_runs"
        ))?;
    }
    conn.execute_batch("DROP TABLE hosted_room_remote_runs")?;
    conn.execute_batch(
        "ALTER TABLE hosted_room_remote_runs_migrating RENAME TO hosted_room_remote_runs",
    )?;
    Ok(())
}

/// Create every room-log table, run the column/PK migrations, backfill legacy
/// data, and create the events cursor index. Mirrors `_initialize_schema`
/// statement for statement. Runs inside the caller's `BEGIN IMMEDIATE`.
fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_rooms (
            room_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            members_json TEXT NOT NULL,
            authority_gateway_id TEXT NOT NULL,
            authority_epoch INTEGER NOT NULL DEFAULT 1 CHECK (authority_epoch >= 1),
            next_seq INTEGER NOT NULL DEFAULT 1 CHECK (next_seq >= 1),
            event_bytes INTEGER NOT NULL DEFAULT 0 CHECK (event_bytes >= 0),
            revision INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL,
            disbanded_at REAL
        )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_events (
            room_id TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK (seq >= 1),
            event_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            actor_json TEXT NOT NULL,
            authority_epoch INTEGER CHECK (authority_epoch IS NULL OR authority_epoch >= 1),
            payload_json TEXT NOT NULL,
            created_at REAL NOT NULL,
            PRIMARY KEY (room_id, seq),
            UNIQUE (room_id, event_id),
            FOREIGN KEY (room_id) REFERENCES hosted_rooms(room_id)
        )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_retired_ids (
            room_id TEXT PRIMARY KEY,
            retired_at REAL NOT NULL
        )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_links (
            room_id TEXT NOT NULL,
            member_id TEXT NOT NULL,
            target_url TEXT NOT NULL,
            target_profile TEXT NOT NULL,
            grant TEXT NOT NULL,
            catalog_json TEXT NOT NULL,
            cancellation_scope_id TEXT NOT NULL,
            trace_id TEXT NOT NULL,
            transport_security TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'ready',
            updated_at REAL NOT NULL,
            PRIMARY KEY (room_id, member_id)
        )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_remote_runs (
            room_id TEXT NOT NULL,
            home_install_id TEXT NOT NULL,
            authority_gateway_id TEXT NOT NULL,
            authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 1),
            member_id TEXT NOT NULL,
            task_id TEXT NOT NULL,
            execution_generation INTEGER NOT NULL CHECK (execution_generation >= 1),
            target_install_id TEXT NOT NULL,
            target_profile TEXT NOT NULL,
            run_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL,
            PRIMARY KEY (
                room_id, home_install_id, authority_gateway_id, authority_epoch,
                member_id, target_install_id, target_profile, task_id,
                execution_generation
            )
        )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_revoked_grants (
            scope_key TEXT PRIMARY KEY,
            expires_at REAL NOT NULL,
            revoked_before REAL NOT NULL
        )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_peer_reservations (
            room_id TEXT NOT NULL,
            member_id TEXT NOT NULL,
            target_profile TEXT NOT NULL,
            authority_gateway_id TEXT NOT NULL,
            authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 1),
            expires_at REAL NOT NULL,
            revoked_at REAL,
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL,
            PRIMARY KEY (room_id, member_id, target_profile)
        )",
    )?;

    let room_columns = table_columns(conn, "hosted_rooms")?;
    if !room_columns.contains("authority_gateway_id") {
        conn.execute_batch(
            "ALTER TABLE hosted_rooms ADD COLUMN authority_gateway_id TEXT NOT NULL DEFAULT 'legacy'",
        )?;
    }
    if !room_columns.contains("authority_epoch") {
        conn.execute_batch(
            "ALTER TABLE hosted_rooms ADD COLUMN authority_epoch INTEGER NOT NULL DEFAULT 1",
        )?;
    }
    let backfill_event_bytes = !room_columns.contains("event_bytes");
    if backfill_event_bytes {
        conn.execute_batch(
            "ALTER TABLE hosted_rooms ADD COLUMN event_bytes INTEGER NOT NULL DEFAULT 0",
        )?;
    }

    let event_columns = table_columns(conn, "hosted_room_events")?;
    if !event_columns.contains("actor_json") {
        // Draft builds before the actor contract carried no identity. Preserve
        // their inert replay rows explicitly as legacy system events.
        let legacy_actor = canonical_json(
            &serde_json::json!({"kind": "system", "id": "legacy"}),
            "actor",
            ACTOR_JSON_MAX_BYTES,
        )?;
        let escaped_actor = legacy_actor.replace('\'', "''");
        conn.execute_batch(&format!(
            "ALTER TABLE hosted_room_events ADD COLUMN actor_json TEXT NOT NULL DEFAULT '{escaped_actor}'"
        ))?;
    }
    if !event_columns.contains("authority_epoch") {
        conn.execute_batch("ALTER TABLE hosted_room_events ADD COLUMN authority_epoch INTEGER")?;
    }
    if backfill_event_bytes {
        conn.execute_batch(
            "UPDATE hosted_rooms
                  SET event_bytes=COALESCE((
                      SELECT SUM(
                          length(CAST(event_id AS BLOB)) +
                          length(CAST(kind AS BLOB)) +
                          length(CAST(actor_json AS BLOB)) +
                          length(CAST(payload_json AS BLOB))
                      )
                      FROM hosted_room_events
                      WHERE hosted_room_events.room_id=hosted_rooms.room_id
                  ), 0)",
        )?;
    }

    // Copy legacy in-table disband tombstones into the permanent id registry
    // before bounded pruning can remove their heavier payloads.
    conn.execute_batch(
        "INSERT OR IGNORE INTO hosted_room_retired_ids (room_id, retired_at)
           SELECT room_id, disbanded_at FROM hosted_rooms
            WHERE disbanded_at IS NOT NULL",
    )?;
    migrate_remote_run_schema(conn)?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_hosted_room_events_cursor
           ON hosted_room_events(room_id, seq)",
    )?;
    if !schema_is_current(conn)? {
        return Err(invalid("hosted room schema migration did not complete"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Row -> struct helpers.
// ---------------------------------------------------------------------------

/// Build an [`Event`] from a row selected as
/// (room_id, seq, event_id, kind, actor_json, authority_epoch, payload_json,
/// created_at). Mirrors `_event_from_row`.
fn event_from_row(row: &Row, idempotent: bool) -> Result<Event> {
    let actor_json: String = row.get(4)?;
    let payload_json: String = row.get(6)?;
    let actor: Value =
        serde_json::from_str(&actor_json).map_err(|_| invalid("stored actor is not valid JSON"))?;
    let payload: Value = serde_json::from_str(&payload_json)
        .map_err(|_| invalid("stored payload is not valid JSON"))?;
    Ok(Event {
        room_id: row.get(0)?,
        seq: row.get(1)?,
        event_id: row.get(2)?,
        kind: row.get(3)?,
        actor,
        authority_epoch: row.get(5)?,
        payload,
        created_at: row.get(7)?,
        idempotent,
    })
}

/// The columns event selects use, in the fixed order [`event_from_row`] reads.
const EVENT_SELECT_COLS: &str =
    "room_id, seq, event_id, kind, actor_json, authority_epoch, payload_json, created_at";

// ---------------------------------------------------------------------------
// Capacity accounting.
// ---------------------------------------------------------------------------

/// Byte cost of one stored event. Mirrors `_event_storage_bytes`: the UTF-8
/// length of `event_id + kind + actor_json + payload_json`.
fn event_storage_bytes(event_id: &str, kind: &str, actor_json: &str, payload_json: &str) -> i64 {
    let total = event_id.len() + kind.len() + actor_json.len() + payload_json.len();
    total as i64
}

/// Enforce the per-room count, per-room byte, and gateway-wide byte ceilings.
/// Mirrors `_assert_event_capacity`, including the prune-and-recheck path.
fn assert_event_capacity(
    conn: &Connection,
    room_next_seq: i64,
    room_event_bytes: i64,
    additional_bytes: i64,
    allow_control: bool,
) -> Result<()> {
    let event_limit = MAX_EVENTS_PER_ROOM
        + if allow_control {
            CONTROL_EVENT_COUNT_RESERVE
        } else {
            0
        };
    let room_byte_limit = MAX_ROOM_EVENT_BYTES
        + if allow_control {
            CONTROL_EVENT_BYTE_RESERVE
        } else {
            0
        };
    let gateway_byte_limit = MAX_GATEWAY_EVENT_BYTES
        + if allow_control {
            CONTROL_EVENT_BYTE_RESERVE
        } else {
            0
        };

    if room_next_seq > event_limit {
        return Err(invalid(
            "This Group Chat reached its history limit. Start a new Group Chat to continue.",
        ));
    }
    if room_event_bytes + additional_bytes > room_byte_limit {
        return Err(invalid(
            "This Group Chat reached its storage limit. Start a new Group Chat to continue.",
        ));
    }
    let mut gateway_bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(event_bytes), 0) FROM hosted_rooms",
        [],
        |r| r.get(0),
    )?;
    if gateway_bytes + additional_bytes > gateway_byte_limit {
        prune_disbanded_rooms_locked(
            conn,
            None,
            Some((gateway_byte_limit - additional_bytes).max(0)),
        )?;
        gateway_bytes = conn.query_row(
            "SELECT COALESCE(SUM(event_bytes), 0) FROM hosted_rooms",
            [],
            |r| r.get(0),
        )?;
    }
    if gateway_bytes + additional_bytes > gateway_byte_limit {
        return Err(invalid(
            "Group Chat storage is full on this host. Delete an old Group Chat and try again.",
        ));
    }
    Ok(())
}

/// Purge disbanded-room payloads while keeping their ids permanently reserved.
/// Mirrors `_prune_disbanded_rooms_locked`.
fn prune_disbanded_rooms_locked(
    conn: &Connection,
    now: Option<f64>,
    max_gateway_event_bytes: Option<i64>,
) -> Result<i64> {
    let mut candidates: HashSet<String> = HashSet::new();

    if let Some(now) = now {
        let cutoff = now - DISBANDED_ROOM_RETENTION_SECONDS;
        let mut stmt = conn.prepare(
            "SELECT room_id FROM hosted_rooms
                 WHERE disbanded_at IS NOT NULL AND disbanded_at<=?",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| r.get::<_, String>(0))?;
        for row in rows {
            candidates.insert(row?);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT room_id FROM hosted_rooms
                 WHERE disbanded_at IS NOT NULL
                 ORDER BY disbanded_at DESC, room_id ASC
                 LIMIT -1 OFFSET ?",
        )?;
        let rows = stmt.query_map(params![MAX_DISBANDED_ROOM_TOMBSTONES], |r| {
            r.get::<_, String>(0)
        })?;
        for row in rows {
            candidates.insert(row?);
        }
    }
    if let Some(max_bytes) = max_gateway_event_bytes {
        let mut retained_bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(event_bytes), 0) FROM hosted_rooms",
            [],
            |r| r.get(0),
        )?;
        if retained_bytes > max_bytes {
            let mut stmt = conn.prepare(
                "SELECT room_id, event_bytes FROM hosted_rooms
                     WHERE disbanded_at IS NOT NULL
                     ORDER BY disbanded_at ASC, room_id ASC",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (room_id, event_bytes) = row?;
                candidates.insert(room_id);
                retained_bytes -= event_bytes;
                if retained_bytes <= max_bytes {
                    break;
                }
            }
        }
    }
    if candidates.is_empty() {
        return Ok(0);
    }

    let mut room_ids: Vec<String> = candidates.into_iter().collect();
    room_ids.sort();
    let placeholders = vec!["?"; room_ids.len()].join(",");

    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO hosted_room_retired_ids (room_id, retired_at)
                SELECT room_id, disbanded_at FROM hosted_rooms
                 WHERE room_id IN ({placeholders}) AND disbanded_at IS NOT NULL"
        ),
        params_from_iter(room_ids.iter()),
    )?;
    let dependent_tables = [
        "hosted_room_policy_transcript_state",
        "hosted_room_policy_transcript",
        "hosted_room_policy_publications",
        "hosted_room_policy_watermarks",
        "hosted_room_policy_events",
        "hosted_room_policy_threads",
        "hosted_room_policy_cursors",
        "hosted_room_driver_tasks",
        "hosted_room_driver_leases",
        "hosted_room_remote_runs",
        "hosted_room_links",
        "hosted_room_peer_reservations",
        "hosted_room_events",
    ];
    for table in dependent_tables {
        if table_exists(conn, table)? {
            conn.execute(
                &format!("DELETE FROM {table} WHERE room_id IN ({placeholders})"),
                params_from_iter(room_ids.iter()),
            )?;
        }
    }
    conn.execute(
        &format!("DELETE FROM hosted_rooms WHERE room_id IN ({placeholders})"),
        params_from_iter(room_ids.iter()),
    )?;
    Ok(room_ids.len() as i64)
}

/// Raise the room-not-found error variant Python would raise, distinguishing a
/// retained disband tombstone, a permanently retired id, and a plain miss.
/// Mirrors `_raise_room_not_found`.
fn raise_room_not_found(conn: &Connection, room_id: &str) -> HostedRoomError {
    let retained: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM hosted_rooms WHERE room_id=?",
            params![room_id],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    if retained.is_some() {
        return HostedRoomError::RoomNotFound("hosted room not found".into());
    }
    let retired: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM hosted_room_retired_ids WHERE room_id=?",
            params![room_id],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    if retired.is_some() {
        return HostedRoomError::RoomHistoryExpired(
            "Group Chat history expired; room_id remains permanently retired".into(),
        );
    }
    HostedRoomError::RoomNotFound("hosted room not found".into())
}

// ---------------------------------------------------------------------------
// Public API: create_room, append_event, read_events.
// ---------------------------------------------------------------------------

/// Create a room, or return the identical existing room idempotently. Mirrors
/// `create_room`, including the legacy-adoption authority claim.
pub fn create_room(
    db_path: &Path,
    room_id: &str,
    name: &str,
    members: &Value,
    authority_gateway_id: &str,
    now: Option<f64>,
) -> Result<Room> {
    let room_id = validate_identifier(room_id, "room_id", MAX_ROOM_ID_CHARS)?;
    let name = validate_room_name(name)?;
    let (normalized_members, members_json) = validate_members(members)?;
    let authority_gateway_id = validate_identifier(
        authority_gateway_id,
        "authority_gateway_id",
        MAX_ACTOR_ID_CHARS,
    )?;
    let now = now.unwrap_or_else(now_secs);

    let mut conn = connect(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let retired: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM hosted_room_retired_ids WHERE room_id=?",
            params![room_id],
            |r| r.get(0),
        )
        .optional()?;
    if retired.is_some() {
        return Err(HostedRoomError::RoomConflict(
            "room_id belongs to a disbanded room".into(),
        ));
    }

    let existing = tx
        .query_row(
            "SELECT room_id, name, members_json, authority_gateway_id,
                    authority_epoch, next_seq, event_bytes, revision,
                    created_at, updated_at, disbanded_at
               FROM hosted_rooms WHERE room_id=?",
            params![room_id],
            |r| {
                Ok(ExistingRoom {
                    name: r.get(1)?,
                    members_json: r.get(2)?,
                    authority_gateway_id: r.get(3)?,
                    authority_epoch: r.get(4)?,
                    next_seq: r.get(5)?,
                    event_bytes: r.get(6)?,
                    revision: r.get(7)?,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                    disbanded_at: r.get(10)?,
                })
            },
        )
        .optional()?;

    if let Some(existing) = existing {
        if existing.disbanded_at.is_some() {
            return Err(HostedRoomError::RoomConflict(
                "room_id belongs to a disbanded room".into(),
            ));
        }
        let legacy_adoption =
            existing.authority_gateway_id == "legacy" && authority_gateway_id != "legacy";
        let members_match = existing.members_json == members_json
            || (legacy_adoption
                && legacy_members_match(&existing.members_json, &normalized_members));
        if existing.name != name || !members_match {
            return Err(HostedRoomError::RoomConflict(
                "room_id already exists with different state".into(),
            ));
        }
        if legacy_adoption {
            let target_epoch = existing.authority_epoch + 1;
            let seq = existing.next_seq;
            let claim_actor_json = canonical_json(
                &serde_json::json!({"kind": "system", "id": "authority-control"}),
                "actor",
                ACTOR_JSON_MAX_BYTES,
            )?;
            let claim_payload_json = canonical_json(
                &serde_json::json!({
                    "previous_gateway_id": "legacy",
                    "authority_gateway_id": authority_gateway_id,
                    "authority_epoch": target_epoch,
                }),
                "payload",
                MAX_EVENT_JSON_BYTES,
            )?;
            let claim_bytes = event_storage_bytes(
                "system:authority-adopted",
                "authority.claimed",
                &claim_actor_json,
                &claim_payload_json,
            );
            assert_event_capacity(
                &tx,
                existing.next_seq,
                existing.event_bytes,
                claim_bytes,
                true,
            )?;
            tx.execute(
                "INSERT INTO hosted_room_events
                   (room_id, seq, event_id, kind, actor_json,
                    authority_epoch, payload_json, created_at)
                   VALUES (?, ?, 'system:authority-adopted',
                           'authority.claimed', ?, ?, ?, ?)",
                params![
                    room_id,
                    seq,
                    claim_actor_json,
                    target_epoch,
                    claim_payload_json,
                    now
                ],
            )?;
            let adopted = tx.execute(
                "UPDATE hosted_rooms
                      SET members_json=?, authority_gateway_id=?, authority_epoch=?,
                          next_seq=next_seq+1, revision=revision+1,
                          event_bytes=event_bytes+?, updated_at=?
                    WHERE room_id=? AND authority_gateway_id='legacy'
                      AND authority_epoch=? AND next_seq=?
                      AND disbanded_at IS NULL",
                params![
                    members_json,
                    authority_gateway_id,
                    target_epoch,
                    claim_bytes,
                    now,
                    room_id,
                    existing.authority_epoch,
                    seq
                ],
            )?;
            if adopted != 1 {
                return Err(HostedRoomError::AuthorityConflict(
                    "legacy room adoption lost its fence".into(),
                ));
            }
            let mut room = query_room_full(&tx, &room_id, true)?
                .ok_or_else(|| invalid("adopted room could not be reloaded"))?;
            room.adopted = true;
            let claim_event = tx.query_row(
                &format!(
                    "SELECT {EVENT_SELECT_COLS} FROM hosted_room_events
                        WHERE room_id=? AND event_id='system:authority-adopted'"
                ),
                params![room_id],
                |r| Ok(event_from_row(r, false)),
            )??;
            room.claim_event = Some(claim_event);
            tx.commit()?;
            return Ok(room);
        }
        if existing.authority_gateway_id != authority_gateway_id {
            return Err(HostedRoomError::RoomConflict(
                "room_id already belongs to a different authority".into(),
            ));
        }
        let mut room = existing_to_room(&existing, room_id.clone(), true)?;
        room.latest_seq = Some(existing.next_seq - 1);
        tx.commit()?;
        return Ok(room);
    }

    let active_rooms: i64 = tx.query_row(
        "SELECT COUNT(*) FROM hosted_rooms WHERE disbanded_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    if active_rooms >= MAX_ACTIVE_ROOMS {
        return Err(invalid(
            "This host has too many active Group Chats. Delete one and try again.",
        ));
    }

    tx.execute(
        "INSERT INTO hosted_rooms
           (room_id, name, members_json, authority_gateway_id,
            authority_epoch, next_seq, event_bytes, revision,
            created_at, updated_at, disbanded_at)
           VALUES (?, ?, ?, ?, 1, 1, 0, 1, ?, ?, NULL)",
        params![room_id, name, members_json, authority_gateway_id, now, now],
    )?;
    // Python's fresh-insert SELECT omits next_seq/disbanded_at, so the returned
    // dict has no latest_seq and no disbanded_at. Match that: latest_seq=None.
    let room = Room {
        room_id: room_id.clone(),
        name: name.clone(),
        members: Value::Array(normalized_members),
        authority_gateway_id: authority_gateway_id.clone(),
        authority_epoch: 1,
        revision: 1,
        created_at: now,
        updated_at: now,
        idempotent: false,
        disbanded_at: None,
        latest_seq: None,
        adopted: false,
        claim_event: None,
    };
    tx.commit()?;
    Ok(room)
}

/// Row shape shared by create_room's existing-room read.
struct ExistingRoom {
    name: String,
    members_json: String,
    authority_gateway_id: String,
    authority_epoch: i64,
    next_seq: i64,
    event_bytes: i64,
    revision: i64,
    created_at: f64,
    updated_at: f64,
    disbanded_at: Option<f64>,
}

fn existing_to_room(existing: &ExistingRoom, room_id: String, idempotent: bool) -> Result<Room> {
    let members: Value = serde_json::from_str(&existing.members_json)
        .map_err(|_| invalid("stored members are not valid JSON"))?;
    Ok(Room {
        room_id,
        name: existing.name.clone(),
        members,
        authority_gateway_id: existing.authority_gateway_id.clone(),
        authority_epoch: existing.authority_epoch,
        revision: existing.revision,
        created_at: existing.created_at,
        updated_at: existing.updated_at,
        idempotent,
        disbanded_at: existing.disbanded_at,
        latest_seq: Some(existing.next_seq - 1),
        adopted: false,
        claim_event: None,
    })
}

/// Reload a room as a full [`Room`] (used after the legacy-adoption update).
fn query_room_full(conn: &Connection, room_id: &str, idempotent: bool) -> Result<Option<Room>> {
    conn.query_row(
        "SELECT room_id, name, members_json, authority_gateway_id,
                authority_epoch, next_seq, revision, created_at,
                updated_at, disbanded_at
           FROM hosted_rooms WHERE room_id=?",
        params![room_id],
        |r| {
            let members_json: String = r.get(2)?;
            let members: Value = serde_json::from_str(&members_json).unwrap_or(Value::Null);
            let next_seq: i64 = r.get(5)?;
            Ok(Room {
                room_id: r.get(0)?,
                name: r.get(1)?,
                members,
                authority_gateway_id: r.get(3)?,
                authority_epoch: r.get(4)?,
                revision: r.get(6)?,
                created_at: r.get(7)?,
                updated_at: r.get(8)?,
                idempotent,
                disbanded_at: r.get(9)?,
                latest_seq: Some(next_seq - 1),
                adopted: false,
                claim_event: None,
            })
        },
    )
    .optional()
    .map_err(HostedRoomError::from)
}

/// Append one immutable event and allocate its per-room sequence atomically.
/// Mirrors `append_event`: idempotent on identical id+content, fails closed on
/// an id reused for different content (`EventConflict`), and refuses a stale
/// authority stamp (`AuthorityConflict`).
#[allow(clippy::too_many_arguments)]
pub fn append_event(
    db_path: &Path,
    room_id: &str,
    event_id: &str,
    kind: &str,
    actor: &Value,
    payload: &Value,
    authority_gateway_id: Option<&str>,
    authority_epoch: Option<i64>,
    now: Option<f64>,
) -> Result<Event> {
    let room_id = validate_identifier(room_id, "room_id", MAX_ROOM_ID_CHARS)?;
    let event_id = validate_identifier(event_id, "event_id", MAX_EVENT_ID_CHARS)?;
    let kind = validate_event_kind(kind)?;
    let (normalized_actor, actor_json) = validate_actor(actor, &kind)?;
    let actor_kind = normalized_actor
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Every valid actor kind is authority-scoped, so authority stamps are
    // always required; the Python `elif` branch is unreachable but kept in
    // shape here for fidelity.
    let authority_scoped = matches!(actor_kind, "user" | "member" | "gateway" | "system");

    let mut normalized_authority_gateway_id: Option<String> = None;
    let mut normalized_authority_epoch: Option<i64> = None;
    if authority_scoped {
        let gw = validate_identifier(
            authority_gateway_id.unwrap_or(""),
            "authority_gateway_id",
            MAX_ACTOR_ID_CHARS,
        )?;
        if actor_kind == "gateway"
            && normalized_actor.get("id").and_then(|v| v.as_str()) != Some(gw.as_str())
        {
            return Err(invalid("gateway actor.id must match authority_gateway_id"));
        }
        match authority_epoch {
            Some(epoch) if epoch >= 1 => normalized_authority_epoch = Some(epoch),
            _ => return Err(invalid("authority_epoch must be a positive integer")),
        }
        normalized_authority_gateway_id = Some(gw);
    } else if authority_gateway_id.is_some() || authority_epoch.is_some() {
        return Err(invalid(
            "authority fields are only valid for room-scoped events",
        ));
    }

    if !payload.is_object() {
        return Err(invalid("payload must be an object"));
    }
    let payload_json = canonical_json(payload, "payload", MAX_EVENT_JSON_BYTES)?;
    let now = now.unwrap_or_else(now_secs);

    let mut conn = connect(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    // Idempotent replay: same id, identical immutable content.
    let existing = tx
        .query_row(
            &format!(
                "SELECT {EVENT_SELECT_COLS} FROM hosted_room_events WHERE room_id=? AND event_id=?"
            ),
            params![room_id, event_id],
            |r| {
                Ok((
                    r.get::<_, String>(3)?,      // kind
                    r.get::<_, String>(4)?,      // actor_json
                    r.get::<_, Option<i64>>(5)?, // authority_epoch
                    r.get::<_, String>(6)?,      // payload_json
                ))
            },
        )
        .optional()?;
    if let Some((e_kind, e_actor_json, e_epoch, e_payload_json)) = existing {
        if e_kind != kind
            || e_actor_json != actor_json
            || e_epoch != normalized_authority_epoch
            || e_payload_json != payload_json
        {
            return Err(HostedRoomError::EventConflict(
                "event_id already exists with different content".into(),
            ));
        }
        // Reload the stored row to return it as the idempotent result.
        let event = tx.query_row(
            &format!(
                "SELECT {EVENT_SELECT_COLS} FROM hosted_room_events WHERE room_id=? AND event_id=?"
            ),
            params![room_id, event_id],
            |r| Ok(event_from_row(r, true)),
        )??;
        tx.commit()?;
        return Ok(event);
    }

    let room = tx
        .query_row(
            "SELECT next_seq, event_bytes, authority_gateway_id, authority_epoch
               FROM hosted_rooms
              WHERE room_id=? AND disbanded_at IS NULL",
            params![room_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,    // next_seq
                    r.get::<_, i64>(1)?,    // event_bytes
                    r.get::<_, String>(2)?, // authority_gateway_id
                    r.get::<_, i64>(3)?,    // authority_epoch
                ))
            },
        )
        .optional()?;
    let (next_seq, event_bytes, room_gateway, room_epoch) = match room {
        Some(r) => r,
        None => return Err(raise_room_not_found(&tx, &room_id)),
    };

    if authority_scoped
        && (Some(room_gateway.as_str()) != normalized_authority_gateway_id.as_deref()
            || Some(room_epoch) != normalized_authority_epoch)
    {
        return Err(HostedRoomError::AuthorityConflict(
            "stale hosted room authority".into(),
        ));
    }

    let seq = next_seq;
    let bytes = event_storage_bytes(&event_id, &kind, &actor_json, &payload_json);
    let allow_control = matches!(
        kind.as_str(),
        "authority.claimed" | "authority.lost" | "room.disbanded" | "room.stop_requested"
    );
    assert_event_capacity(&tx, next_seq, event_bytes, bytes, allow_control)?;

    tx.execute(
        "INSERT INTO hosted_room_events
           (room_id, seq, event_id, kind, actor_json, authority_epoch,
            payload_json, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            room_id,
            seq,
            event_id,
            kind,
            actor_json,
            normalized_authority_epoch,
            payload_json,
            now
        ],
    )?;
    let advanced = tx.execute(
        "UPDATE hosted_rooms
           SET next_seq=?, event_bytes=event_bytes+?, updated_at=?
           WHERE room_id=? AND next_seq=?",
        params![seq + 1, bytes, now, room_id, seq],
    )?;
    if advanced != 1 {
        // Matches Python's RuntimeError, surfaced here as a generic failure.
        return Err(invalid("hosted room sequence advance lost its write fence"));
    }
    let mut event = tx.query_row(
        &format!("SELECT {EVENT_SELECT_COLS} FROM hosted_room_events WHERE room_id=? AND seq=?"),
        params![room_id, seq],
        |r| Ok(event_from_row(r, false)),
    )??;
    // Python overwrites the reloaded actor with the normalized in-memory one.
    event.actor = normalized_actor;
    tx.commit()?;
    Ok(event)
}

/// Read a monotonic room-log delta after `since_seq`. Mirrors `read_events`,
/// including the SQL cumulative-byte window cap and the JSON page-byte binary
/// search.
pub fn read_events(
    db_path: &Path,
    room_id: &str,
    since_seq: i64,
    limit: i64,
    include_disbanded: bool,
) -> Result<EventPage> {
    let room_id = validate_identifier(room_id, "room_id", MAX_ROOM_ID_CHARS)?;
    if since_seq < 0 {
        return Err(invalid("since_seq must be a non-negative integer"));
    }
    if !(1..=MAX_LOG_LIMIT).contains(&limit) {
        return Err(invalid(format!(
            "limit must be between 1 and {MAX_LOG_LIMIT}"
        )));
    }

    let mut conn = connect(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;

    let room = tx
        .query_row(
            "SELECT next_seq, authority_gateway_id, authority_epoch
               FROM hosted_rooms
               WHERE room_id=? AND (disbanded_at IS NULL OR ?)",
            params![room_id, include_disbanded as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    let (next_seq, authority_gateway, authority_epoch) = match room {
        Some(r) => r,
        None => return Err(raise_room_not_found(&tx, &room_id)),
    };
    let latest_seq = next_seq - 1;
    if since_seq > latest_seq {
        return Err(invalid("since_seq is ahead of the hosted room log"));
    }

    let mut stmt = tx.prepare(
        "WITH candidates AS (
                SELECT room_id, seq, event_id, kind, actor_json,
                       authority_epoch, payload_json, created_at,
                       SUM(
                           LENGTH(CAST(event_id AS BLOB)) +
                           LENGTH(CAST(kind AS BLOB)) +
                           LENGTH(CAST(actor_json AS BLOB)) +
                           LENGTH(CAST(payload_json AS BLOB))
                       ) OVER (ORDER BY seq ASC) AS cumulative_bytes
                  FROM hosted_room_events
                 WHERE room_id=? AND seq>?
                 ORDER BY seq ASC LIMIT ?
            )
            SELECT room_id, seq, event_id, kind, actor_json,
                   authority_epoch, payload_json, created_at
              FROM candidates
             WHERE cumulative_bytes<=?
             ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(
        params![room_id, since_seq, limit, MAX_LOG_PAGE_BYTES as i64],
        |r| Ok(event_from_row(r, false)),
    )?;
    let mut events: Vec<Event> = Vec::new();
    for row in rows {
        events.push(row??);
    }
    drop(stmt);
    tx.commit()?;

    // Page-byte trim (mirrors build_page / page_bytes and the binary search).
    let mut count = events.len();
    if !events.is_empty()
        && page_bytes(
            &events,
            since_seq,
            latest_seq,
            &authority_gateway,
            authority_epoch,
        ) > MAX_LOG_PAGE_BYTES
    {
        let (mut low, mut high) = (1usize, events.len());
        while low < high {
            let middle = (low + high).div_ceil(2);
            let candidate = page_bytes(
                &events[..middle],
                since_seq,
                latest_seq,
                &authority_gateway,
                authority_epoch,
            );
            if candidate <= MAX_LOG_PAGE_BYTES {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        count = low;
        if page_bytes(
            &events[..count],
            since_seq,
            latest_seq,
            &authority_gateway,
            authority_epoch,
        ) > MAX_LOG_PAGE_BYTES
        {
            return Err(invalid("hosted room event exceeds replay page limit"));
        }
    }
    events.truncate(count);

    let cursor = events.last().map(|e| e.seq).unwrap_or(since_seq);
    Ok(EventPage {
        has_more: cursor < latest_seq,
        cursor,
        latest_seq,
        events,
        authority_gateway_id: authority_gateway,
        authority_epoch,
    })
}

/// Serialized byte size of one candidate page. Mirrors Python's `page_bytes`:
/// `json.dumps(page, ensure_ascii=False, separators=(",",":"))` length. Key
/// order does not affect the byte count (same keys/values), so the sorted
/// serde_json output measures the same total. Float formatting can differ by a
/// byte or two from CPython's `repr`, which is immaterial at the 2 MiB ceiling.
fn page_bytes(
    events: &[Event],
    since_seq: i64,
    latest_seq: i64,
    gateway: &str,
    epoch: i64,
) -> usize {
    let cursor = events.last().map(|e| e.seq).unwrap_or(since_seq);
    let event_values: Vec<Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "room_id": e.room_id,
                "seq": e.seq,
                "event_id": e.event_id,
                "kind": e.kind,
                "actor": e.actor,
                "authority_epoch": e.authority_epoch,
                "payload": e.payload,
                "created_at": e.created_at,
                "idempotent": e.idempotent,
            })
        })
        .collect();
    let page = serde_json::json!({
        "events": event_values,
        "cursor": cursor,
        "latest_seq": latest_seq,
        "has_more": cursor < latest_seq,
        "authority": {"gateway_id": gateway, "epoch": epoch},
    });
    serde_json::to_string(&page).map(|s| s.len()).unwrap_or(0)
}

/// Allow adoption to add routing metadata an older room could not store.
/// Mirrors `_legacy_members_match`.
fn legacy_members_match(existing_json: &str, proposed: &[Value]) -> bool {
    let existing: Value = match serde_json::from_str(existing_json) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let existing = match existing.as_array() {
        Some(a) => a,
        None => return false,
    };
    if existing.len() != proposed.len() {
        return false;
    }
    for (previous, current) in existing.iter().zip(proposed.iter()) {
        let previous = match previous.as_object() {
            Some(o) => o,
            None => return false,
        };
        let current = match current.as_object() {
            Some(o) => o,
            None => return false,
        };
        let mut prev = previous.clone();
        let mut cur = current.clone();
        // Python pops "target" with a None default; JSON null and an absent
        // key both read as None there, so normalize an explicit null away too.
        let previous_target = prev.remove("target").filter(|v| !v.is_null());
        let current_target = cur.remove("target").filter(|v| !v.is_null());
        if prev != cur {
            return false;
        }
        // Python: `if previous_target not in (None, {})` -> a None or empty
        // dict counts as "no stored target" and never blocks adoption.
        let previous_target_is_empty = match &previous_target {
            None => true,
            Some(Value::Object(o)) => o.is_empty(),
            Some(_) => false,
        };
        if !previous_target_is_empty && previous_target != current_target {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let mut path = std::env::temp_dir();
            path.push(format!("hermes_hosted_rooms_log_{tag}_{pid}_{n}.db"));
            let _ = std::fs::remove_file(&path);
            TempDb { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(self.path.with_extension("db-wal"));
            let _ = std::fs::remove_file(self.path.with_extension("db-shm"));
        }
    }

    const GW: &str = "install:test-gateway";

    fn make_room(db: &Path) -> Room {
        create_room(
            db,
            "room-1",
            "Test Room",
            &serde_json::json!([{"id": "u1"}, {"id": "m1"}]),
            GW,
            Some(1000.0),
        )
        .unwrap()
    }

    fn user_actor() -> Value {
        serde_json::json!({"kind": "user", "id": "u1"})
    }

    fn append_user_message(db: &Path, event_id: &str, text: &str, now: f64) -> Result<Event> {
        append_event(
            db,
            "room-1",
            event_id,
            "message.user",
            &user_actor(),
            &serde_json::json!({"text": text}),
            Some(GW),
            Some(1),
            Some(now),
        )
    }

    #[test]
    fn schema_init_creates_all_tables_and_cursor_index() {
        let db = TempDb::new("schema");
        let conn = connect(db.path()).unwrap();
        assert!(schema_is_current(&conn).unwrap());
        for table in [
            "hosted_rooms",
            "hosted_room_events",
            "hosted_room_retired_ids",
            "hosted_room_links",
            "hosted_room_remote_runs",
            "hosted_room_revoked_grants",
            "hosted_room_peer_reservations",
        ] {
            assert!(table_exists(&conn, table).unwrap(), "missing table {table}");
        }
        // The event cursor index and the remote-run identity PK are the two
        // parts of the schema _schema_is_current checks beyond column presence.
        let idx: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_hosted_room_events_cursor'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(idx.is_some());
        assert_eq!(
            primary_key_columns(&conn, "hosted_room_remote_runs").unwrap(),
            REMOTE_RUN_IDENTITY_COLUMNS
        );
    }

    #[test]
    fn append_then_read_page_roundtrip() {
        let db = TempDb::new("roundtrip");
        make_room(db.path());
        let ev = append_user_message(db.path(), "evt-1", "hello", 1001.0).unwrap();
        assert_eq!(ev.seq, 1);
        assert!(!ev.idempotent);
        assert_eq!(ev.authority_epoch, Some(1));
        assert_eq!(ev.payload, serde_json::json!({"text": "hello"}));

        let page = read_events(db.path(), "room-1", 0, 100, false).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event_id, "evt-1");
        assert_eq!(page.cursor, 1);
        assert_eq!(page.latest_seq, 1);
        assert!(!page.has_more);
        assert_eq!(page.authority_gateway_id, GW);
        assert_eq!(page.authority_epoch, 1);
    }

    #[test]
    fn idempotent_reingest_returns_original() {
        let db = TempDb::new("idempotent");
        make_room(db.path());
        let first = append_user_message(db.path(), "evt-1", "hello", 1001.0).unwrap();
        // Same id + identical content: returns the stored row, flagged idempotent.
        let second = append_user_message(db.path(), "evt-1", "hello", 2002.0).unwrap();
        assert_eq!(first.seq, second.seq);
        assert!(!first.idempotent);
        assert!(second.idempotent);
        assert_eq!(second.created_at, 1001.0); // original timestamp, not the retry's

        // Same id, different content: fails closed.
        let conflict = append_user_message(db.path(), "evt-1", "changed", 3003.0);
        assert!(matches!(conflict, Err(HostedRoomError::EventConflict(_))));

        // Only one event was ever stored.
        let page = read_events(db.path(), "room-1", 0, 100, false).unwrap();
        assert_eq!(page.events.len(), 1);
    }

    #[test]
    fn read_refuses_since_seq_ahead_of_log() {
        let db = TempDb::new("gap");
        make_room(db.path());
        append_user_message(db.path(), "evt-1", "hello", 1001.0).unwrap();
        // latest_seq is 1; asking past it is the sequence-gap refusal.
        let err = read_events(db.path(), "room-1", 5, 100, false).unwrap_err();
        match err {
            HostedRoomError::Invalid(m) => {
                assert_eq!(m, "since_seq is ahead of the hosted room log")
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn append_refuses_authority_epoch_regression() {
        let db = TempDb::new("epoch");
        make_room(db.path()); // room is at authority_epoch 1
                              // A stale caller stamps epoch 2 (or a different gateway): refused.
        let err = append_event(
            db.path(),
            "room-1",
            "evt-1",
            "message.user",
            &user_actor(),
            &serde_json::json!({"text": "hi"}),
            Some(GW),
            Some(2),
            Some(1001.0),
        )
        .unwrap_err();
        assert!(err.is_authority_conflict());
        assert_eq!(err.reason(), Some("authority_conflict"));
        assert_eq!(err.to_string(), "stale hosted room authority");

        // A wrong gateway id at the right epoch is refused the same way.
        let err2 = append_event(
            db.path(),
            "room-1",
            "evt-1",
            "message.user",
            &user_actor(),
            &serde_json::json!({"text": "hi"}),
            Some("install:other"),
            Some(1),
            Some(1001.0),
        )
        .unwrap_err();
        assert!(err2.is_authority_conflict());

        // Nothing was appended.
        let page = read_events(db.path(), "room-1", 0, 100, false).unwrap();
        assert!(page.events.is_empty());
    }

    #[test]
    fn read_cursor_paginates_monotonically() {
        let db = TempDb::new("cursor");
        make_room(db.path());
        append_user_message(db.path(), "evt-1", "one", 1001.0).unwrap();
        append_user_message(db.path(), "evt-2", "two", 1002.0).unwrap();
        append_user_message(db.path(), "evt-3", "three", 1003.0).unwrap();

        // First page: limit 1 from the start.
        let page1 = read_events(db.path(), "room-1", 0, 1, false).unwrap();
        assert_eq!(page1.events.len(), 1);
        assert_eq!(page1.events[0].seq, 1);
        assert_eq!(page1.cursor, 1);
        assert_eq!(page1.latest_seq, 3);
        assert!(page1.has_more);

        // Resume from the returned cursor.
        let page2 = read_events(db.path(), "room-1", page1.cursor, 100, false).unwrap();
        let seqs: Vec<i64> = page2.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(page2.cursor, 3);
        assert!(!page2.has_more);
    }

    #[test]
    fn create_room_is_idempotent_and_conflicts_on_diff() {
        let db = TempDb::new("create");
        let a = make_room(db.path());
        assert!(!a.idempotent);
        // Same identity + state: idempotent hit.
        let b = make_room(db.path());
        assert!(b.idempotent);
        assert_eq!(a.room_id, b.room_id);

        // Same id, different name: conflict.
        let conflict = create_room(
            db.path(),
            "room-1",
            "Different",
            &serde_json::json!([{"id": "u1"}, {"id": "m1"}]),
            GW,
            Some(1000.0),
        );
        assert!(matches!(conflict, Err(HostedRoomError::RoomConflict(_))));
    }

    #[test]
    fn read_unknown_room_is_not_found() {
        let db = TempDb::new("missing");
        make_room(db.path());
        let err = read_events(db.path(), "no-such-room", 0, 100, false).unwrap_err();
        assert!(err.is_room_not_found());
    }

    #[test]
    fn canonical_json_sorts_keys_and_caps_bytes() {
        let v = serde_json::json!({"b": 1, "a": 2});
        assert_eq!(canonical_json(&v, "x", 4096).unwrap(), r#"{"a":2,"b":1}"#);
        let err = canonical_json(&v, "x", 3).unwrap_err();
        assert_eq!(err.to_string(), "x is too large");
    }

    #[test]
    fn identifier_validation_matches_python_rules() {
        assert_eq!(
            validate_identifier(" room.1:a-b_c ", "room_id", 128).unwrap(),
            "room.1:a-b_c"
        );
        assert!(validate_identifier("", "room_id", 128).is_err());
        assert!(validate_identifier(".leading", "room_id", 128).is_err());
        assert!(validate_identifier("has space", "room_id", 128).is_err());
        assert!(validate_identifier("x", "room_id", 0).is_err());
    }
}
