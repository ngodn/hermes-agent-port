//! Durable delivery-obligation ledger for gateway final responses.
//!
// Public API is ahead of its callers (the delivery path wires it next).
#![allow(dead_code)]
//!
//! Port of `gateway/delivery_ledger.py`. A final agent response generated but
//! not yet confirmed-delivered is the one artifact the gateway can lose without
//! a trace: a crash between finalize and platform ACK drops it silently. This
//! records a small durable row per outbound final response in the shared
//! `state.db`, with checkpoints around the send:
//!
//!   record_obligation()  state=pending     before any send attempt
//!   mark_attempting()     state=attempting  immediately before the await
//!   mark_delivered()      state=delivered   on success
//!   mark_failed()         state=failed      on a definitive rejection
//!
//! On startup [`sweep_recoverable`] claims rows whose owning process is dead and
//! hands them back for redelivery, with honest at-least-once markers for the
//! ambiguous states. Poison rows cannot spin: attempts are capped and stale rows
//! expire, both transitioning to `abandoned`. Everything is best-effort: a
//! ledger error must never block a real send.
//!
//! Owner liveness uses pid + process start time (Linux `/proc/<pid>/stat` field
//! 22), matching the Python pid+start-time stamp so a shared DB stays coherent.
//! The runtime-reconnect sweep (`sweep_failed_for_runtime`) is deferred until
//! the adapter send-error classification it depends on is ported.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tracing::debug;

pub const MAX_ATTEMPTS: i64 = 3;
pub const STALE_AFTER_SECONDS: f64 = 24.0 * 60.0 * 60.0;
const RETENTION_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
const MAX_ROWS: i64 = 500;

/// Visible prefix for redeliveries that might duplicate an already-received
/// message (crash mid-send / post-rejection retry). Honest at-least-once.
pub const RECOVERED_MARKER: &str =
    "\u{267b}\u{fe0f} Recovered reply — the gateway restarted during delivery, so this may be a duplicate:\n\n";

/// One row read during a sweep: (obligation_id, session_key, platform, chat_id,
/// thread_id, content, state, attempts, created_at, owner_pid, owner_started_at).
type SweepRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    f64,
    Option<i64>,
    Option<i64>,
);

/// A claimed obligation handed back for redelivery.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimedObligation {
    pub obligation_id: String,
    pub session_key: String,
    pub platform: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
    pub content: String,
    /// pending = send never started (redeliver plainly); attempting/failed =
    /// ambiguous or rejected (carry the recovered marker).
    pub needs_marker: bool,
    pub attempts: i64,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Stable id: same turn + content re-records idempotently; distinct
/// threads/topics never collide (session_key carries platform/chat/thread,
/// message_ref the triggering inbound id). sha256(...)[:24 hex], matching Python.
pub fn compute_obligation_id(session_key: &str, message_ref: &str, content: &str) -> String {
    let payload = format!("{session_key}|{message_ref}|{content}");
    let digest = Sha256::digest(payload.as_bytes());
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    hex[..24].to_string()
}

/// This process's owner stamp: (pid, start-time ticks) for liveness matching.
fn owner_stamp() -> (i64, Option<i64>) {
    let pid = std::process::id() as i64;
    (pid, process_start_ticks(pid))
}

/// Process start time from `/proc/<pid>/stat` field 22 (Linux). The comm field
/// (2) can contain spaces/parens, so parse after the last ')'.
fn process_start_ticks(pid: i64) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 2..)?;
    // After comm: field 3 is index 0 here; starttime is field 22 -> index 19.
    rest.split_whitespace().nth(19)?.parse::<i64>().ok()
}

