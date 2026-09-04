//! Port of gateway/hosted_room_replicas.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Replica store and takeover primitives for hosted Group Chat rooms. The
//! authority gateway owns a room's ordered log in `hosted_rooms_log.rs`; this
//! module gives every OTHER participant gateway a durable local copy of that
//! log (two extra tables in the same shared `state.db`) plus the fenced
//! primitives to continue the room when the authority host dies. `ingest_page`
//! persists replay pages idempotently, refusing sequence gaps and
//! authority-epoch regressions; `promote_replica` instantiates the replicated
//! log as a locally-owned room at `epoch + 1` with a lineage-proving
//! `authority.claimed` event; `demote_room` fences a returning stale authority
//! with an `authority.lost` event once a newer epoch is proven. These are
//! storage primitives only: none of them decide WHEN takeover is safe, exactly
//! like the Python source. The module defines its own error hierarchy
//! (`ReplicaError` / `ReplicaGapError` / `ReplicaEpochRegressionError`), which
//! in Python subclass `HostedRoomError`; errors propagated from the imported
//! `hosted_rooms` validators (and `RoomConflictError`) are carried through the
//! `ReplicaError::Hosted` variant so the base-class relationship is preserved.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use crate::hosted_rooms_log::{
    self as rooms, HostedRoomError, MAX_ACTOR_ID_CHARS, MAX_EVENT_JSON_BYTES, MAX_ROOM_ID_CHARS,
};

// ---------------------------------------------------------------------------
// Constants (mirror the module-level constants in hosted_room_replicas.py).
// ---------------------------------------------------------------------------

pub const MAX_REPLICA_ROOMS: i64 = 256;
pub const MAX_REPLICA_EVENT_BYTES: i64 = 256 * 1024 * 1024;

// Actor JSON has its own small ceiling everywhere it is canonicalized, exactly
// as the Python source passes `max_bytes=4 * 1024`.
const ACTOR_JSON_MAX_BYTES: usize = 4 * 1024;

// ---------------------------------------------------------------------------
// Errors (mirror the ReplicaError exception hierarchy).
// ---------------------------------------------------------------------------

/// One error from the replica layer.
///
/// In Python `ReplicaError` subclasses `HostedRoomError`, and `ReplicaGapError`
/// / `ReplicaEpochRegressionError` subclass `ReplicaError`. Here `Invalid`,
/// `Gap` and `EpochRegression` are the three replica-defined classes. Errors
/// raised by the imported `hosted_rooms` validators (plain `HostedRoomError`)
/// and `RoomConflictError` (raised by `promote_replica`) are carried through
/// `Hosted` so a caller can still distinguish, say, a `RoomConflict` from a
/// replica gap. `Sqlite` surfaces an underlying SQLite failure best-effort.
#[derive(Debug)]
pub enum ReplicaError {
    /// Base `ReplicaError`: an invalid or conflicting replica operation.
    Invalid(String),
    /// `ReplicaGapError`: a page does not start at the next expected sequence.
    Gap(String),
    /// `ReplicaEpochRegressionError`: an older authority epoch than stored.
    EpochRegression(String),
    /// A `hosted_rooms`-layer error surfaced through the replica API (a
    /// validator rejection, or `RoomConflictError` from `promote_replica`).
    Hosted(HostedRoomError),
    /// Any underlying SQLite failure.
    Sqlite(rusqlite::Error),
}

impl ReplicaError {
    /// True for `ReplicaGapError`.
    pub fn is_gap(&self) -> bool {
        matches!(self, ReplicaError::Gap(_))
    }

    /// True for `ReplicaEpochRegressionError`.
    pub fn is_epoch_regression(&self) -> bool {
        matches!(self, ReplicaError::EpochRegression(_))
    }

    /// True for a propagated `RoomConflictError`.
    pub fn is_room_conflict(&self) -> bool {
        matches!(self, ReplicaError::Hosted(HostedRoomError::RoomConflict(_)))
    }
}

impl fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplicaError::Invalid(m) | ReplicaError::Gap(m) | ReplicaError::EpochRegression(m) => {
                f.write_str(m)
            }
            ReplicaError::Hosted(err) => write!(f, "{err}"),
            ReplicaError::Sqlite(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ReplicaError {}

impl From<HostedRoomError> for ReplicaError {
    fn from(err: HostedRoomError) -> Self {
        ReplicaError::Hosted(err)
    }
}

impl From<rusqlite::Error> for ReplicaError {
    fn from(err: rusqlite::Error) -> Self {
        ReplicaError::Sqlite(err)
    }
}

type Result<T> = std::result::Result<T, ReplicaError>;

fn invalid(msg: impl Into<String>) -> ReplicaError {
    ReplicaError::Invalid(msg.into())
}

fn gap(msg: impl Into<String>) -> ReplicaError {
    ReplicaError::Gap(msg.into())
}

fn epoch_regression(msg: impl Into<String>) -> ReplicaError {
    ReplicaError::EpochRegression(msg.into())
}

fn hosted_invalid(msg: impl Into<String>) -> ReplicaError {
    ReplicaError::Hosted(HostedRoomError::Invalid(msg.into()))
}

// ---------------------------------------------------------------------------
// Output structs (mirror the dicts the Python functions return).
// ---------------------------------------------------------------------------

/// A page's authority stamp: which gateway owns the room and at which epoch.
/// Mirrors the `{"gateway_id", "epoch"}` dict `_validate_page` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Authority {
    pub gateway_id: String,
    pub epoch: i64,
}

