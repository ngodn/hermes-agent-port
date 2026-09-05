//! Port of gateway/platforms/api_server_run_idempotency.py.
//!
// Public API is ahead of its callers (the API server layer is wired later).
#![allow(dead_code)]
//!
//! Durable, tenant-scoped idempotency reservations for `POST /v1/runs`. A unique
//! `(scope, idempotency_key)` row is inserted inside `BEGIN IMMEDIATE` so separate
//! gateway workers/processes cannot both admit the same request. Only request
//! fingerprints and public run status are stored; request bodies and credentials
//! are deliberately excluded.
//!
//! This is a faithful port of the whole `RunIdempotencyStore` class, which is a
//! self-contained SQLite store with no aiohttp / GatewayRunner dependency, so it
//! ports cleanly. The one deliberate divergence is WAL setup: Python calls
//! `hermes_state.apply_wal_with_fallback`, which carries a large corruption-gate /
//! NFS-fallback policy. Every sibling SQLite store in this crate (session_db,
//! hosted_rooms, delivery_ledger) instead does a best-effort `journal_mode=WAL`,
//! so this port matches the crate idiom rather than reimplementing that policy.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

/// Records with no explicit retention are swept 24h after their last update, but
/// only once their stored run is terminal (see `prune_stale_terminal`).
pub const RETENTION_SECONDS: f64 = 24.0 * 60.0 * 60.0;
/// A terminal run whose output the room home durably imported (acknowledged) is
/// swept 24h after acknowledgement.
pub const ACKNOWLEDGED_RETENTION_SECONDS: f64 = 24.0 * 60.0 * 60.0;

/// Outcome of a reserve or lookup, mirroring the Python string outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A brand-new reservation was written (`reserve` only).
    Created,
    /// The key already existed and the fingerprint matches: a safe replay.
    Reused,
    /// The key already existed but the fingerprint differs: reuse is refused.
    Conflict,
    /// No reservation exists for the key (`lookup` only).
    Missing,
}

impl Outcome {
    /// The exact Python string for this outcome.
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Created => "created",
            Outcome::Reused => "reused",
            Outcome::Conflict => "conflict",
            Outcome::Missing => "missing",
        }
    }
}

/// The stored reservation record returned by reserve/lookup/status_for_run.
///
/// Matches the Python dict shape: `run_id`, parsed `status` JSON, `owner_pid`,
/// `owner_started`, `updated_at`. `status_for_run` fills `run_id` with the queried
/// run id (the Python dict there omits the key, but it is unambiguous).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRecord {
    pub run_id: String,
    pub status: Value,
    pub owner_pid: i64,
    pub owner_started: i64,
    pub updated_at: f64,
}

/// Seconds since the Unix epoch, matching Python's `time.time()`.
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Compact JSON with recursive sorting for Python's canonical status bytes.
fn encode_status(status: &Value) -> String {
    let mut status = status.clone();
    status.sort_all_objects();
    serde_json::to_string(&status).unwrap_or_else(|_| "null".to_string())
}

/// Constant-time byte comparison, mirroring Python's `hmac.compare_digest` for
/// fingerprint checks. Different lengths compare unequal.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Durable, tenant-scoped reservations for `POST /v1/runs`.
pub struct RunIdempotencyStore {
    conn: Mutex<Connection>,
    /// `None` means the store is in-memory (not durable), matching Python's
    /// `_db_path is None` both for `:memory:` and for the open-failure fallback.
    db_path: Option<PathBuf>,
}

impl RunIdempotencyStore {
    /// Production store path: `$HERMES_HOME/runs_idempotency.db`.
    pub fn default_db_path() -> PathBuf {
        crate::config_file::hermes_home().join("runs_idempotency.db")
    }

    /// Open (or create) the store at `$HERMES_HOME/runs_idempotency.db`.
    pub fn open_default() -> rusqlite::Result<Self> {
        Self::open(Self::default_db_path())
    }

