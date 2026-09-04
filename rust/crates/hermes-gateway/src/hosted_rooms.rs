//! Port of the room-link record layer of gateway/hosted_rooms.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! This is only the room-link store slice of hosted_rooms.py. The full Python
//! module owns hosted-room identity plus an append-only event log and several
//! sibling tables (rooms, events, retired ids, remote runs, revoked grants,
//! peer reservations); none of that is ported here. What lives here is the
//! private `hosted_room_links` SQLite table and the three record functions the
//! `hosted_room_links` wrapper needs: list, upsert (with the MAX_LINKS
//! capacity cap), and status update, plus the DB-open helper that ensures the
//! link schema and turns on WAL. The row's `catalog_json` is kept as opaque
//! serialized JSON text, exactly as Python stores it, so this slice does not
//! depend on the typed catalog owned by hosted_room_peer.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// Cap on stored room links. The value lives in the Python wrapper
/// (`gateway/hosted_room_links.py` `MAX_LINKS = 512`) and is passed into
/// `upsert_room_link_record`. Exposed here to match it.
pub const MAX_LINKS: i64 = 512;

/// Errors from the room-link record layer. Mirrors Python's `HostedRoomError`
/// (a `ValueError` subclass) for the capacity case, and wraps SQLite failures.
#[derive(Debug)]
pub enum HostedRoomError {
    /// Raised when the capacity cap would be exceeded ("too many stored room
    /// links"), matching `hosted_rooms.upsert_room_link_record`.
    TooManyStoredRoomLinks,
    /// Any underlying SQLite error, surfaced best-effort like the Python layer.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for HostedRoomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostedRoomError::TooManyStoredRoomLinks => f.write_str("too many stored room links"),
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

/// One private room-link row. Column order matches the Python schema and the
/// SELECT/INSERT statements exactly.
///
/// `catalog_json` is opaque serialized JSON text. The Python record layer never
/// parses it; only the wrapper (via hosted_room_peer's `GatewayRoomCatalog`)
/// does. Keeping it as a `String` keeps this slice free of that dependency.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomLinkRecord {
    pub room_id: String,
    pub member_id: String,
    pub target_url: String,
    pub target_profile: String,
    pub grant: String,
    pub catalog_json: String,
    pub cancellation_scope_id: String,
    pub trace_id: String,
    pub transport_security: String,
    pub status: String,
    pub updated_at: f64,
}

/// Seconds since the Unix epoch, matching Python's `time.time()`.
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Production room store path: `$HERMES_HOME/state.db`, the same root database
/// the Python gateway uses. Callers may pass any path to isolate tests, exactly
/// as the Python functions take an explicit `db_path`.
pub fn default_db_path() -> PathBuf {
    crate::config_file::hermes_home().join("state.db")
}

/// Open the rooms DB and make sure the `hosted_room_links` table exists.
///
/// Mirrors the relevant part of Python's `_connect`: create the parent dir,
/// open the SQLite file, put it in WAL, turn foreign keys on, and ensure the
/// schema. The full Python module creates seven tables together inside one
/// `BEGIN IMMEDIATE` DDL transaction; this slice only touches `hosted_room_links`
/// and its functions never join the other tables, so it ensures just that one
/// table. WAL is best-effort here (as in the delivery_ledger port): a
/// filesystem that refuses WAL falls back to the default journal mode rather
/// than failing the open.
pub fn open_rooms_db(db_path: &Path) -> Result<Connection, HostedRoomError> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(db_path)?;
    // WAL so the Python gateway can share the same file during migration.
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    // Mirror Python's literal `PRAGMA foreign_keys=ON`.
    conn.execute_batch("PRAGMA foreign_keys=ON")?;
    initialize_link_schema(&conn)?;
    Ok(conn)
}

/// Create the `hosted_room_links` table if it is missing. Column definitions,
/// order, defaults, and the composite primary key match
/// `_initialize_schema` in hosted_rooms.py byte for byte.
fn initialize_link_schema(conn: &Connection) -> Result<(), HostedRoomError> {
    conn.execute(
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
        [],
    )?;
    Ok(())
}