/// Result of `ingest_page`. Mirrors the returned dict.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestResult {
    pub room_id: String,
    pub stored_seq: i64,
    pub ingested: usize,
    pub authority: Authority,
    pub caught_up: bool,
}

/// Result of `replica_state`. Mirrors the returned dict.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplicaState {
    pub room_id: String,
    pub name: String,
    pub members: Value,
    pub authority: Authority,
    pub last_seq: i64,
    pub latest_seq: i64,
    pub event_bytes: i64,
    pub created_at: f64,
    pub updated_at: f64,
}

/// Result of `promote_replica`. Mirrors the returned dict.
#[derive(Debug, Clone, PartialEq)]
pub struct PromoteResult {
    pub room_id: String,
    pub authority_gateway_id: String,
    pub authority_epoch: i64,
    pub previous_gateway_id: String,
    pub previous_epoch: i64,
    pub claim_seq: i64,
    pub latest_seq: i64,
}

/// Result of `demote_room`. Mirrors the returned dict.
#[derive(Debug, Clone, PartialEq)]
pub struct DemoteResult {
    pub room_id: String,
    pub authority_gateway_id: String,
    pub authority_epoch: i64,
    pub idempotent: bool,
}

/// One validated event drawn from a replay page. Mirrors the per-event dict
/// `_validate_page` keeps: `seq`, `event_id`, `kind` and `actor` are validated,
/// `payload` is only required to be present, and `authority_epoch`/`created_at`
/// are read verbatim later by `ingest_page`.
struct PageEvent {
    seq: i64,
    event_id: String,
    kind: String,
    actor: Value,
    payload: Value,
    authority_epoch: Option<Value>,
    created_at: Option<Value>,
}

// ---------------------------------------------------------------------------
// Time + paths.
// ---------------------------------------------------------------------------

/// Seconds since the Unix epoch, matching Python's `time.time()`.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Production store path: the same shared `$HERMES_HOME/state.db` the room-log
/// layer opens, so the replica tables live in one database with the authority
/// tables. Callers may pass any path to isolate tests.
pub fn default_db_path() -> PathBuf {
    crate::config_file::hermes_home().join("state.db")
}

// ---------------------------------------------------------------------------
// Schema.
// ---------------------------------------------------------------------------

/// Create the two replica tables. Mirrors `_initialize_replica_schema`,
/// statement for statement (column order, checks and the composite primary
/// key). Idempotent (`IF NOT EXISTS`); safe to run inside a transaction.
fn initialize_replica_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_replicas (
            room_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            members_json TEXT NOT NULL,
            authority_gateway_id TEXT NOT NULL,
            authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 1),
            last_seq INTEGER NOT NULL DEFAULT 0 CHECK (last_seq >= 0),
            latest_seq INTEGER NOT NULL DEFAULT 0,
            event_bytes INTEGER NOT NULL DEFAULT 0,
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL
        )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_room_replica_events (
            room_id TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK (seq >= 1),
            event_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            actor_json TEXT NOT NULL,
            authority_epoch INTEGER,
            payload_json TEXT NOT NULL,
            created_at REAL NOT NULL,
            PRIMARY KEY (room_id, seq)
        )",
    )?;
    Ok(())
}