    /// Open a durable file-backed store. If the file cannot be opened, fall back
    /// to an in-memory store (not durable), matching Python's `__init__` which
    /// logs a warning and reopens `:memory:` on connect failure.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Connection::open(path) {
            Ok(conn) => {
                Self::setup(&conn)?;
                let store = Self {
                    conn: Mutex::new(conn),
                    db_path: Some(path.to_path_buf()),
                };
                store.tighten_permissions();
                Ok(store)
            }
            Err(_) => Self::open_in_memory(),
        }
    }

    /// Open an in-memory (non-durable) store, matching Python's `:memory:` path.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::setup(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: None,
        })
    }

    /// Whether reservations survive this process (Python's `durable` property).
    pub fn durable(&self) -> bool {
        self.db_path.is_some()
    }

    /// WAL (best-effort, per crate idiom) plus schema init.
    fn setup(conn: &Connection) -> rusqlite::Result<()> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        Self::ensure_schema(conn)
    }

    /// Create the `run_idempotency` table and the unique run-id index. The
    /// column list, types, defaults, PRIMARY KEY and index match the Python
    /// schema exactly. The `PRAGMA table_info` migration below replays Python's
    /// ALTER TABLE guards for pre-existing tables that predate the newer
    /// columns; on a fresh table every column already exists, so it is a no-op.
    fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS run_idempotency (
                scope TEXT NOT NULL,
                idempotency_key TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                run_id TEXT NOT NULL,
                status_json TEXT NOT NULL,
                owner_pid INTEGER NOT NULL DEFAULT 0,
                owner_started INTEGER NOT NULL DEFAULT 0,
                retention_until REAL NOT NULL DEFAULT 0,
                acknowledged_at REAL,
                created_at REAL NOT NULL,
                updated_at REAL NOT NULL,
                PRIMARY KEY (scope, idempotency_key)
            )",
            [],
        )?;

        // Migration guards for older tables (byte-for-byte with Python).
        let mut existing: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare("PRAGMA table_info(run_idempotency)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for r in rows {
                existing.push(r?);
            }
        }
        let has = |name: &str| existing.iter().any(|c| c == name);
        if !has("owner_pid") {
            conn.execute(
                "ALTER TABLE run_idempotency ADD COLUMN owner_pid INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has("owner_started") {
            conn.execute(
                "ALTER TABLE run_idempotency ADD COLUMN owner_started INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has("retention_until") {
            conn.execute(
                "ALTER TABLE run_idempotency ADD COLUMN retention_until REAL NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has("acknowledged_at") {
            conn.execute(
                "ALTER TABLE run_idempotency ADD COLUMN acknowledged_at REAL",
                [],
            )?;
        }
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS run_idempotency_run_id ON run_idempotency(run_id)",
            [],
        )?;
        Ok(())
    }

    /// Best-effort chmod 0600 on the db and its -wal/-shm side files, matching
    /// Python's `_tighten_permissions`. No-op for an in-memory store.
    fn tighten_permissions(&self) {
        let Some(ref db_path) = self.db_path else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let base = db_path.as_os_str().to_os_string();
            for suffix in ["", "-wal", "-shm"] {
                let mut p = base.clone();
                p.push(suffix);
                let candidate = PathBuf::from(p);
                if candidate.exists() {
                    let _ = std::fs::set_permissions(
                        &candidate,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = db_path;
        }
    }

    /// Atomically reserve a key; return `(outcome, stored_record)`.
    ///
    /// Port of `reserve`. Under `BEGIN IMMEDIATE`: prune stale terminal rows,
    /// look up the `(scope, key)` row; if present it is reused (fingerprint
    /// matches) or a conflict (it differs) and the passed retention window is
    /// merged in with `MAX`; if absent a new row is inserted and returned as
    /// `Created`.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve(
        &self,
        scope: &str,
        key: &str,
        fingerprint: &str,
        run_id: &str,
        status: &Value,
        owner_pid: i64,
        owner_started: i64,
        retention_until: f64,
    ) -> rusqlite::Result<(Outcome, StoredRecord)> {
        let now = now_secs();
        let retention_until = retention_until.max(0.0);
        let encoded = encode_status(status);

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::prune_stale_terminal(&tx, now)?;

        let row = tx
            .query_row(
                "SELECT fingerprint, run_id, status_json, owner_pid, owner_started, updated_at \
                 FROM run_idempotency WHERE scope=? AND idempotency_key=?",
                params![scope, key],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<f64>>(5)?,
                    ))
                },
            )
            .optional()?;

        if let Some((row_fp, row_run_id, row_status, row_pid, row_started, row_updated)) = row {
            if retention_until != 0.0 {
                tx.execute(
                    "UPDATE run_idempotency \
                        SET retention_until=MAX(retention_until, ?) \
                      WHERE scope=? AND idempotency_key=? AND fingerprint=?",
                    params![retention_until, scope, key, fingerprint],
                )?;
            }
            tx.commit()?;
            let outcome = if ct_eq(row_fp.as_bytes(), fingerprint.as_bytes()) {
                Outcome::Reused
            } else {
                Outcome::Conflict
            };
            let record = StoredRecord {
                run_id: row_run_id,
                status: serde_json::from_str(&row_status).unwrap_or(Value::Null),
                owner_pid: row_pid.unwrap_or(0),
                owner_started: row_started.unwrap_or(0),
                updated_at: row_updated.unwrap_or(0.0),
            };
            return Ok((outcome, record));
        }

        tx.execute(
            "INSERT INTO run_idempotency(\
                scope,idempotency_key,fingerprint,run_id,status_json,\
                owner_pid,owner_started,retention_until,created_at,updated_at\
             ) VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![
                scope,
                key,
                fingerprint,
                run_id,
                encoded,
                owner_pid,
                owner_started,
                retention_until,
                now,
                now,
            ],
        )?;
        tx.commit()?;
        Ok((
            Outcome::Created,
            StoredRecord {
                run_id: run_id.to_string(),
                status: status.clone(),
                owner_pid,
                owner_started,
                updated_at: now,
            },
        ))
    }

    /// Return `Missing`, `Reused` or `Conflict` without reserving.
    ///
    /// Port of `lookup`. Optionally bumps the retention window before pruning,
    /// then reads the row. Returns `(Missing, None)` when no row exists.
    pub fn lookup(
        &self,
        scope: &str,
        key: &str,
        fingerprint: &str,
        retention_until: f64,
    ) -> rusqlite::Result<(Outcome, Option<StoredRecord>)> {
        let now = now_secs();
        let retention_until = retention_until.max(0.0);

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if retention_until != 0.0 {
            tx.execute(
                "UPDATE run_idempotency \
                    SET retention_until=MAX(retention_until, ?) \
                  WHERE scope=? AND idempotency_key=? AND fingerprint=?",
                params![retention_until, scope, key, fingerprint],
            )?;
        }
        Self::prune_stale_terminal(&tx, now)?;
        let row = tx
            .query_row(
                "SELECT fingerprint, run_id, status_json, owner_pid, owner_started, updated_at \
                 FROM run_idempotency WHERE scope=? AND idempotency_key=?",
                params![scope, key],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<f64>>(5)?,
                    ))
                },
            )
            .optional()?;
        tx.commit()?;

        let Some((row_fp, row_run_id, row_status, row_pid, row_started, row_updated)) = row else {
            return Ok((Outcome::Missing, None));
        };
        let outcome = if ct_eq(row_fp.as_bytes(), fingerprint.as_bytes()) {
            Outcome::Reused
        } else {
            Outcome::Conflict
        };
        Ok((
            outcome,
            Some(StoredRecord {
                run_id: row_run_id,
                status: serde_json::from_str(&row_status).unwrap_or(Value::Null),
                owner_pid: row_pid.unwrap_or(0),
                owner_started: row_started.unwrap_or(0),
                updated_at: row_updated.unwrap_or(0.0),
            }),
        ))
    }

    /// Prune replay records only after their stored run is terminal.
    ///
    /// Port of `_prune_stale_terminal_locked`. Age alone can never release an
    /// in-flight reservation: a long or disconnected room turn may legitimately
    /// outlive the retention window, so only rows whose status is one of
    /// completed/failed/cancelled/interrupted AND past their retention horizon
    /// are deleted. The caller holds the connection lock and an active tx.
    fn prune_stale_terminal(conn: &Connection, now: f64) -> rusqlite::Result<()> {
        // (scope, idempotency_key, status_json, retention_until, acknowledged_at,
        //  updated_at) for each row that is a candidate for deletion.
        type PruneCandidate = (
            String,
            String,
            String,
            Option<f64>,
            Option<f64>,
            Option<f64>,
        );
        let candidates: Vec<PruneCandidate> = {
            let mut stmt = conn.prepare(
                "SELECT scope, idempotency_key, status_json, retention_until, \
                        acknowledged_at, updated_at \
                   FROM run_idempotency \
                  WHERE acknowledged_at <= ? \
                     OR (retention_until > 0 AND retention_until <= ?) \
                     OR (retention_until <= 0 AND updated_at < ?)",
            )?;
            let rows = stmt.query_map(
                params![
                    now - ACKNOWLEDGED_RETENTION_SECONDS,
                    now,
                    now - RETENTION_SECONDS,
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                        r.get::<_, Option<f64>>(4)?,
                        r.get::<_, Option<f64>>(5)?,
                    ))
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };

        for (scope, key, status_json, retention_until, acknowledged_at, updated_at) in candidates {
            let terminal = serde_json::from_str::<Value>(&status_json)
                .ok()
                .and_then(|v| {
                    v.get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| matches!(s, "completed" | "failed" | "cancelled" | "interrupted"))
                })
                .unwrap_or(false);
            let retention_until = retention_until.unwrap_or(0.0);
            let updated_at = updated_at.unwrap_or(0.0);
            let expired = (acknowledged_at
                .map(|a| a <= now - ACKNOWLEDGED_RETENTION_SECONDS)
                .unwrap_or(false))
                || (retention_until > 0.0 && now >= retention_until)
                || (retention_until <= 0.0 && updated_at < now - RETENTION_SECONDS);
            if terminal && expired {
                conn.execute(
                    "DELETE FROM run_idempotency WHERE scope=? AND idempotency_key=?",
                    params![scope, key],
                )?;
            }
        }
        Ok(())
    }

    /// Load one durable run status inside its authenticated scope.
    ///
    /// Port of `status_for_run`. Optionally bumps retention, then reads the row
    /// keyed by `(scope, run_id)`. The returned record's `run_id` is the queried
    /// one (the Python dict omits it; it is unambiguous here).
    pub fn status_for_run(
        &self,
        scope: &str,
        run_id: &str,
        retention_until: f64,
    ) -> rusqlite::Result<Option<StoredRecord>> {
        let retention_until = retention_until.max(0.0);
        let conn = self.conn.lock().unwrap();
        if retention_until != 0.0 {
            conn.execute(
                "UPDATE run_idempotency \
                    SET retention_until=MAX(retention_until, ?) \
                  WHERE scope=? AND run_id=?",
                params![retention_until, scope, run_id],
            )?;
        }
        let row = conn
            .query_row(
                "SELECT status_json, owner_pid, owner_started, updated_at \
                 FROM run_idempotency WHERE scope=? AND run_id=?",
                params![scope, run_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(
            row.map(|(status_json, pid, started, updated)| StoredRecord {
                run_id: run_id.to_string(),
                status: serde_json::from_str(&status_json).unwrap_or(Value::Null),
                owner_pid: pid.unwrap_or(0),
                owner_started: started.unwrap_or(0),
                updated_at: updated.unwrap_or(0.0),
            }),
        )
    }

    /// Allow cleanup once the room home durably imported terminal output.
    ///
    /// Port of `acknowledge_terminal`. Returns true only when exactly one row
    /// was updated.
    pub fn acknowledge_terminal(&self, scope: &str, run_id: &str) -> rusqlite::Result<bool> {
        let now = now_secs();
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE run_idempotency SET acknowledged_at=? WHERE scope=? AND run_id=?",
            params![now, scope, run_id],
        )?;
        Ok(changed == 1)
    }

    /// Persist the latest verified recovery horizon for an active grant.
    ///
    /// Port of `extend_retention`. A non-positive `until` is a no-op returning
    /// false; otherwise the stored `retention_until` is raised with `MAX` and
    /// true is returned only when exactly one row was updated.
    pub fn extend_retention(
        &self,
        scope: &str,
        run_id: &str,
        until: f64,
    ) -> rusqlite::Result<bool> {
        let checked_until = until.max(0.0);
        if checked_until == 0.0 {
            return Ok(false);
        }
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE run_idempotency \
                SET retention_until=MAX(retention_until, ?) \
              WHERE scope=? AND run_id=?",
            params![checked_until, scope, run_id],
        )?;
        Ok(changed == 1)
    }

    /// Whether a `(scope, run_id)` reservation exists. Port of `owns_run`.
    pub fn owns_run(&self, scope: &str, run_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM run_idempotency WHERE scope=? AND run_id=?",
                params![scope, run_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Overwrite the stored status JSON for a run and bump `updated_at`.
    ///
    /// Port of `update_status`. Keyed by `run_id` alone (no scope), matching
    /// Python; the unique run-id index keeps that unambiguous.
    pub fn update_status(&self, run_id: &str, status: &Value) -> rusqlite::Result<()> {
        let encoded = encode_status(status);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE run_idempotency SET status_json=?, updated_at=? WHERE run_id=?",
            params![encoded, now_secs(), run_id],
        )?;
        Ok(())
    }

    /// Close the store. The connection is dropped when `self` is dropped; this
    /// consumes the store to make that explicit, matching Python's `close`.
    pub fn close(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
            path.push(format!("hermes_run_idem_{tag}_{pid}_{n}.db"));
            let _ = std::fs::remove_file(&path);
            TempDb { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn open(&self) -> RunIdempotencyStore {
            RunIdempotencyStore::open(&self.path).unwrap()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(self.path.with_extension("db-wal"));
            let _ = std::fs::remove_file(self.path.with_extension("db-shm"));
        }
    }

    #[test]
    fn schema_columns_match_python() {
        let db = TempDb::new("schema");
        let store = db.open();
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(run_idempotency)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            cols,
            vec![
                "scope",
                "idempotency_key",
                "fingerprint",
                "run_id",
                "status_json",
                "owner_pid",
                "owner_started",
                "retention_until",
                "acknowledged_at",
                "created_at",
                "updated_at",
            ]
        );
        // The unique run-id index exists.
        let has_index: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name='run_idempotency_run_id'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(has_index, Some(1));
    }

    #[test]
    fn first_insert_created_and_stored_json_is_sorted_compact() {
        let db = TempDb::new("insert");
        let store = db.open();
        assert!(store.durable());
        let status = json!({"status": "queued", "run_id": "r1", "extra": {"z": 1, "a": 2}});
        let (outcome, rec) = store
            .reserve("scopeA", "key1", "fp-abc", "r1", &status, 42, 1000, 0.0)
            .unwrap();
        assert_eq!(outcome, Outcome::Created);
        assert_eq!(outcome.as_str(), "created");
        assert_eq!(rec.run_id, "r1");
        assert_eq!(rec.owner_pid, 42);
        assert_eq!(rec.owner_started, 1000);
        assert_eq!(rec.status, status);

        // Stored JSON is key-sorted and compact, locked against real Python.
        let stored: String = {
            let conn = store.conn.lock().unwrap();
            conn.query_row(
                "SELECT status_json FROM run_idempotency WHERE scope=? AND idempotency_key=?",
                params!["scopeA", "key1"],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            stored,
            r#"{"extra":{"a":2,"z":1},"run_id":"r1","status":"queued"}"#
        );
    }

    #[test]
    fn duplicate_key_reused_returns_stored_record() {
        let db = TempDb::new("dup");
        let store = db.open();
        let status = json!({"status": "queued", "run_id": "r1"});
        let (_, first) = store
            .reserve("scopeA", "key1", "fp-abc", "r1", &status, 42, 1000, 0.0)
            .unwrap();

        // Same key + same fingerprint, different run_id/status/owner => reused,
        // and the ORIGINAL stored record comes back (not the new arguments).
        let (outcome, rec) = store
            .reserve(
                "scopeA",
                "key1",
                "fp-abc",
                "r2",
                &json!({"status": "running"}),
                99,
                2000,
                0.0,
            )
            .unwrap();
        assert_eq!(outcome, Outcome::Reused);
        assert_eq!(rec.run_id, "r1");
        assert_eq!(rec.owner_pid, 42);
        assert_eq!(rec.owner_started, 1000);
        assert_eq!(rec.status, json!({"status": "queued", "run_id": "r1"}));
        assert_eq!(rec.updated_at, first.updated_at);
    }

    #[test]
    fn duplicate_key_different_fingerprint_is_conflict() {
        let db = TempDb::new("conflict");
        let store = db.open();
        store
            .reserve(
                "scopeA",
                "key1",
                "fp-abc",
                "r1",
                &json!({"status": "queued"}),
                0,
                0,
                0.0,
            )
            .unwrap();
        let (outcome, rec) = store
            .reserve(
                "scopeA",
                "key1",
                "fp-OTHER",
                "r3",
                &json!({"status": "x"}),
                0,
                0,
                0.0,
            )
            .unwrap();
        assert_eq!(outcome, Outcome::Conflict);
        // Still the original run, reuse refused.
        assert_eq!(rec.run_id, "r1");
    }

    #[test]
    fn distinct_keys_are_independent() {
        let db = TempDb::new("distinct");
        let store = db.open();
        let (o1, _) = store
            .reserve(
                "scopeA",
                "key1",
                "fp-a",
                "r1",
                &json!({"status": "queued"}),
                0,
                0,
                0.0,
            )
            .unwrap();
        let (o2, r2) = store
            .reserve(
                "scopeA",
                "key2",
                "fp-z",
                "r9",
                &json!({"status": "queued"}),
                0,
                0,
                0.0,
            )
            .unwrap();
        assert_eq!(o1, Outcome::Created);
        assert_eq!(o2, Outcome::Created);
        assert_eq!(r2.run_id, "r9");
    }

    #[test]
    fn lookup_missing_reused_conflict() {
        let db = TempDb::new("lookup");
        let store = db.open();
        store
            .reserve(
                "scopeA",
                "key1",
                "fp-abc",
                "r1",
                &json!({"status": "queued"}),
                0,
                0,
                0.0,
            )
            .unwrap();

        let (missing, none) = store.lookup("scopeA", "nope", "fp", 0.0).unwrap();
        assert_eq!(missing, Outcome::Missing);
        assert!(none.is_none());

        let (reused, rec) = store.lookup("scopeA", "key1", "fp-abc", 0.0).unwrap();
        assert_eq!(reused, Outcome::Reused);
        assert_eq!(rec.unwrap().run_id, "r1");

        let (conflict, _) = store.lookup("scopeA", "key1", "other", 0.0).unwrap();
        assert_eq!(conflict, Outcome::Conflict);
    }

    #[test]
    fn status_owns_ack_extend_update() {
        let db = TempDb::new("lifecycle");
        let store = db.open();
        store
            .reserve(
                "scopeA",
                "key1",
                "fp",
                "r1",
                &json!({"status": "queued"}),
                7,
                8,
                0.0,
            )
            .unwrap();

        let sr = store.status_for_run("scopeA", "r1", 0.0).unwrap().unwrap();
        assert_eq!(sr.run_id, "r1");
        assert_eq!(sr.owner_pid, 7);
        assert_eq!(sr.owner_started, 8);
        assert_eq!(sr.status, json!({"status": "queued"}));
        assert!(store
            .status_for_run("scopeA", "missing", 0.0)
            .unwrap()
            .is_none());

        assert!(store.owns_run("scopeA", "r1").unwrap());
        assert!(!store.owns_run("scopeA", "nope").unwrap());

        assert!(store.acknowledge_terminal("scopeA", "r1").unwrap());
        assert!(!store.acknowledge_terminal("scopeA", "nope").unwrap());

        // extend: 0 is a no-op false; a real horizon updates; a missing run is false.
        assert!(!store.extend_retention("scopeA", "r1", 0.0).unwrap());
        assert!(store
            .extend_retention("scopeA", "r1", now_secs() + 500.0)
            .unwrap());
        assert!(!store.extend_retention("scopeA", "nope", 1.0).unwrap());

        store
            .update_status("r1", &json!({"status": "completed"}))
            .unwrap();
        let after = store.status_for_run("scopeA", "r1", 0.0).unwrap().unwrap();
        assert_eq!(after.status, json!({"status": "completed"}));
    }

    #[test]
    fn extend_retention_raises_but_never_lowers() {
        let db = TempDb::new("extend");
        let store = db.open();
        store
            .reserve(
                "s",
                "k",
                "fp",
                "r1",
                &json!({"status": "queued"}),
                0,
                0,
                0.0,
            )
            .unwrap();
        let read = |store: &RunIdempotencyStore| -> f64 {
            let conn = store.conn.lock().unwrap();
            conn.query_row(
                "SELECT retention_until FROM run_idempotency WHERE run_id=?",
                params!["r1"],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(store.extend_retention("s", "r1", 500.0).unwrap());
        assert_eq!(read(&store), 500.0);
        // A lower horizon does not lower the stored value (MAX semantics).
        assert!(store.extend_retention("s", "r1", 100.0).unwrap());
        assert_eq!(read(&store), 500.0);
    }

    // Direct-insert helper for controlling timestamps in the prune tests.
    #[allow(clippy::too_many_arguments)]
    fn put(
        store: &RunIdempotencyStore,
        scope: &str,
        key: &str,
        run_id: &str,
        status: &Value,
        updated_at: f64,
        retention_until: f64,
        acknowledged_at: Option<f64>,
    ) {
        let enc = encode_status(status);
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run_idempotency(\
                scope,idempotency_key,fingerprint,run_id,status_json,\
                owner_pid,owner_started,retention_until,acknowledged_at,created_at,updated_at\
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![
                scope,
                key,
                "fp",
                run_id,
                enc,
                0,
                0,
                retention_until,
                acknowledged_at,
                updated_at,
                updated_at,
            ],
        )
        .unwrap();
    }

    #[test]
    fn prune_only_sweeps_terminal_and_expired() {
        let db = TempDb::new("prune");
        let store = db.open();
        let now = now_secs();
        let ret = RETENTION_SECONDS;
        let ack = ACKNOWLEDGED_RETENTION_SECONDS;

        // Pruned: terminal + old updated_at (no retention).
        put(
            &store,
            "s",
            "old_terminal",
            "r1",
            &json!({"status": "completed"}),
            now - ret - 10.0,
            0.0,
            None,
        );
        // Kept: non-terminal + old updated_at (in-flight protection).
        put(
            &store,
            "s",
            "old_running",
            "r2",
            &json!({"status": "running"}),
            now - ret - 10.0,
            0.0,
            None,
        );
        // Pruned: terminal + retention_until in the past.
        put(
            &store,
            "s",
            "term_ret",
            "r3",
            &json!({"status": "failed"}),
            now,
            now - 5.0,
            None,
        );
        // Kept: terminal + retention_until in the future.
        put(
            &store,
            "s",
            "term_ret_future",
            "r4",
            &json!({"status": "failed"}),
            now,
            now + 9999.0,
            None,
        );
        // Pruned: terminal + acknowledged long ago.
        put(
            &store,
            "s",
            "ackd",
            "r5",
            &json!({"status": "completed"}),
            now,
            0.0,
            Some(now - ack - 10.0),
        );
        // Kept: terminal but recently updated, no retention/ack.
        put(
            &store,
            "s",
            "fresh_term",
            "r6",
            &json!({"status": "completed"}),
            now - 5.0,
            0.0,
            None,
        );

        // Any reserve triggers the prune sweep.
        store
            .reserve(
                "s",
                "trigger",
                "fp2",
                "rT",
                &json!({"status": "queued"}),
                0,
                0,
                0.0,
            )
            .unwrap();

        let mut surviving: Vec<String> = {
            let conn = store.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT idempotency_key FROM run_idempotency")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        surviving.sort();
        assert_eq!(
            surviving,
            vec![
                "fresh_term".to_string(),
                "old_running".to_string(),
                "term_ret_future".to_string(),
                "trigger".to_string(),
            ]
        );
    }

    #[test]
    fn in_memory_store_is_not_durable() {
        let store = RunIdempotencyStore::open_in_memory().unwrap();
        assert!(!store.durable());
        let (o, _) = store
            .reserve(
                "s",
                "k",
                "fp",
                "r1",
                &json!({"status": "queued"}),
                0,
                0,
                0.0,
            )
            .unwrap();
        assert_eq!(o, Outcome::Created);
    }
    #[test]
    fn status_encoding_is_independent_of_nested_insertion_order() {
        let first: Value = serde_json::from_str(r#"{"z":[{"b":1,"a":2}],"a":0}"#).unwrap();
        let second: Value = serde_json::from_str(r#"{"a":0,"z":[{"a":2,"b":1}]}"#).unwrap();
        assert_eq!(encode_status(&first), encode_status(&second));
        assert_eq!(encode_status(&first), r#"{"a":0,"z":[{"a":2,"b":1}]}"#);
    }
}
