//! Port of gateway/hosted_room_policy_checkpoint.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Durable bounded policy projection for hosted Group Chat preparation. The
//! append-only room log (`hosted_rooms_log`) stays the user-visible source of
//! truth; this module materializes only the state needed to choose and
//! reconstruct the next active discussion, so a busy room does not replay its
//! full history every poll. It owns its own set of `hosted_room_policy_*`
//! SQLite tables (all created here, `IF NOT EXISTS`) inside the same shared
//! `state.db`, and replays the room log forward exactly once per durable cursor
//! (`sync`), projecting each event into a bounded active-discussion index plus a
//! bounded per-thread transcript of references back into the log. `snapshot`
//! returns the oldest still-active discussion with its committed transcript and
//! member watermarks; `events_for_task` reconstructs one bounded discussion for
//! terminal handling; `publication_exists` answers whether one driver outcome is
//! already durable. Stored active events are serialized with Python's
//! `json.dumps(sort_keys=True, ensure_ascii=True, separators=(",",":"))`
//! semantics so the cached copy round-trips byte-for-byte with the Python store.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::{json, Map, Value};

use crate::hosted_rooms_log::{self, Event, HostedRoomError, MAX_LOG_LIMIT};

// ---------------------------------------------------------------------------
// Constants (mirror the module-level constants in the Python source).
// ---------------------------------------------------------------------------

/// Hard cap on the number of active-discussion events one projection may hold.
pub const MAX_ACTIVE_POLICY_EVENTS: i64 = 64;
/// Hard cap on committed message references kept per thread transcript.
pub const MAX_THREAD_TRANSCRIPT_EVENTS: i64 = 24;
/// Bump to force a one-time transcript backfill from the durable log.
const TRANSCRIPT_SCHEMA_VERSION: i64 = 1;

/// Terminal turn outcomes. Mirrors `_TERMINAL_KINDS`.
const TERMINAL_KINDS: &[&str] = &[
    "turn.settled",
    "turn.failed",
    "turn.cancelled",
    "turn.deferred",
];

fn is_terminal(kind: &str) -> bool {
    TERMINAL_KINDS.contains(&kind)
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// One error from the policy-checkpoint layer.
///
/// `RoomNotFound` mirrors `hosted_rooms.RoomNotFoundError` (both the ones this
/// module raises directly and the ones bubbling up from `read_events`).
/// `Runtime` mirrors the plain `RuntimeError`s the Python raises when an
/// invariant breaks (cursor ahead of the log, cursor did not advance, bound
/// exceeded). `Room` carries any other `HostedRoomError` from the room-log
/// layer verbatim, and `Sqlite` any underlying SQLite failure.
#[derive(Debug)]
pub enum CheckpointError {
    /// `hosted_rooms.RoomNotFoundError`.
    RoomNotFound(String),
    /// A plain `RuntimeError` from the Python source.
    Runtime(String),
    /// Any other room-log error propagated from `hosted_rooms_log`.
    Room(HostedRoomError),
    /// Underlying SQLite failure.
    Sqlite(rusqlite::Error),
}

impl CheckpointError {
    /// True when a Python caller's `isinstance(exc, RoomNotFoundError)` would be,
    /// including a `RoomNotFoundError`/`RoomHistoryExpiredError` from
    /// `read_events`.
    pub fn is_room_not_found(&self) -> bool {
        match self {
            CheckpointError::RoomNotFound(_) => true,
            CheckpointError::Room(err) => err.is_room_not_found(),
            _ => false,
        }
    }
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckpointError::RoomNotFound(m) | CheckpointError::Runtime(m) => f.write_str(m),
            CheckpointError::Room(err) => write!(f, "{err}"),
            CheckpointError::Sqlite(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CheckpointError {}

impl From<rusqlite::Error> for CheckpointError {
    fn from(err: rusqlite::Error) -> Self {
        CheckpointError::Sqlite(err)
    }
}

impl From<HostedRoomError> for CheckpointError {
    fn from(err: HostedRoomError) -> Self {
        // A room-not-found (or history-expired) from read_events must keep
        // reporting as room-not-found, matching Python's exception subclassing.
        if err.is_room_not_found() {
            CheckpointError::RoomNotFound(err.to_string())
        } else {
            CheckpointError::Room(err)
        }
    }
}

type Result<T> = std::result::Result<T, CheckpointError>;

// ---------------------------------------------------------------------------
// Output types.
// ---------------------------------------------------------------------------

/// Bounded active policy input at one durable room-log cursor. Mirrors the
/// frozen `PolicySnapshot` dataclass. `watermarks` is keyed by
/// `(thread_id, member_id)`, matching the Python `Mapping[tuple[str, str], int]`.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicySnapshot {
    pub through_seq: i64,
    pub stopped_through_seq: i64,
    pub events: Vec<Event>,
    pub watermarks: BTreeMap<(String, String), i64>,
}

// ---------------------------------------------------------------------------
// JSON helpers.
// ---------------------------------------------------------------------------

/// Escape every non-ASCII scalar to a `\uXXXX` sequence (astral chars as a
/// UTF-16 surrogate pair), matching Python's `json.dumps(ensure_ascii=True)`.
/// JSON structural bytes are all ASCII and non-ASCII only appears inside string
/// literals, so escaping the whole serialized string is safe.
fn ensure_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let cp = ch as u32;
        if cp < 0x80 {
            out.push(ch);
        } else if cp > 0xFFFF {
            let v = cp - 0x10000;
            let hi = 0xD800 + (v >> 10);
            let lo = 0xDC00 + (v & 0x3FF);
            out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
        } else {
            out.push_str(&format!("\\u{cp:04x}"));
        }
    }
    out
}