/// Open the shared DB (ensuring the room-log schema via `rooms::connect`) and
/// ensure the replica tables exist. Mirrors `_ensure_schema`.
fn ensure_replica_schema(db_path: &Path) -> Result<()> {
    let conn = rooms::connect(db_path)?;
    initialize_replica_schema(&conn)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation helpers.
// ---------------------------------------------------------------------------

/// Validate an identifier drawn from an untyped JSON value, reproducing the
/// `"{label} must be a string"` branch the Python `_validate_identifier`
/// applies before delegating to the shared identifier rules.
fn validate_identifier_value(v: Option<&Value>, label: &str, max_chars: usize) -> Result<String> {
    match v {
        Some(Value::String(s)) => {
            rooms::validate_identifier(s, label, max_chars).map_err(ReplicaError::from)
        }
        _ => Err(hosted_invalid(format!("{label} must be a string"))),
    }
}

/// Validate a room name drawn from an untyped JSON value, reproducing the
/// `"name must be a string"` branch of the Python `_validate_room_name`.
fn validate_room_name_value(v: &Value) -> Result<String> {
    match v {
        Value::String(s) => rooms::validate_room_name(s).map_err(ReplicaError::from),
        _ => Err(hosted_invalid("name must be a string")),
    }
}

/// Read a JSON value as a positive integer, matching Python's
/// `isinstance(x, bool) or not isinstance(x, int) or x < 1` rejection: a JSON
/// bool is `Value::Bool` (never a `Number`), and a float parses to a `Number`
/// whose `as_i64` is `None`, so both are rejected exactly as Python rejects
/// `bool` and `float`.
fn as_positive_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64().filter(|&i| i >= 1),
        _ => None,
    }
}

/// Byte cost of one page event, matching `_event_bytes`: the UTF-8 length of
/// `event_id + kind + json(actor) + json(payload)`. The Python source dumps
/// actor/payload with `separators=(",",":")` and no `sort_keys`; serde's
/// default `Map` is sorted, but key order never changes the total byte count
/// (same keys and values), so the length matches. Float formatting can differ
/// by a byte or two from CPython, immaterial at the 256 MiB ceiling.
fn event_bytes(event_id: &str, kind: &str, actor: &Value, payload: &Value) -> i64 {
    let actor_len = serde_json::to_string(actor).map(|s| s.len()).unwrap_or(0);
    let payload_len = serde_json::to_string(payload).map(|s| s.len()).unwrap_or(0);
    (event_id.len() + kind.len() + actor_len + payload_len) as i64
}

/// Convert an event's raw `authority_epoch` into a bindable SQLite value,
/// matching Python sqlite3's binding of an arbitrary Python object into the
/// nullable `authority_epoch INTEGER` column: absent/None -> NULL, bool -> 0/1
/// integer, int -> integer, float -> real, str -> text. A real groups.log
/// event carries an int or omits the field; the array/object last-resort
/// (where Python sqlite3 would raise) is unreachable in practice and stored as
/// its JSON text so the ingest still completes.
fn authority_epoch_to_sql(v: Option<&Value>) -> SqlValue {
    match v {
        None | Some(Value::Null) => SqlValue::Null,
        Some(Value::Bool(b)) => SqlValue::Integer(i64::from(*b)),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(u) = n.as_u64() {
                SqlValue::Integer(u as i64)
            } else {
                SqlValue::Real(n.as_f64().unwrap_or(0.0))
            }
        }
        Some(Value::String(s)) => SqlValue::Text(s.clone()),
        Some(other) => SqlValue::Text(other.to_string()),
    }
}

/// Resolve a stored `created_at`, matching Python's `float(event.get("created_at") or now)`.
/// Falsy values (absent, null, `False`, numeric zero, empty string) fall back
/// to `now`; a truthy value is coerced to float. A non-numeric string (where
/// Python's `float()` would raise) falls back to `now` here rather than
/// erroring, since real events always carry a numeric timestamp.
fn resolve_created_at(v: Option<&Value>, now: f64) -> f64 {
    let truthy = match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    };
    if !truthy {
        return now;
    }
    match v {
        Some(Value::Bool(_)) => 1.0,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(now),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(now),
        _ => now,
    }
}

/// Validate a replay page. Mirrors `_validate_page`: the page and its
/// `authority` stamp must be objects, `events` a list, the authority carries a
/// valid gateway id and a positive-integer epoch, and every event has a
/// positive-integer `seq` (contiguous across the page), non-empty string
/// `event_id`/`kind`, an object `actor`, and a present `payload`.
fn validate_page(page: &Value) -> Result<(Vec<PageEvent>, Authority)> {
    let obj = page.as_object().ok_or_else(|| invalid("page must be an object"))?;
    let events_arr = match obj.get("events") {
        Some(Value::Array(a)) => a,
        _ => return Err(invalid("page.events must be a list")),
    };
    let authority_obj = match obj.get("authority") {
        Some(Value::Object(o)) => o,
        _ => return Err(invalid("page.authority is required for replication")),
    };
    let gateway_id = validate_identifier_value(
        authority_obj.get("gateway_id"),
        "page.authority.gateway_id",
        MAX_ACTOR_ID_CHARS,
    )?;
    let epoch = as_positive_int(authority_obj.get("epoch"))
        .ok_or_else(|| invalid("page.authority.epoch must be a positive integer"))?;

    let mut out: Vec<PageEvent> = Vec::with_capacity(events_arr.len());
    let mut previous_seq: Option<i64> = None;
    for ev in events_arr {
        let eo = ev
            .as_object()
            .ok_or_else(|| invalid("page events must be objects"))?;
        let seq = as_positive_int(eo.get("seq"))
            .ok_or_else(|| invalid("event.seq must be a positive integer"))?;
        if let Some(prev) = previous_seq {
            if seq != prev + 1 {
                return Err(gap("page events must be contiguous"));
            }
        }
        previous_seq = Some(seq);
        let event_id = match eo.get("event_id") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err(invalid("event.event_id must be a non-empty string")),
        };
        let kind = match eo.get("kind") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err(invalid("event.kind must be a non-empty string")),
        };
        let actor = match eo.get("actor") {
            Some(v @ Value::Object(_)) => v.clone(),
            _ => return Err(invalid("event.actor must be an object")),
        };
        if !eo.contains_key("payload") {
            return Err(invalid("event.payload is required"));
        }
        let payload = eo.get("payload").cloned().unwrap_or(Value::Null);
        out.push(PageEvent {
            seq,
            event_id,
            kind,
            actor,
            payload,
            authority_epoch: eo.get("authority_epoch").cloned(),
            created_at: eo.get("created_at").cloned(),
        });
    }
    Ok((out, Authority { gateway_id, epoch }))
}