fn pid_exists(pid: i64) -> bool {
    pid > 0 && std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// True when the recorded owning process still exists (pid + start time).
fn owner_alive(pid: Option<i64>, started_at: Option<i64>) -> bool {
    let Some(pid) = pid.filter(|&p| p > 0) else {
        return false;
    };
    match process_start_ticks(pid) {
        None => pid_exists(pid),
        Some(current) => match started_at {
            None => true,
            Some(s) => current == s,
        },
    }
}

/// The durable ledger over `state.db`.
pub struct DeliveryLedger {
    conn: Mutex<Connection>,
}

impl DeliveryLedger {
    /// Open (or create) the ledger at `$HERMES_HOME/state.db`.
    pub fn open_default() -> rusqlite::Result<Self> {
        let path = crate::config_file::hermes_home().join("state.db");
        Self::open(path)
    }

    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        // WAL so other processes (the Python gateway during migration) can share
        // the file; ignore failure (a read-only FS falls back to the default).
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute(
            "CREATE TABLE IF NOT EXISTS delivery_obligations (
                obligation_id TEXT PRIMARY KEY,
                session_key TEXT NOT NULL,
                platform TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                thread_id TEXT,
                content TEXT NOT NULL,
                state TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                created_at REAL NOT NULL,
                updated_at REAL NOT NULL,
                owner_pid INTEGER,
                owner_started_at INTEGER,
                last_error TEXT,
                adapter_profile TEXT
            )",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record a final response as owed to the platform (state=pending).
    #[allow(clippy::too_many_arguments)]
    pub fn record_obligation(
        &self,
        obligation_id: &str,
        session_key: &str,
        platform: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        content: &str,
        adapter_profile: Option<&str>,
    ) -> rusqlite::Result<()> {
        let now = now_secs();
        let (pid, started) = owner_stamp();
        let profile = adapter_profile
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .unwrap_or("default");
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO delivery_obligations
                 (obligation_id, session_key, platform, chat_id, thread_id,
                  content, state, attempts, created_at, updated_at,
                  owner_pid, owner_started_at, adapter_profile)
                 VALUES (?, ?, ?, ?, ?, ?, 'pending', 0, ?, ?, ?, ?, ?)",
                params![
                    obligation_id,
                    session_key,
                    platform,
                    chat_id,
                    thread_id,
                    content,
                    now,
                    now,
                    pid,
                    started,
                    profile
                ],
            )?;
        }
        self.prune();
        Ok(())
    }

    pub fn mark_attempting(&self, obligation_id: &str) -> rusqlite::Result<()> {
        self.update_state(obligation_id, "attempting", None)
    }
    pub fn mark_delivered(&self, obligation_id: &str) -> rusqlite::Result<()> {
        self.update_state(obligation_id, "delivered", None)
    }
    pub fn mark_failed(&self, obligation_id: &str, error: &str) -> rusqlite::Result<()> {
        self.update_state(obligation_id, "failed", Some(error))
    }

    fn update_state(
        &self,
        obligation_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> rusqlite::Result<()> {
        let err = error
            .filter(|e| !e.is_empty())
            .map(|e| e.chars().take(500).collect::<String>());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE delivery_obligations SET state=?, updated_at=?, last_error=?
             WHERE obligation_id=?",
            params![state, now_secs(), err, obligation_id],
        )?;
        Ok(())
    }

    /// Claim undelivered rows owned by dead processes; return them for
    /// redelivery. Re-stamps the owner to this process and increments attempts
    /// atomically, so a racing gateway cannot double-claim. Rows over the
    /// attempts cap or stale cutoff transition to 'abandoned' instead.
    ///
    /// `deliverable_platforms`, when Some, restricts claiming to platforms this
    /// boot can actually send on (so a disconnected platform does not burn its
    /// retry budget on a no-op).
    pub fn sweep_recoverable(
        &self,
        deliverable_platforms: Option<&[String]>,
    ) -> rusqlite::Result<Vec<ClaimedObligation>> {
        let now = now_secs();
        let (pid, started) = owner_stamp();
        let mut claimed = Vec::new();
        let conn = self.conn.lock().unwrap();

        let rows: Vec<SweepRow> = {
            let mut stmt = conn.prepare(
                "SELECT obligation_id, session_key, platform, chat_id, thread_id,
                        content, state, attempts, created_at, owner_pid, owner_started_at
                 FROM delivery_obligations
                 WHERE state IN ('pending', 'attempting', 'failed')",
            )?;
            let mapped = stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for (
            oid,
            session_key,
            platform,
            chat_id,
            thread_id,
            content,
            state,
            attempts,
            created_at,
            owner_pid,
            owner_started_at,
        ) in rows
        {
            if owner_alive(owner_pid, owner_started_at) {
                continue; // a live gateway still owns this row
            }
            if attempts >= MAX_ATTEMPTS || (now - created_at) > STALE_AFTER_SECONDS {
                conn.execute(
                    "UPDATE delivery_obligations SET state='abandoned', updated_at=? WHERE obligation_id=?",
                    params![now, oid],
                )?;
                continue;
            }
            if let Some(platforms) = deliverable_platforms {
                if !platforms.iter().any(|p| p == &platform) {
                    continue; // no adapter for this platform this boot
                }
            }
            // Guard the claim on the previous owner so a racing sweep loses.
            let changed = conn.execute(
                "UPDATE delivery_obligations
                 SET owner_pid=?, owner_started_at=?, attempts=attempts+1, updated_at=?
                 WHERE obligation_id=? AND (owner_pid IS ? OR owner_pid=?)",
                params![pid, started, now, oid, owner_pid, owner_pid],
            )?;
            if changed > 0 {
                claimed.push(ClaimedObligation {
                    obligation_id: oid,
                    session_key,
                    platform,
                    chat_id,
                    thread_id,
                    content,
                    needs_marker: state != "pending",
                    attempts: attempts + 1,
                });
            }
        }
        Ok(claimed)
    }

    /// Prune delivered/abandoned rows past retention, then cap total rows.
    fn prune(&self) {
        let now = now_secs();
        let cutoff = now - RETENTION_SECONDS;
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM delivery_obligations
             WHERE state IN ('delivered', 'abandoned') AND updated_at < ?",
            params![cutoff],
        );
        if let Ok(total) = conn.query_row("SELECT COUNT(*) FROM delivery_obligations", [], |r| {
            r.get::<_, i64>(0)
        }) {
            let excess = (total - MAX_ROWS).max(0);
            if excess > 0 {
                let _ = conn.execute(
                    "DELETE FROM delivery_obligations WHERE obligation_id IN (
                         SELECT obligation_id FROM delivery_obligations
                         ORDER BY CASE state
                                    WHEN 'delivered' THEN 0
                                    WHEN 'abandoned' THEN 1
                                    ELSE 2
                                  END, updated_at ASC
                         LIMIT ?)",
                    params![excess],
                );
            }
        }
    }

    /// Count rows in a given state (for tests / diagnostics).
    pub fn count_state(&self, state: &str) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM delivery_obligations WHERE state=?",
            params![state],
            |r| r.get(0),
        )
    }
}