/// Serialize an event the way `_event_json` does: sorted keys (serde_json's
/// default `Map` is a `BTreeMap`), compact `(",",":")` separators, ASCII-safe.
fn encode_event(event: &Event) -> String {
    let value = json!({
        "room_id": event.room_id,
        "seq": event.seq,
        "event_id": event.event_id,
        "kind": event.kind,
        "actor": event.actor,
        "authority_epoch": event.authority_epoch,
        "payload": event.payload,
        "created_at": event.created_at,
        "idempotent": event.idempotent,
    });
    ensure_ascii(&serde_json::to_string(&value).unwrap_or_default())
}

/// Rebuild an [`Event`] from a stored `event_json` value.
fn value_to_event(v: &Value) -> Event {
    Event {
        room_id: v
            .get("room_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        seq: v.get("seq").and_then(|x| x.as_i64()).unwrap_or(0),
        event_id: v
            .get("event_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        kind: v
            .get("kind")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        actor: v.get("actor").cloned().unwrap_or(Value::Null),
        authority_epoch: v.get("authority_epoch").and_then(|x| x.as_i64()),
        payload: v.get("payload").cloned().unwrap_or(Value::Null),
        created_at: v.get("created_at").and_then(|x| x.as_f64()).unwrap_or(0.0),
        idempotent: v
            .get("idempotent")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    }
}

fn decode_event(json_str: &str) -> Result<Event> {
    let v: Value = serde_json::from_str(json_str).map_err(|e| {
        CheckpointError::Runtime(format!("stored policy event is not valid JSON: {e}"))
    })?;
    Ok(value_to_event(&v))
}

/// `str(value or "")` from Python: falsy (missing/null/""/0/false/empty
/// container) becomes "", a string passes through, and the realistic scalar
/// cases coerce the way `str()` would.
fn value_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => String::new(),
        Some(Value::Number(n)) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Some(other) => {
            let empty_container = other.as_array().map(|a| a.is_empty()).unwrap_or(false)
                || other.as_object().map(|o| o.is_empty()).unwrap_or(false);
            if empty_container {
                String::new()
            } else {
                other.to_string()
            }
        }
    }
}

/// `int(value or 0)` from Python for the realistic scalar cases.
fn value_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Row -> Event.
// ---------------------------------------------------------------------------

/// Build an [`Event`] from a `hosted_room_events` row selected as
/// (room_id, seq, event_id, kind, actor_json, authority_epoch, payload_json,
/// created_at). Mirrors `_event_from_room_row`.
fn event_from_room_row(row: &Row) -> rusqlite::Result<Event> {
    let actor_json: String = row.get(4)?;
    let payload_json: String = row.get(6)?;
    Ok(Event {
        room_id: row.get(0)?,
        seq: row.get(1)?,
        event_id: row.get(2)?,
        kind: row.get(3)?,
        actor: serde_json::from_str(&actor_json).unwrap_or(Value::Null),
        authority_epoch: row.get(5)?,
        payload: serde_json::from_str(&payload_json).unwrap_or(Value::Null),
        created_at: row.get(7)?,
        idempotent: false,
    })
}

/// The room-log columns `event_from_room_row` reads, in order.
const ROOM_EVENT_COLS: &str =
    "room_id, seq, event_id, kind, actor_json, authority_epoch, payload_json, created_at";