// ---------------------------------------------------------------------------
// Public API: ingest_page, replica_state, promote_replica, demote_room.
// ---------------------------------------------------------------------------

/// Persist one replay page for `room_id`; idempotent, gap- and
/// epoch-regression-safe. `page` is the verbatim result of the authority's
/// `read_events()` call, whose `authority` stamp proves lineage. Mirrors
/// `ingest_page`.
pub fn ingest_page(
    db_path: &Path,
    room_id: &Value,
    room_name: &Value,
    members: &Value,
    page: &Value,
    now: Option<f64>,
) -> Result<IngestResult> {
    let room_id = validate_identifier_value(Some(room_id), "room_id", MAX_ROOM_ID_CHARS)?;
    let room_name = validate_room_name_value(room_name)?;
    let (_members, members_json) = rooms::validate_members(members).map_err(ReplicaError::from)?;
    let (events, authority) = validate_page(page)?;
    let now = now.unwrap_or_else(now_secs);
    ensure_replica_schema(db_path)?;

    let mut conn = rooms::connect(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    initialize_replica_schema(&tx)?;

    let row = tx
        .query_row(
            "SELECT authority_gateway_id, authority_epoch, last_seq,
                    latest_seq, event_bytes
               FROM hosted_room_replicas WHERE room_id=?",
            params![room_id],
            |r| {
                Ok((
                    r.get::<_, i64>(1)?, // authority_epoch
                    r.get::<_, i64>(2)?, // last_seq
                    r.get::<_, i64>(4)?, // event_bytes
                ))
            },
        )
        .optional()?;

    let row_exists = row.is_some();
    let (stored_epoch, last_seq, stored_bytes) = match row {
        None => {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM hosted_room_replicas", [], |r| r.get(0))?;
            if count >= MAX_REPLICA_ROOMS {
                return Err(invalid("replica room capacity exhausted"));
            }
            (0i64, 0i64, 0i64)
        }
        Some((epoch, last, bytes)) => (epoch, last, bytes),
    };

    if authority.epoch < stored_epoch {
        return Err(epoch_regression(
            "page authority epoch is older than the stored replica epoch",
        ));
    }

    let new_events: Vec<&PageEvent> = events.iter().filter(|e| e.seq > last_seq).collect();
    if let Some(first) = new_events.first() {
        if first.seq != last_seq + 1 {
            return Err(gap("page skips sequences the replica has not stored"));
        }
    }

    let mut added_bytes: i64 = 0;
    for event in &new_events {
        let size = event_bytes(&event.event_id, &event.kind, &event.actor, &event.payload);
        if stored_bytes + added_bytes + size > MAX_REPLICA_EVENT_BYTES {
            return Err(invalid("replica event storage exhausted"));
        }
        let actor_json =
            rooms::canonical_json(&event.actor, "actor", ACTOR_JSON_MAX_BYTES).map_err(ReplicaError::from)?;
        let payload_json = rooms::canonical_json(&event.payload, "payload", MAX_EVENT_JSON_BYTES)
            .map_err(ReplicaError::from)?;
        let epoch_value = authority_epoch_to_sql(event.authority_epoch.as_ref());
        let created_at = resolve_created_at(event.created_at.as_ref(), now);
        tx.execute(
            "INSERT INTO hosted_room_replica_events
               (room_id, seq, event_id, kind, actor_json, authority_epoch,
                payload_json, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                room_id,
                event.seq,
                event.event_id,
                event.kind,
                actor_json,
                epoch_value,
                payload_json,
                created_at
            ],
        )?;
        added_bytes += size;
    }

    let new_last = new_events.last().map(|e| e.seq).unwrap_or(last_seq);
    // page.get("latest_seq"): keep only a plain integer; bool or non-int falls
    // back to new_last, exactly like Python.
    let latest_seq = match page.get("latest_seq") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(new_last),
        _ => new_last,
    };
    let stored_latest = latest_seq.max(new_last);

    if row_exists {
        tx.execute(
            "UPDATE hosted_room_replicas
                SET name=?, members_json=?, authority_gateway_id=?,
                    authority_epoch=?, last_seq=?, latest_seq=?,
                    event_bytes=event_bytes+?, updated_at=?
              WHERE room_id=?",
            params![
                room_name,
                members_json,
                authority.gateway_id,
                authority.epoch,
                new_last,
                stored_latest,
                added_bytes,
                now,
                room_id
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO hosted_room_replicas
               (room_id, name, members_json, authority_gateway_id,
                authority_epoch, last_seq, latest_seq, event_bytes,
                created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                room_id,
                room_name,
                members_json,
                authority.gateway_id,
                authority.epoch,
                new_last,
                stored_latest,
                added_bytes,
                now,
                now
            ],
        )?;
    }

    let ingested = new_events.len();
    tx.commit()?;
    Ok(IngestResult {
        room_id,
        stored_seq: new_last,
        ingested,
        caught_up: new_last >= stored_latest,
        authority,
    })
}

/// Return the stored replica's coverage and authority lineage. Mirrors
/// `replica_state`.
pub fn replica_state(db_path: &Path, room_id: &Value) -> Result<ReplicaState> {
    let room_id = validate_identifier_value(Some(room_id), "room_id", MAX_ROOM_ID_CHARS)?;
    ensure_replica_schema(db_path)?;

    let mut conn = rooms::connect(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    initialize_replica_schema(&tx)?;
    let row = tx
        .query_row(
            "SELECT room_id, name, members_json, authority_gateway_id,
                    authority_epoch, last_seq, latest_seq, event_bytes,
                    created_at, updated_at
               FROM hosted_room_replicas WHERE room_id=?",
            params![room_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?, // room_id
                    r.get::<_, String>(1)?, // name
                    r.get::<_, String>(2)?, // members_json
                    r.get::<_, String>(3)?, // authority_gateway_id
                    r.get::<_, i64>(4)?,    // authority_epoch
                    r.get::<_, i64>(5)?,    // last_seq
                    r.get::<_, i64>(6)?,    // latest_seq
                    r.get::<_, i64>(7)?,    // event_bytes
                    r.get::<_, f64>(8)?,    // created_at
                    r.get::<_, f64>(9)?,    // updated_at
                ))
            },
        )
        .optional()?;
    tx.commit()?;

    let (
        room_id,
        name,
        members_json,
        gateway_id,
        epoch,
        last_seq,
        latest_seq,
        event_bytes,
        created_at,
        updated_at,
    ) = row.ok_or_else(|| invalid("replica not found"))?;
    let members: Value = serde_json::from_str(&members_json)
        .map_err(|_| invalid("stored members are not valid JSON"))?;
    Ok(ReplicaState {
        room_id,
        name,
        members,
        authority: Authority { gateway_id, epoch },
        last_seq,
        latest_seq,
        event_bytes,
        created_at,
        updated_at,
    })
}

