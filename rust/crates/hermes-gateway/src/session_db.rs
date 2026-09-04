//! Session + message store over `state.db` (phase 3 foundation).
//!
// Public API is ahead of its callers (turn history wiring lands next).
#![allow(dead_code)]
//!
//! A bounded slice of `hermes_state.py`: the conversation-history read/write
//! path every agent backend needs for multi-turn chat. It targets the real
//! hermes schema (verified against a live state.db):
//!
//!   sessions(id TEXT PK, source, session_key, chat_id, chat_type, thread_id,
//!            started_at REAL, message_count, ...)
//!   messages(id INTEGER PK AUTOINCREMENT, session_id, role, content, tool_calls,
//!            tool_name, tool_call_id, timestamp REAL, active INTEGER DEFAULT 1, ...)
//!
//! Live history is `active = 1` ordered by `id`. Against a hermes-managed DB the
//! CREATE TABLE IF NOT EXISTS calls are no-ops (the real, wider tables are used,
//! and their FTS5 triggers fire on our INSERTs); on a fresh Rust-only DB they
//! create a compatible minimal schema. Full schema + migrations + FTS5 search
//! are later slices.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::{params, Connection};

/// One message in a conversation, as needed to reconstruct history for a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Session + message store.
pub struct SessionDb {
    conn: Mutex<Connection>,
}

impl SessionDb {
    /// Open (or create) the store at `$HERMES_HOME/state.db`.
    pub fn open_default() -> rusqlite::Result<Self> {
        Self::open(crate::config_file::hermes_home().join("state.db"))
    }

    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        Self::ensure_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Minimal, hermes-compatible schema. No-op against the real (wider) tables.
    fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                session_key TEXT,
                chat_id TEXT,
                chat_type TEXT,
                thread_id TEXT,
                started_at REAL NOT NULL,
                message_count INTEGER DEFAULT 0,
                last_activity_at REAL
            )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_call_id TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                timestamp REAL NOT NULL,
                active INTEGER NOT NULL DEFAULT 1
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id, id)",
            [],
        )?;
        Ok(())
    }

    /// Ensure a session row exists (INSERT OR IGNORE). Safe to call every turn.
    pub fn ensure_session(
        &self,
        session_id: &str,
        source: &str,
        session_key: Option<&str>,
        chat_id: Option<&str>,
        chat_type: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO sessions
             (id, source, session_key, chat_id, chat_type, started_at, last_activity_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                session_id,
                source,
                session_key,
                chat_id,
                chat_type,
                now_secs(),
                now_secs()
            ],
        )?;
        Ok(())
    }

    /// Append a message to a session and bump its counters. Returns the new row id.
    pub fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let now = now_secs();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, active)
             VALUES (?, ?, ?, ?, 1)",
            params![session_id, role, content, now],
        )?;
        let id = conn.last_insert_rowid();
        // Best-effort counter bump; ignore if the session row isn't present.
        let _ = conn.execute(
            "UPDATE sessions SET message_count = message_count + 1, last_activity_at = ?
             WHERE id = ?",
            params![now, session_id],
        );
        Ok(id)
    }

    /// Load a session's live history (active = 1), oldest first. `limit` caps the
    /// most recent N (0 = all).
    pub fn load_history(
        &self,
        session_id: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<HistoryMessage>> {
        let conn = self.conn.lock().unwrap();
        // Take the most recent `limit` by id, then present oldest-first.
        let sql = if limit == 0 {
            "SELECT role, content FROM messages
             WHERE session_id = ? AND active = 1 ORDER BY id ASC"
                .to_string()
        } else {
            format!(
                "SELECT role, content FROM (
                     SELECT id, role, content FROM messages
                     WHERE session_id = ? AND active = 1
                     ORDER BY id DESC LIMIT {limit}
                 ) ORDER BY id ASC"
            )
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(HistoryMessage {
                role: r.get::<_, String>(0)?,
                content: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            })
        })?;
        rows.collect()
    }

    /// Count live messages in a session.
    pub fn message_count(&self, session_id: &str) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ? AND active = 1",
            params![session_id],
            |r| r.get(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_sessdb_{}_{}_{}",
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
    fn append_and_load_history_in_order() {
        let path = temp_db("order");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("s1", "cli", None, Some("c1"), Some("dm"))
            .unwrap();
        db.append_message("s1", "user", "hello").unwrap();
        db.append_message("s1", "assistant", "hi there").unwrap();
        db.append_message("s1", "user", "how are you").unwrap();

        let hist = db.load_history("s1", 0).unwrap();
        assert_eq!(
            hist,
            vec![
                HistoryMessage {
                    role: "user".into(),
                    content: "hello".into()
                },
                HistoryMessage {
                    role: "assistant".into(),
                    content: "hi there".into()
                },
                HistoryMessage {
                    role: "user".into(),
                    content: "how are you".into()
                },
            ]
        );
        assert_eq!(db.message_count("s1").unwrap(), 3);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn limit_returns_most_recent_oldest_first() {
        let path = temp_db("limit");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("s1", "cli", None, None, None).unwrap();
        for i in 0..5 {
            db.append_message("s1", "user", &format!("m{i}")).unwrap();
        }
        let hist = db.load_history("s1", 2).unwrap();
        let texts: Vec<&str> = hist.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, vec!["m3", "m4"]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn history_is_per_session() {
        let path = temp_db("iso");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("a", "cli", None, None, None).unwrap();
        db.ensure_session("b", "cli", None, None, None).unwrap();
        db.append_message("a", "user", "for a").unwrap();
        db.append_message("b", "user", "for b").unwrap();
        assert_eq!(db.load_history("a", 0).unwrap().len(), 1);
        assert_eq!(db.load_history("b", 0).unwrap()[0].content, "for b");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ensure_session_is_idempotent() {
        let path = temp_db("idem");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("s1", "cli", None, None, None).unwrap();
        db.ensure_session("s1", "cli", None, None, None).unwrap(); // no error, no dup
        let count: i64 = {
            let conn = db.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM sessions WHERE id='s1'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