/// Read the `gateway.delivery_ledger` config gate (default on).
pub fn ledger_enabled(user_config: &serde_json::Value) -> bool {
    match user_config
        .get("gateway")
        .and_then(|g| g.get("delivery_ledger"))
    {
        None => true,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => !matches!(
            s.trim().to_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        ),
        Some(_) => true,
    }
}

impl Drop for DeliveryLedger {
    fn drop(&mut self) {
        let _ = &self.conn; // connection closes with the Mutex
        debug!("delivery ledger closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_ledger_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        p.push("state.db");
        p
    }

    #[test]
    fn obligation_id_is_stable_and_24_hex() {
        let a = compute_obligation_id("s", "m", "hello");
        let b = compute_obligation_id("s", "m", "hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 24);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Different content -> different id.
        assert_ne!(a, compute_obligation_id("s", "m", "world"));
    }

    #[test]
    fn record_and_mark_transitions() {
        let path = temp_db("marks");
        let led = DeliveryLedger::open(path.clone()).unwrap();
        led.record_obligation("o1", "sk", "telegram", "c1", None, "hi", None)
            .unwrap();
        assert_eq!(led.count_state("pending").unwrap(), 1);
        led.mark_attempting("o1").unwrap();
        assert_eq!(led.count_state("attempting").unwrap(), 1);
        led.mark_delivered("o1").unwrap();
        assert_eq!(led.count_state("delivered").unwrap(), 1);
        assert_eq!(led.count_state("pending").unwrap(), 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn sweep_claims_dead_owner_rows_with_marker() {
        let path = temp_db("sweep");
        let led = DeliveryLedger::open(path.clone()).unwrap();
        // Insert a row owned by a definitely-dead pid in the 'attempting' state.
        {
            let conn = led.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO delivery_obligations
                 (obligation_id, session_key, platform, chat_id, thread_id, content,
                  state, attempts, created_at, updated_at, owner_pid, owner_started_at, adapter_profile)
                 VALUES ('o1','sk','telegram','c1',NULL,'hi','attempting',0,?,?,2147483000,999999,'default')",
                params![now_secs(), now_secs()],
            )
            .unwrap();
        }
        let claimed = led.sweep_recoverable(None).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].obligation_id, "o1");
        assert!(
            claimed[0].needs_marker,
            "attempting rows carry the recovered marker"
        );
        assert_eq!(claimed[0].attempts, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn live_owner_rows_are_not_claimed() {
        let path = temp_db("live");
        let led = DeliveryLedger::open(path.clone()).unwrap();
        // Owned by THIS live process -> must not be swept.
        led.record_obligation("o1", "sk", "telegram", "c1", None, "hi", None)
            .unwrap();
        led.mark_attempting("o1").unwrap();
        let claimed = led.sweep_recoverable(None).unwrap();
        assert!(claimed.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn over_cap_dead_rows_are_abandoned() {
        let path = temp_db("poison");
        let led = DeliveryLedger::open(path.clone()).unwrap();
        {
            let conn = led.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO delivery_obligations
                 (obligation_id, session_key, platform, chat_id, thread_id, content,
                  state, attempts, created_at, updated_at, owner_pid, owner_started_at, adapter_profile)
                 VALUES ('o1','sk','telegram','c1',NULL,'hi','failed',3,?,?,2147483000,999999,'default')",
                params![now_secs(), now_secs()],
            )
            .unwrap();
        }
        let claimed = led.sweep_recoverable(None).unwrap();
        assert!(claimed.is_empty());
        assert_eq!(led.count_state("abandoned").unwrap(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn deliverable_platforms_filter() {
        let path = temp_db("platforms");
        let led = DeliveryLedger::open(path.clone()).unwrap();
        {
            let conn = led.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO delivery_obligations
                 (obligation_id, session_key, platform, chat_id, thread_id, content,
                  state, attempts, created_at, updated_at, owner_pid, owner_started_at, adapter_profile)
                 VALUES ('o1','sk','discord','c1',NULL,'hi','pending',0,?,?,2147483000,999999,'default')",
                params![now_secs(), now_secs()],
            )
            .unwrap();
        }
        // Only telegram is deliverable this boot -> the discord row is left alone.
        let claimed = led
            .sweep_recoverable(Some(&["telegram".to_string()]))
            .unwrap();
        assert!(claimed.is_empty());
        // With discord deliverable it is claimed (pending -> no marker).
        let claimed = led
            .sweep_recoverable(Some(&["discord".to_string()]))
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(!claimed[0].needs_marker);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ledger_enabled_gate() {
        use serde_json::json;
        assert!(ledger_enabled(&json!({})));
        assert!(ledger_enabled(&json!({"gateway": {}})));
        assert!(!ledger_enabled(
            &json!({"gateway": {"delivery_ledger": false}})
        ));
        assert!(!ledger_enabled(
            &json!({"gateway": {"delivery_ledger": "off"}})
        ));
        assert!(ledger_enabled(
            &json!({"gateway": {"delivery_ledger": true}})
        ));
    }
}