/// Continue a replicated room on THIS gateway at `epoch + 1`. Copies the
/// replica's log into the authoritative store, appends a lineage-proving
/// `authority.claimed` event, and clears the replica. Mirrors
/// `promote_replica`.
///
/// Fails closed until a stable install identity is available: the Rust port of
/// `local_authority_gateway_id` returns an error while the install-id module is
/// unported (see `hosted_rooms_log.rs`), exactly like Python fails when the
/// install id is unavailable. `reason` defaults to `"authority-unreachable"`.
pub fn promote_replica(
    db_path: &Path,
    room_id: &Value,
    reason: Option<&str>,
    now: Option<f64>,
) -> Result<PromoteResult> {
    let room_id = validate_identifier_value(Some(room_id), "room_id", MAX_ROOM_ID_CHARS)?;
    let reason = reason.unwrap_or("authority-unreachable");
    if reason.is_empty() || reason.chars().count() > 200 {
        return Err(invalid(
            "reason must be a non-empty string of at most 200 chars",
        ));
    }
    let now = now.unwrap_or_else(now_secs);
    let local_gateway = rooms::local_authority_gateway_id().map_err(ReplicaError::from)?;
    ensure_replica_schema(db_path)?;

    let mut conn = rooms::connect(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    initialize_replica_schema(&tx)?;

    let replica = tx
        .query_row(
            "SELECT room_id, name, members_json, authority_gateway_id,
                    authority_epoch, last_seq, event_bytes
               FROM hosted_room_replicas WHERE room_id=?",
            params![room_id],
            |r| {
                Ok((
                    r.get::<_, String>(1)?, // name
                    r.get::<_, String>(2)?, // members_json
                    r.get::<_, String>(3)?, // authority_gateway_id
                    r.get::<_, i64>(4)?,    // authority_epoch
                    r.get::<_, i64>(5)?,    // last_seq
                    r.get::<_, i64>(6)?,    // event_bytes
                ))
            },
        )
        .optional()?;
    let (name, members_json, previous_gateway, previous_epoch, last_seq, replica_event_bytes) =
        replica.ok_or_else(|| invalid("replica not found"))?;

    if previous_gateway == local_gateway {
        return Err(invalid("this gateway already holds the room authority"));
    }
    let room_exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM hosted_rooms WHERE room_id=?",
            params![room_id],
            |r| r.get(0),
        )
        .optional()?;
    if room_exists.is_some() {
        return Err(ReplicaError::Hosted(HostedRoomError::RoomConflict(
            "room_id already exists in the local authoritative store".into(),
        )));
    }
    let retired: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM hosted_room_retired_ids WHERE room_id=?",
            params![room_id],
            |r| r.get(0),
        )
        .optional()?;
    if retired.is_some() {
        return Err(ReplicaError::Hosted(HostedRoomError::RoomConflict(
            "room_id belongs to a disbanded room".into(),
        )));
    }

    let target_epoch = previous_epoch + 1;
    let claim_seq = last_seq + 1;
    let claim_event_id = format!("system:authority-claimed:{target_epoch}");
    let claim_actor_json = rooms::canonical_json(
        &json!({"kind": "system", "id": "authority-control"}),
        "actor",
        ACTOR_JSON_MAX_BYTES,
    )
    .map_err(ReplicaError::from)?;
    let claim_payload_json = rooms::canonical_json(
        &json!({
            "previous_gateway_id": previous_gateway,
            "authority_gateway_id": local_gateway,
            "authority_epoch": target_epoch,
            "promoted_from_replica": true,
            "reason": reason,
        }),
        "payload",
        MAX_EVENT_JSON_BYTES,
    )
    .map_err(ReplicaError::from)?;
    let claim_bytes = (claim_event_id.len()
        + "authority.claimed".len()
        + claim_actor_json.len()
        + claim_payload_json.len()) as i64;

    tx.execute(
        "INSERT INTO hosted_rooms
           (room_id, name, members_json, authority_gateway_id,
            authority_epoch, next_seq, event_bytes, revision,
            created_at, updated_at, disbanded_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, NULL)",
        params![
            room_id,
            name,
            members_json,
            local_gateway,
            target_epoch,
            claim_seq + 1,
            replica_event_bytes + claim_bytes,
            now,
            now
        ],
    )?;
    tx.execute(
        "INSERT INTO hosted_room_events
           (room_id, seq, event_id, kind, actor_json, authority_epoch,
            payload_json, created_at)
           SELECT room_id, seq, event_id, kind, actor_json,
                  authority_epoch, payload_json, created_at
             FROM hosted_room_replica_events WHERE room_id=?",
        params![room_id],
    )?;
    tx.execute(
        "INSERT INTO hosted_room_events
           (room_id, seq, event_id, kind, actor_json, authority_epoch,
            payload_json, created_at)
           VALUES (?, ?, ?, 'authority.claimed', ?, ?, ?, ?)",
        params![
            room_id,
            claim_seq,
            claim_event_id,
            claim_actor_json,
            target_epoch,
            claim_payload_json,
            now
        ],
    )?;
    tx.execute(
        "DELETE FROM hosted_room_replica_events WHERE room_id=?",
        params![room_id],
    )?;
    tx.execute(
        "DELETE FROM hosted_room_replicas WHERE room_id=?",
        params![room_id],
    )?;
    tx.commit()?;

    Ok(PromoteResult {
        room_id,
        authority_gateway_id: local_gateway,
        authority_epoch: target_epoch,
        previous_gateway_id: previous_gateway,
        previous_epoch,
        claim_seq,
        latest_seq: claim_seq,
    })
}