// ---------------------------------------------------------------------------
// Projection store helpers (free functions over a live connection/transaction).
// ---------------------------------------------------------------------------

/// Cache one event by reference into the bounded active projection. Mirrors
/// `_store_active_event`.
fn store_active_event(
    conn: &Connection,
    event: &Event,
    thread_id: &str,
    discussion_event_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO hosted_room_policy_events(
                room_id, thread_id, discussion_event_id, seq, event_json
             ) VALUES (?, ?, ?, ?, ?)",
        params![
            event.room_id,
            thread_id,
            discussion_event_id,
            event.seq,
            encode_event(event)
        ],
    )?;
    Ok(())
}

/// Record one transcript reference and trim the thread to its bound. Mirrors
/// `_store_transcript_event`.
fn store_transcript_event(
    conn: &Connection,
    event: &Event,
    thread_id: &str,
    settled_seq: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO hosted_room_policy_transcript(
                room_id, thread_id, seq, kind, settled_seq
             ) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(room_id, thread_id, seq) DO UPDATE SET
                 settled_seq=COALESCE(
                     excluded.settled_seq,
                     hosted_room_policy_transcript.settled_seq
                 )",
        params![event.room_id, thread_id, event.seq, event.kind, settled_seq],
    )?;
    if event.kind == "message.user" || event.kind == "message.member" {
        let cutoff: Option<i64> = conn
            .query_row(
                "SELECT seq FROM hosted_room_policy_transcript
                   WHERE room_id=? AND thread_id=?
                     AND kind IN ('message.user', 'message.member')
                   ORDER BY seq DESC LIMIT 1 OFFSET ?",
                params![event.room_id, thread_id, MAX_THREAD_TRANSCRIPT_EVENTS - 1],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(cutoff) = cutoff {
            conn.execute(
                "DELETE FROM hosted_room_policy_transcript
                   WHERE room_id=? AND thread_id=? AND seq<?",
                params![event.room_id, thread_id, cutoff],
            )?;
        }
    }
    Ok(())
}

/// Migrate bounded committed thread history from the durable room log. Mirrors
/// `_backfill_transcript`.
fn backfill_transcript(conn: &Connection, room_id: &str, through_seq: i64) -> Result<()> {
    if through_seq <= 0 {
        return Ok(());
    }
    let mut settled_seq_by_message: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT seq, payload_json FROM hosted_room_events
               WHERE room_id=? AND seq<=? AND kind='turn.settled'
               ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![room_id, through_seq], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (seq, payload_json) = row?;
            let payload: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
            let message_event_id = value_str(payload.get("message_event_id"));
            if !message_event_id.is_empty() {
                settled_seq_by_message.insert(message_event_id, seq);
            }
        }
    }
    let events: Vec<Event> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {ROOM_EVENT_COLS}
               FROM hosted_room_events
               WHERE room_id=? AND seq<=?
                 AND kind IN ('message.user', 'message.member')
               ORDER BY seq"
        ))?;
        let rows = stmt.query_map(params![room_id, through_seq], event_from_room_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for event in events {
        if event.kind == "message.member" && !settled_seq_by_message.contains_key(&event.event_id) {
            continue;
        }
        let thread_id = value_str(event.payload.get("thread_id"));
        if !thread_id.is_empty() {
            store_transcript_event(
                conn,
                &event,
                &thread_id,
                settled_seq_by_message.get(&event.event_id).copied(),
            )?;
        }
    }
    Ok(())
}

/// Load the committed transcript for a thread by re-reading the durable log.
/// Mirrors `_transcript_events`.
fn transcript_events(conn: &Connection, room_id: &str, thread_id: &str) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "WITH transcript_events(seq) AS (
                SELECT seq FROM hosted_room_policy_transcript
                 WHERE room_id=? AND thread_id=?
                UNION ALL
                SELECT settled_seq FROM hosted_room_policy_transcript
                 WHERE room_id=? AND thread_id=? AND settled_seq IS NOT NULL
            )
            SELECT events.room_id, events.seq, events.event_id,
                   events.kind, events.actor_json,
                   events.authority_epoch, events.payload_json,
                   events.created_at
            FROM transcript_events
            JOIN hosted_room_events AS events
              ON events.room_id=? AND events.seq=transcript_events.seq
            ORDER BY events.seq",
    )?;
    let rows = stmt.query_map(
        params![room_id, thread_id, room_id, thread_id, room_id],
        event_from_room_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Load up to `MAX_ACTIVE_POLICY_EVENTS + 1` active events for one discussion,
/// so the caller can detect a bound overflow.
fn active_events(
    conn: &Connection,
    room_id: &str,
    discussion_event_id: &str,
) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT event_json FROM hosted_room_policy_events
           WHERE room_id=? AND discussion_event_id=?
           ORDER BY seq LIMIT ?",
    )?;
    let rows = stmt.query_map(
        params![room_id, discussion_event_id, MAX_ACTIVE_POLICY_EVENTS + 1],
        |r| r.get::<_, String>(0),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_event(&row?)?);
    }
    Ok(out)
}