/// Return every stored room-link record, ordered by (room_id, member_id).
///
/// Port of `list_room_link_records`. Python opens a plain (non-immediate)
/// transaction for the read; a single SELECT on a freshly opened connection is
/// the same thing.
pub fn list_room_link_records(db_path: &Path) -> Result<Vec<RoomLinkRecord>, HostedRoomError> {
    let conn = open_rooms_db(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT room_id, member_id, target_url, target_profile, grant,
                catalog_json, cancellation_scope_id, trace_id,
                transport_security, status, updated_at
           FROM hosted_room_links
       ORDER BY room_id, member_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(RoomLinkRecord {
            room_id: row.get(0)?,
            member_id: row.get(1)?,
            target_url: row.get(2)?,
            target_profile: row.get(3)?,
            grant: row.get(4)?,
            catalog_json: row.get(5)?,
            cancellation_scope_id: row.get(6)?,
            trace_id: row.get(7)?,
            transport_security: row.get(8)?,
            status: row.get(9)?,
            updated_at: row.get(10)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Atomically insert or replace one room-link record under `BEGIN IMMEDIATE`.
///
/// Port of `upsert_room_link_record`. The capacity cap only counts against a
/// genuinely new (room_id, member_id): if the row already exists the count
/// check is skipped, so an update at the cap still succeeds. Reaching the cap
/// on a new row raises `TooManyStoredRoomLinks` and the transaction rolls back.
pub fn upsert_room_link_record(
    db_path: &Path,
    record: &RoomLinkRecord,
    max_links: i64,
) -> Result<(), HostedRoomError> {
    let mut conn = open_rooms_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let existing: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM hosted_room_links WHERE room_id=? AND member_id=?",
            params![record.room_id, record.member_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_none() {
        let count: i64 =
            tx.query_row("SELECT COUNT(*) FROM hosted_room_links", [], |row| row.get(0))?;
        if count >= max_links {
            // Dropping `tx` without committing rolls the transaction back,
            // matching Python's exception-then-rollback path.
            return Err(HostedRoomError::TooManyStoredRoomLinks);
        }
    }

    tx.execute(
        "INSERT INTO hosted_room_links(
             room_id, member_id, target_url, target_profile, grant,
             catalog_json, cancellation_scope_id, trace_id,
             transport_security, status, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(room_id, member_id) DO UPDATE SET
             target_url=excluded.target_url,
             target_profile=excluded.target_profile,
             grant=excluded.grant,
             catalog_json=excluded.catalog_json,
             cancellation_scope_id=excluded.cancellation_scope_id,
             trace_id=excluded.trace_id,
             transport_security=excluded.transport_security,
             status=excluded.status,
             updated_at=excluded.updated_at",
        params![
            record.room_id,
            record.member_id,
            record.target_url,
            record.target_profile,
            record.grant,
            record.catalog_json,
            record.cancellation_scope_id,
            record.trace_id,
            record.transport_security,
            record.status,
            record.updated_at,
        ],
    )?;

    tx.commit()?;
    Ok(())
}

/// Persist a non-secret route health classification for one link.
///
/// Port of `update_room_link_status`. Returns true only when exactly one row
/// was updated (the link exists), false otherwise. `now` defaults to the
/// current time, matching the Python `now: float | None = None` argument.
pub fn update_room_link_status(
    db_path: &Path,
    room_id: &str,
    member_id: &str,
    status: &str,
    now: Option<f64>,
) -> Result<bool, HostedRoomError> {
    let mut conn = open_rooms_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let updated_at = now.unwrap_or_else(now_secs);
    let changed = tx.execute(
        "UPDATE hosted_room_links SET status=?, updated_at=?
           WHERE room_id=? AND member_id=?",
        params![status, updated_at, room_id, member_id],
    )?;
    tx.commit()?;
    Ok(changed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Unique temp path per test, cleaned up (DB plus -wal/-shm side files).
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let mut path = std::env::temp_dir();
            path.push(format!("hermes_hosted_rooms_{tag}_{pid}_{n}.db"));
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

    fn sample(room: &str, member: &str) -> RoomLinkRecord {
        RoomLinkRecord {
            room_id: room.to_string(),
            member_id: member.to_string(),
            target_url: "wss://peer.example/ws".to_string(),
            target_profile: "default".to_string(),
            grant: "opaque-grant-token".to_string(),
            catalog_json: r#"{"installation_id":"abc","protocol_versions":[2]}"#.to_string(),
            cancellation_scope_id: "scope-1".to_string(),
            trace_id: "trace-1".to_string(),
            transport_security: "tls".to_string(),
            status: "ready".to_string(),
            updated_at: 100.0,
        }
    }

    #[test]
    fn schema_init_creates_link_table_with_expected_columns() {
        let db = TempDb::new("schema");
        let conn = open_rooms_db(db.path()).unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(hosted_room_links)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            cols,
            vec![
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
            ]
        );
    }

    #[test]
    fn upsert_then_list_roundtrip_and_conflict_update() {
        let db = TempDb::new("roundtrip");
        let rec = sample("room-a", "member-1");
        upsert_room_link_record(db.path(), &rec, MAX_LINKS).unwrap();

        let listed = list_room_link_records(db.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], rec);

        // Upsert on the same key updates fields (ON CONFLICT), no new row.
        let mut updated = rec.clone();
        updated.target_url = "wss://peer.example/ws2".to_string();
        updated.status = "unavailable".to_string();
        updated.updated_at = 200.0;
        upsert_room_link_record(db.path(), &updated, MAX_LINKS).unwrap();

        let listed = list_room_link_records(db.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], updated);
    }

    #[test]
    fn list_is_ordered_by_room_then_member() {
        let db = TempDb::new("order");
        upsert_room_link_record(db.path(), &sample("room-b", "m2"), MAX_LINKS).unwrap();
        upsert_room_link_record(db.path(), &sample("room-a", "m2"), MAX_LINKS).unwrap();
        upsert_room_link_record(db.path(), &sample("room-a", "m1"), MAX_LINKS).unwrap();

        let listed = list_room_link_records(db.path()).unwrap();
        let keys: Vec<(String, String)> = listed
            .into_iter()
            .map(|r| (r.room_id, r.member_id))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("room-a".to_string(), "m1".to_string()),
                ("room-a".to_string(), "m2".to_string()),
                ("room-b".to_string(), "m2".to_string()),
            ]
        );
    }

    #[test]
    fn status_update_reports_hit_and_miss() {
        let db = TempDb::new("status");
        upsert_room_link_record(db.path(), &sample("room-a", "member-1"), MAX_LINKS).unwrap();

        let hit = update_room_link_status(
            db.path(),
            "room-a",
            "member-1",
            "needs_reauthorization",
            Some(555.0),
        )
        .unwrap();
        assert!(hit);

        let miss =
            update_room_link_status(db.path(), "room-a", "missing", "ready", Some(1.0)).unwrap();
        assert!(!miss);

        let listed = list_room_link_records(db.path()).unwrap();
        assert_eq!(listed[0].status, "needs_reauthorization");
        assert_eq!(listed[0].updated_at, 555.0);
    }

    #[test]
    fn max_links_cap_blocks_new_rows_but_allows_updates() {
        let db = TempDb::new("cap");
        let cap = 2;
        upsert_room_link_record(db.path(), &sample("room-a", "m1"), cap).unwrap();
        upsert_room_link_record(db.path(), &sample("room-a", "m2"), cap).unwrap();

        // A third distinct key hits the cap.
        let err = upsert_room_link_record(db.path(), &sample("room-a", "m3"), cap).unwrap_err();
        assert!(matches!(err, HostedRoomError::TooManyStoredRoomLinks));
        assert_eq!(list_room_link_records(db.path()).unwrap().len(), 2);

        // Updating an existing key at the cap still succeeds (count skipped).
        let mut existing = sample("room-a", "m1");
        existing.status = "unavailable".to_string();
        upsert_room_link_record(db.path(), &existing, cap).unwrap();
        assert_eq!(list_room_link_records(db.path()).unwrap().len(), 2);
    }
}