/// Fence THIS gateway's stale room authority against a proven newer epoch.
/// Appends `authority.lost` and adopts the observed lineage; idempotent for
/// repeated observations of the same lineage. Mirrors `demote_room`.
///
/// Like `promote_replica`, this fails closed until a stable install identity is
/// available (`local_authority_gateway_id`).
pub fn demote_room(
    db_path: &Path,
    room_id: &Value,
    observed_gateway_id: &Value,
    observed_epoch: &Value,
    now: Option<f64>,
) -> Result<DemoteResult> {
    let room_id = validate_identifier_value(Some(room_id), "room_id", MAX_ROOM_ID_CHARS)?;
    let observed_gateway_id = validate_identifier_value(
        Some(observed_gateway_id),
        "observed_gateway_id",
        MAX_ACTOR_ID_CHARS,
    )?;
    let observed_epoch = as_positive_int(Some(observed_epoch))
        .ok_or_else(|| invalid("observed_epoch must be a positive integer"))?;
    let now = now.unwrap_or_else(now_secs);
    let local_gateway = rooms::local_authority_gateway_id().map_err(ReplicaError::from)?;

    let mut conn = rooms::connect(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let row = tx
        .query_row(
            "SELECT authority_gateway_id, authority_epoch, next_seq
               FROM hosted_rooms WHERE room_id=? AND disbanded_at IS NULL",
            params![room_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?, // authority_gateway_id
                    r.get::<_, i64>(1)?,    // authority_epoch
                    r.get::<_, i64>(2)?,    // next_seq
                ))
            },
        )
        .optional()?;
    let (current_gateway, current_epoch, next_seq) =
        row.ok_or_else(|| invalid("room not found in the local authoritative store"))?;

    if current_gateway == observed_gateway_id && current_epoch == observed_epoch {
        tx.commit()?;
        return Ok(DemoteResult {
            room_id,
            authority_gateway_id: current_gateway,
            authority_epoch: current_epoch,
            idempotent: true,
        });
    }
    if observed_epoch <= current_epoch {
        return Err(epoch_regression(
            "observed epoch does not supersede the stored authority",
        ));
    }
    if current_gateway != local_gateway {
        return Err(invalid("room is not locally authoritative; nothing to demote"));
    }

    let seq = next_seq;
    let lost_actor_json = rooms::canonical_json(
        &json!({"kind": "system", "id": "authority-control"}),
        "actor",
        ACTOR_JSON_MAX_BYTES,
    )
    .map_err(ReplicaError::from)?;
    let lost_payload_json = rooms::canonical_json(
        &json!({
            "previous_gateway_id": current_gateway,
            "authority_gateway_id": observed_gateway_id,
            "authority_epoch": observed_epoch,
        }),
        "payload",
        MAX_EVENT_JSON_BYTES,
    )
    .map_err(ReplicaError::from)?;
    tx.execute(
        "INSERT INTO hosted_room_events
           (room_id, seq, event_id, kind, actor_json, authority_epoch,
            payload_json, created_at)
           VALUES (?, ?, ?, 'authority.lost', ?, ?, ?, ?)",
        params![
            room_id,
            seq,
            format!("system:authority-lost:{observed_epoch}"),
            lost_actor_json,
            observed_epoch,
            lost_payload_json,
            now
        ],
    )?;
    tx.execute(
        "UPDATE hosted_rooms
            SET authority_gateway_id=?, authority_epoch=?,
                next_seq=next_seq+1, revision=revision+1, updated_at=?
          WHERE room_id=?",
        params![observed_gateway_id, observed_epoch, now, room_id],
    )?;
    tx.commit()?;

    Ok(DemoteResult {
        room_id,
        authority_gateway_id: observed_gateway_id,
        authority_epoch: observed_epoch,
        idempotent: false,
    })
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
            path.push(format!("hermes_hosted_room_replicas_{tag}_{pid}_{n}.db"));
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

    // Build a replay page for room-1 covering seqs `start..=end` at `epoch`.
    fn page(start: i64, end: i64, epoch: i64, latest_seq: i64) -> Value {
        let events: Vec<Value> = (start..=end)
            .map(|seq| {
                json!({
                    "seq": seq,
                    "event_id": format!("evt-{seq}"),
                    "kind": "message.user",
                    "actor": {"kind": "user", "id": "u1"},
                    "authority_epoch": epoch,
                    "payload": {"text": format!("m{seq}")},
                    "created_at": 1000.0 + seq as f64,
                })
            })
            .collect();
        json!({
            "events": events,
            "latest_seq": latest_seq,
            "authority": {"gateway_id": "install:owner", "epoch": epoch},
        })
    }

    fn ingest(db: &Path, page: &Value) -> Result<IngestResult> {
        ingest_page(
            db,
            &json!("room-1"),
            &json!("Test Room"),
            &json!([{"id": "u1"}]),
            page,
            Some(2000.0),
        )
    }

    #[test]
    fn schema_init_creates_replica_tables() {
        let db = TempDb::new("schema");
        ensure_replica_schema(db.path()).unwrap();
        let conn = rooms::connect(db.path()).unwrap();
        for table in ["hosted_room_replicas", "hosted_room_replica_events"] {
            let found: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                    params![table],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            assert!(found.is_some(), "missing table {table}");
        }
    }

    #[test]
    fn ingest_roundtrip_and_idempotent_reingest() {
        let db = TempDb::new("roundtrip");
        let first = ingest(db.path(), &page(1, 3, 1, 3)).unwrap();
        assert_eq!(first.ingested, 3);
        assert_eq!(first.stored_seq, 3);
        assert!(first.caught_up);
        assert_eq!(first.authority.gateway_id, "install:owner");
        assert_eq!(first.authority.epoch, 1);

        // Re-ingesting the identical page stores nothing new but reports the
        // same coverage: idempotent.
        let second = ingest(db.path(), &page(1, 3, 1, 3)).unwrap();
        assert_eq!(second.ingested, 0);
        assert_eq!(second.stored_seq, 3);
        assert!(second.caught_up);

        // The store reflects exactly the three events once.
        let state = replica_state(db.path(), &json!("room-1")).unwrap();
        assert_eq!(state.last_seq, 3);
        assert_eq!(state.latest_seq, 3);
        assert_eq!(state.authority.epoch, 1);
        assert_eq!(state.name, "Test Room");

        // A follow-on page extends coverage.
        let third = ingest(db.path(), &page(4, 5, 1, 5)).unwrap();
        assert_eq!(third.ingested, 2);
        assert_eq!(third.stored_seq, 5);
    }

    #[test]
    fn ingest_refuses_sequence_gap() {
        let db = TempDb::new("gap_first");
        // First page must start at seq 1 (last_seq is 0); a page starting at
        // seq 2 skips a sequence the replica has not stored.
        let err = ingest(db.path(), &page(2, 3, 1, 3)).unwrap_err();
        assert!(err.is_gap(), "expected gap, got {err:?}");
        assert_eq!(err.to_string(), "page skips sequences the replica has not stored");

        // A non-contiguous page is rejected during validation.
        let broken = json!({
            "events": [
                {"seq": 1, "event_id": "a", "kind": "message.user",
                 "actor": {"kind": "user", "id": "u1"}, "payload": {}},
                {"seq": 3, "event_id": "b", "kind": "message.user",
                 "actor": {"kind": "user", "id": "u1"}, "payload": {}}
            ],
            "authority": {"gateway_id": "install:owner", "epoch": 1},
        });
        let err2 = ingest(db.path(), &broken).unwrap_err();
        assert!(err2.is_gap());
        assert_eq!(err2.to_string(), "page events must be contiguous");
    }

    #[test]
    fn ingest_refuses_authority_epoch_regression() {
        let db = TempDb::new("epoch");
        // Store the room at epoch 2 first.
        ingest(db.path(), &page(1, 2, 2, 2)).unwrap();
        // A later page carrying an older epoch is refused.
        let err = ingest(db.path(), &page(3, 3, 1, 3)).unwrap_err();
        assert!(err.is_epoch_regression(), "expected epoch regression, got {err:?}");
        assert_eq!(
            err.to_string(),
            "page authority epoch is older than the stored replica epoch"
        );
        // Nothing past seq 2 was stored.
        let state = replica_state(db.path(), &json!("room-1")).unwrap();
        assert_eq!(state.last_seq, 2);
        assert_eq!(state.authority.epoch, 2);
    }

    #[test]
    fn ingest_rejects_bad_page_shapes() {
        let db = TempDb::new("shape");
        // Missing authority stamp.
        let err = ingest(db.path(), &json!({"events": []})).unwrap_err();
        assert_eq!(err.to_string(), "page.authority is required for replication");
        // Non-positive epoch.
        let err2 = ingest(
            db.path(),
            &json!({"events": [], "authority": {"gateway_id": "install:owner", "epoch": 0}}),
        )
        .unwrap_err();
        assert_eq!(err2.to_string(), "page.authority.epoch must be a positive integer");
    }

    #[test]
    fn replica_state_missing_is_not_found() {
        let db = TempDb::new("missing");
        let err = replica_state(db.path(), &json!("no-such-room")).unwrap_err();
        assert_eq!(err.to_string(), "replica not found");
    }

    #[test]
    fn promote_fails_closed_without_install_identity() {
        // local_authority_gateway_id fails while the install-id module is
        // unported, so the takeover primitive fails closed before touching the
        // store, matching Python's behavior when the install id is unavailable.
        let db = TempDb::new("promote");
        ingest(db.path(), &page(1, 2, 1, 2)).unwrap();
        let err = promote_replica(db.path(), &json!("room-1"), None, Some(3000.0)).unwrap_err();
        assert_eq!(err.to_string(), "stable gateway install identity is unavailable");
        // The replica is untouched: still recoverable.
        let state = replica_state(db.path(), &json!("room-1")).unwrap();
        assert_eq!(state.last_seq, 2);
    }
}