/// Merge transcript and active events by seq (active wins), sorted by seq.
/// Mirrors the `events_by_seq` dict comprehension + `sorted` in both readers.
fn merge_by_seq(transcript: Vec<Event>, active: Vec<Event>) -> Vec<Event> {
    let mut by_seq: BTreeMap<i64, Event> = BTreeMap::new();
    for event in transcript {
        by_seq.insert(event.seq, event);
    }
    for event in active {
        by_seq.insert(event.seq, event);
    }
    by_seq.into_values().collect()
}

/// Fold one durable room-log event into the projection. Mirrors `_apply_event`.
fn apply_event(conn: &Connection, event: &Event) -> Result<()> {
    let room_id = event.room_id.as_str();
    let seq = event.seq;
    let kind = event.kind.as_str();
    let empty = Map::new();
    let payload = event.payload.as_object().unwrap_or(&empty);

    if kind == "message.user" {
        let thread_id = value_str(payload.get("thread_id"));
        let event_id = event.event_id.clone();
        if thread_id.is_empty() || event_id.is_empty() {
            return Ok(());
        }
        conn.execute(
            "INSERT INTO hosted_room_policy_threads(
                    room_id, thread_id, discussion_event_id,
                    latest_user_seq, completed
                ) VALUES (?, ?, ?, ?, 0)
                ON CONFLICT(room_id, thread_id) DO UPDATE SET
                    discussion_event_id=excluded.discussion_event_id,
                    latest_user_seq=excluded.latest_user_seq,
                    completed=0",
            params![room_id, thread_id, event_id, seq],
        )?;
        store_active_event(conn, event, &thread_id, &event_id)?;
        store_transcript_event(conn, event, &thread_id, None)?;
        return Ok(());
    }

    if kind == "message.member" || is_terminal(kind) {
        let thread_id = value_str(payload.get("thread_id"));
        let discussion_event_id = value_str(payload.get("discussion_event_id"));
        let source: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM hosted_room_policy_events
                   WHERE room_id=? AND discussion_event_id=? LIMIT 1",
                params![room_id, discussion_event_id],
                |r| r.get(0),
            )
            .optional()?;
        if source.is_none() {
            return Ok(());
        }
        store_active_event(conn, event, &thread_id, &discussion_event_id)?;
        if is_terminal(kind) {
            let task_id = value_str(payload.get("task_id"));
            let execution_generation = if kind == "turn.deferred" {
                value_int(payload.get("execution_generation"))
            } else {
                0
            };
            if !task_id.is_empty() {
                conn.execute(
                    "INSERT OR IGNORE INTO hosted_room_policy_publications(
                            room_id, task_id, kind, execution_generation, seq
                        ) VALUES (?, ?, ?, ?, ?)",
                    params![room_id, task_id, kind, execution_generation, seq],
                )?;
            }
            let member_id = value_str(payload.get("member_id"));
            let mut seen_through_seq = value_int(payload.get("seen_through_seq"));
            let message_event_id = value_str(payload.get("message_event_id"));
            if kind == "turn.settled" && !message_event_id.is_empty() {
                let stored: Vec<String> = {
                    let mut stmt = conn.prepare(
                        "SELECT seq, event_json FROM hosted_room_policy_events
                           WHERE room_id=? AND discussion_event_id=?",
                    )?;
                    let rows = stmt.query_map(params![room_id, discussion_event_id], |r| {
                        r.get::<_, String>(1)
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                let mut committed: Option<Event> = None;
                for json_str in &stored {
                    let candidate = decode_event(json_str)?;
                    if candidate.event_id == message_event_id {
                        committed = Some(candidate);
                        break;
                    }
                }
                if let Some(committed) = committed {
                    seen_through_seq = seen_through_seq.max(committed.seq);
                    store_transcript_event(conn, &committed, &thread_id, Some(event.seq))?;
                }
            }
            if !member_id.is_empty() && seen_through_seq > 0 {
                conn.execute(
                    "INSERT INTO hosted_room_policy_watermarks(
                            room_id, thread_id, member_id, seen_through_seq
                        ) VALUES (?, ?, ?, ?)
                        ON CONFLICT(room_id, thread_id, member_id) DO UPDATE SET
                            seen_through_seq=MAX(
                                hosted_room_policy_watermarks.seen_through_seq,
                                excluded.seen_through_seq
                            )",
                    params![room_id, thread_id, member_id, seen_through_seq],
                )?;
            }
        }
        return Ok(());
    }

    if kind == "room.activity" {
        let thread_id = value_str(payload.get("thread_id"));
        let discussion_event_id = value_str(payload.get("discussion_event_id"));
        conn.execute(
            "DELETE FROM hosted_room_policy_events
               WHERE room_id=? AND discussion_event_id=?",
            params![room_id, discussion_event_id],
        )?;
        conn.execute(
            "DELETE FROM hosted_room_policy_threads
               WHERE room_id=? AND thread_id=?",
            params![room_id, thread_id],
        )?;
        return Ok(());
    }

    if kind == "room.stop_requested" {
        conn.execute(
            "UPDATE hosted_room_policy_cursors
               SET stopped_through_seq=MAX(stopped_through_seq, ?)
               WHERE room_id=?",
            params![seq, room_id],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The checkpoint store.
// ---------------------------------------------------------------------------

/// Incrementally index room policy without compacting visible history. Mirrors
/// `HostedRoomPolicyCheckpoint`.
pub struct HostedRoomPolicyCheckpoint {
    db_path: PathBuf,
}

impl HostedRoomPolicyCheckpoint {
    /// Open (initializing the projection schema) against `db_path`. Mirrors
    /// `__init__` + `_initialize`.
    pub fn new(db_path: impl Into<PathBuf>) -> Result<Self> {
        let store = Self {
            db_path: db_path.into(),
        };
        store.initialize()?;
        Ok(store)
    }

    /// The room store path.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Open a connection with a 10s busy timeout and best-effort WAL, exactly
    /// like the Python `_connect` (which calls `apply_wal_with_fallback`). Unlike
    /// the room-log `connect`, this does not create the `hosted_rooms`/
    /// `hosted_room_events` schema: in production those already exist, and the
    /// projection tables carry no foreign keys into them.
    fn connect(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&self.db_path)?;
        let _ = conn.busy_timeout(Duration::from_secs(10));
        // Best-effort WAL, matching apply_wal_with_fallback's DELETE fallback.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        Ok(conn)
    }

    /// Create the projection tables and indexes. Mirrors `_initialize`.
    fn initialize(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS hosted_room_policy_cursors (
                room_id TEXT PRIMARY KEY,
                through_seq INTEGER NOT NULL DEFAULT 0,
                stopped_through_seq INTEGER NOT NULL DEFAULT 0,
                updated_at REAL NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS hosted_room_policy_threads (
                room_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                discussion_event_id TEXT NOT NULL,
                latest_user_seq INTEGER NOT NULL,
                completed INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(room_id, thread_id)
            );
            CREATE INDEX IF NOT EXISTS idx_hosted_room_policy_pending
                ON hosted_room_policy_threads(
                    room_id, completed, latest_user_seq, thread_id
                );
            CREATE TABLE IF NOT EXISTS hosted_room_policy_events (
                room_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                discussion_event_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_json TEXT NOT NULL,
                PRIMARY KEY(room_id, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_hosted_room_policy_events_active
                ON hosted_room_policy_events(
                    room_id, discussion_event_id, seq
                );
            CREATE TABLE IF NOT EXISTS hosted_room_policy_watermarks (
                room_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                member_id TEXT NOT NULL,
                seen_through_seq INTEGER NOT NULL,
                PRIMARY KEY(room_id, thread_id, member_id)
            );
            CREATE TABLE IF NOT EXISTS hosted_room_policy_publications (
                room_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                execution_generation INTEGER NOT NULL DEFAULT 0,
                seq INTEGER NOT NULL,
                PRIMARY KEY(room_id, task_id, kind, execution_generation)
            );
            CREATE TABLE IF NOT EXISTS hosted_room_policy_transcript (
                room_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                settled_seq INTEGER,
                PRIMARY KEY(room_id, thread_id, seq)
            );
            CREATE TABLE IF NOT EXISTS hosted_room_policy_transcript_state (
                room_id TEXT PRIMARY KEY,
                schema_version INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    /// Materialize each unseen event exactly once by durable cursor. Mirrors
    /// `sync`.
    pub fn sync(&self, room_id: &str, latest_seq: i64) -> Result<i64> {
        let mut cursor: i64;
        {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM hosted_rooms WHERE room_id=?",
                    params![room_id],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(CheckpointError::RoomNotFound(
                    "hosted room not found".into(),
                ));
            }
            tx.execute(
                "INSERT OR IGNORE INTO hosted_room_policy_cursors(
                        room_id, through_seq, stopped_through_seq, updated_at
                    ) VALUES (?, 0, 0, 0)",
                params![room_id],
            )?;
            cursor = tx.query_row(
                "SELECT through_seq FROM hosted_room_policy_cursors WHERE room_id=?",
                params![room_id],
                |r| r.get(0),
            )?;
            let schema_version: Option<i64> = tx
                .query_row(
                    "SELECT schema_version
                       FROM hosted_room_policy_transcript_state WHERE room_id=?",
                    params![room_id],
                    |r| r.get(0),
                )
                .optional()?;
            if schema_version.is_none_or(|v| v < TRANSCRIPT_SCHEMA_VERSION) {
                backfill_transcript(&tx, room_id, cursor)?;
                tx.execute(
                    "INSERT INTO hosted_room_policy_transcript_state(
                            room_id, schema_version
                        ) VALUES (?, ?)
                        ON CONFLICT(room_id) DO UPDATE SET
                            schema_version=excluded.schema_version",
                    params![room_id, TRANSCRIPT_SCHEMA_VERSION],
                )?;
            }
            tx.commit()?;
        }
        if cursor > latest_seq {
            return Err(CheckpointError::Runtime(
                "room policy cursor is ahead of the durable log".into(),
            ));
        }

        while cursor < latest_seq {
            let page = hosted_rooms_log::read_events(
                &self.db_path,
                room_id,
                cursor,
                MAX_LOG_LIMIT,
                false,
            )?;
            let rows = page.events;
            let next_cursor = if page.cursor != 0 {
                page.cursor
            } else {
                cursor
            };
            if rows.is_empty() || next_cursor <= cursor {
                return Err(CheckpointError::Runtime(
                    "hosted room policy cursor did not advance".into(),
                ));
            }
            {
                let mut conn = self.connect()?;
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let exists: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM hosted_rooms WHERE room_id=?",
                        params![room_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Err(CheckpointError::RoomNotFound(
                        "hosted room not found".into(),
                    ));
                }
                for event in &rows {
                    apply_event(&tx, event)?;
                }
                let updated_at = rows.last().map(|e| e.created_at).unwrap_or(0.0);
                let updated = tx.execute(
                    "UPDATE hosted_room_policy_cursors
                       SET through_seq=?, updated_at=? WHERE room_id=?",
                    params![next_cursor, updated_at, room_id],
                )?;
                if updated != 1 {
                    return Err(CheckpointError::Runtime(
                        "room policy cursor disappeared during replay".into(),
                    ));
                }
                tx.commit()?;
            }
            cursor = next_cursor;
        }
        Ok(cursor)
    }

    /// Return only the oldest active discussion and its watermark set. Mirrors
    /// `snapshot`.
    pub fn snapshot(&self, room_id: &str, latest_seq: i64) -> Result<PolicySnapshot> {
        let through_seq = self.sync(room_id, latest_seq)?;
        let conn = self.connect()?;
        let stopped_through_seq: i64 = conn.query_row(
            "SELECT stopped_through_seq FROM hosted_room_policy_cursors WHERE room_id=?",
            params![room_id],
            |r| r.get(0),
        )?;
        let thread: Option<(String, String)> = conn
            .query_row(
                "SELECT thread_id, discussion_event_id
                   FROM hosted_room_policy_threads
                   WHERE room_id=? AND completed=0 AND latest_user_seq>?
                   ORDER BY latest_user_seq, thread_id LIMIT 1",
                params![room_id, stopped_through_seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (thread_id, discussion_event_id) = match thread {
            None => {
                return Ok(PolicySnapshot {
                    through_seq,
                    stopped_through_seq,
                    events: Vec::new(),
                    watermarks: BTreeMap::new(),
                });
            }
            Some(thread) => thread,
        };
        let active = active_events(&conn, room_id, &discussion_event_id)?;
        if active.len() as i64 > MAX_ACTIVE_POLICY_EVENTS {
            return Err(CheckpointError::Runtime(
                "active room policy projection exceeded its bound".into(),
            ));
        }
        let transcript = transcript_events(&conn, room_id, &thread_id)?;
        let mut watermarks: BTreeMap<(String, String), i64> = BTreeMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT member_id, seen_through_seq
                   FROM hosted_room_policy_watermarks
                   WHERE room_id=? AND thread_id=?",
            )?;
            let rows = stmt.query_map(params![room_id, thread_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (member_id, seen_through_seq) = row?;
                watermarks.insert((thread_id.clone(), member_id), seen_through_seq);
            }
        }
        Ok(PolicySnapshot {
            through_seq,
            stopped_through_seq,
            events: merge_by_seq(transcript, active),
            watermarks,
        })
    }

    /// Return whether one exact driver outcome is already in the room log.
    /// Mirrors `publication_exists`.
    pub fn publication_exists(
        &self,
        room_id: &str,
        task_id: &str,
        status: &str,
        execution_generation: i64,
    ) -> Result<bool> {
        let kind = format!("turn.{status}");
        let generation = if status == "deferred" {
            execution_generation
        } else {
            0
        };
        let conn = self.connect()?;
        let row: Option<i64> = if status == "deferred" {
            conn.query_row(
                "SELECT 1 FROM hosted_room_policy_publications
                   WHERE room_id=? AND task_id=? AND kind=?
                     AND execution_generation=?",
                params![room_id, task_id, kind, generation],
                |r| r.get(0),
            )
            .optional()?
        } else {
            conn.query_row(
                "SELECT 1 FROM hosted_room_policy_publications
                   WHERE room_id=? AND task_id=? AND kind IN (
                       'turn.settled', 'turn.failed', 'turn.cancelled'
                   )",
                params![room_id, task_id],
                |r| r.get(0),
            )
            .optional()?
        };
        Ok(row.is_some())
    }

    /// Load one bounded discussion projection for terminal reconstruction.
    /// Mirrors `events_for_task`.
    pub fn events_for_task(&self, room_id: &str, source_event_seq: i64) -> Result<Vec<Event>> {
        let conn = self.connect()?;
        let source: Option<(String, String)> = conn
            .query_row(
                "SELECT discussion_event_id, thread_id
                   FROM hosted_room_policy_events
                   WHERE room_id=? AND seq=?",
                params![room_id, source_event_seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (discussion_event_id, thread_id) = match source {
            None => return Ok(Vec::new()),
            Some(source) => source,
        };
        let active = active_events(&conn, room_id, &discussion_event_id)?;
        let transcript = transcript_events(&conn, room_id, &thread_id)?;
        if active.len() as i64 > MAX_ACTIVE_POLICY_EVENTS {
            return Err(CheckpointError::Runtime(
                "task policy projection exceeded its bound".into(),
            ));
        }
        Ok(merge_by_seq(transcript, active))
    }

    /// Drop any completed projections left by an interrupted sync. Mirrors
    /// `compact_completed`.
    pub fn compact_completed(&self, room_id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let completed: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT discussion_event_id FROM hosted_room_policy_threads
                   WHERE room_id=? AND completed=1",
            )?;
            let rows = stmt.query_map(params![room_id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for discussion_event_id in &completed {
            tx.execute(
                "DELETE FROM hosted_room_policy_events
                   WHERE room_id=? AND discussion_event_id=?",
                params![room_id, discussion_event_id],
            )?;
        }
        tx.execute(
            "DELETE FROM hosted_room_policy_threads
               WHERE room_id=? AND completed=1",
            params![room_id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosted_rooms_log;

    const GW: &str = "install:test";

    fn unique_db() -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        p.push(format!(
            "hermes_policy_ckpt_{}_{}.db",
            std::process::id(),
            nanos
        ));
        p
    }

    /// Remove the db plus its WAL/SHM sidecars.
    fn cleanup(db: &Path) {
        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_file(format!("{}-wal", db.display()));
        let _ = std::fs::remove_file(format!("{}-shm", db.display()));
    }

    fn make_room(db: &Path) {
        let members = json!([{"id": "user1", "kind": "user"}]);
        hosted_rooms_log::create_room(db, "room1", "Room", &members, GW, Some(1.0)).unwrap();
    }

    fn append(
        db: &Path,
        event_id: &str,
        kind: &str,
        actor: Value,
        payload: Value,
        now: f64,
    ) -> Event {
        hosted_rooms_log::append_event(
            db,
            "room1",
            event_id,
            kind,
            &actor,
            &payload,
            Some(GW),
            Some(1),
            Some(now),
        )
        .unwrap()
    }

    fn user_msg(db: &Path, event_id: &str, thread_id: &str, now: f64) -> Event {
        append(
            db,
            event_id,
            "message.user",
            json!({"kind": "user", "id": "user1"}),
            json!({"thread_id": thread_id}),
            now,
        )
    }

    fn member_msg(db: &Path, event_id: &str, thread_id: &str, discussion: &str, now: f64) -> Event {
        append(
            db,
            event_id,
            "message.member",
            json!({"kind": "member", "id": "member1"}),
            json!({"thread_id": thread_id, "discussion_event_id": discussion}),
            now,
        )
    }

    fn gateway_event(db: &Path, event_id: &str, kind: &str, payload: Value, now: f64) -> Event {
        append(
            db,
            event_id,
            kind,
            json!({"kind": "gateway", "id": GW}),
            payload,
            now,
        )
    }

    #[test]
    fn schema_init_creates_projection_tables() {
        let db = unique_db();
        let _ckpt = HostedRoomPolicyCheckpoint::new(&db).unwrap();
        let conn = Connection::open(&db).unwrap();
        for table in [
            "hosted_room_policy_cursors",
            "hosted_room_policy_threads",
            "hosted_room_policy_events",
            "hosted_room_policy_watermarks",
            "hosted_room_policy_publications",
            "hosted_room_policy_transcript",
            "hosted_room_policy_transcript_state",
        ] {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                    params![table],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            assert!(exists.is_some(), "missing table {table}");
        }
        drop(conn);
        cleanup(&db);
    }

    #[test]
    fn checkpoint_write_and_read_roundtrip() {
        let db = unique_db();
        make_room(&db);
        let ev = user_msg(&db, "u1", "t1", 2.0);
        assert_eq!(ev.seq, 1);

        let ckpt = HostedRoomPolicyCheckpoint::new(&db).unwrap();
        let snap = ckpt.snapshot("room1", 1).unwrap();
        assert_eq!(snap.through_seq, 1);
        assert_eq!(snap.stopped_through_seq, 0);
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.events[0].event_id, "u1");
        assert_eq!(snap.events[0].kind, "message.user");
        assert!(snap.watermarks.is_empty());

        // A second sync at the same cursor is a no-op and stays idempotent.
        assert_eq!(ckpt.sync("room1", 1).unwrap(), 1);

        // events_for_task reconstructs the same bounded projection by seq.
        let events = ckpt.events_for_task("room1", 1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, "u1");

        cleanup(&db);
    }

    #[test]
    fn missing_room_reports_room_not_found() {
        let db = unique_db();
        let ckpt = HostedRoomPolicyCheckpoint::new(&db).unwrap();
        // No hosted_rooms row exists at all.
        make_room(&db);
        let err = ckpt.sync("ghost", 0).unwrap_err();
        assert!(err.is_room_not_found());
        cleanup(&db);
    }

    #[test]
    fn bounded_projection_rejects_overflow() {
        let db = unique_db();
        make_room(&db);
        user_msg(&db, "u1", "t1", 2.0);
        // 65 member messages on one discussion -> 66 active events (> 64 + 1).
        for i in 0..65 {
            member_msg(&db, &format!("m{i}"), "t1", "u1", 3.0 + i as f64);
        }
        let latest = 66; // seq 1 user + seqs 2..=66 members

        let ckpt = HostedRoomPolicyCheckpoint::new(&db).unwrap();
        let err = ckpt.snapshot("room1", latest).unwrap_err();
        match err {
            CheckpointError::Runtime(msg) => {
                assert!(msg.contains("exceeded its bound"), "unexpected: {msg}");
            }
            other => panic!("expected Runtime bound error, got {other:?}"),
        }
        cleanup(&db);
    }

    #[test]
    fn next_active_discussion_selection() {
        let db = unique_db();
        make_room(&db);
        user_msg(&db, "u1", "t1", 2.0); // seq 1
        user_msg(&db, "u2", "t2", 3.0); // seq 2

        let ckpt = HostedRoomPolicyCheckpoint::new(&db).unwrap();
        // Oldest active discussion (lowest latest_user_seq) is t1.
        let snap = ckpt.snapshot("room1", 2).unwrap();
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.events[0].event_id, "u1");

        // room.activity retires t1's projection; selection falls to t2.
        gateway_event(
            &db,
            "a1",
            "room.activity",
            json!({"thread_id": "t1", "discussion_event_id": "u1"}),
            4.0,
        ); // seq 3
        let snap = ckpt.snapshot("room1", 3).unwrap();
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.events[0].event_id, "u2");

        cleanup(&db);
    }
}
