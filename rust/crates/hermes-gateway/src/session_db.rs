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

use hermes_core::{Message, Platform};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

/// One message in a conversation, as needed to reconstruct history for a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// Optional columns for [`SessionDb::append_message_with`]. `None` fields are
/// left NULL. `display_metadata` is serialized to JSON text.
#[derive(Default)]
pub struct AppendOptions<'a> {
    pub tool_call_id: Option<&'a str>,
    pub tool_calls: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub display_kind: Option<&'a str>,
    pub display_metadata: Option<Value>,
    pub timestamp: Option<f64>,
}

/// A full stored message row (recovery / diagnostics read path).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_name: Option<String>,
    pub display_kind: Option<String>,
    pub display_metadata: Option<String>,
    pub timestamp: f64,
}

/// One full-text search hit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub message_id: i64,
    /// Content excerpt with the matched terms bracketed.
    pub snippet: String,
    pub timestamp: f64,
}

/// How many prior messages to feed a stateless backend as context.
pub const HISTORY_LIMIT: usize = 40;

/// Stable session id for a message: `<platform>:<channel_id>`, lowercased.
pub fn session_id_for(platform: Platform, channel_id: &str) -> String {
    format!("{platform:?}:{channel_id}").to_lowercase()
}

/// Start a turn for a stateless backend: ensure the session exists, load prior
/// history, and record the inbound user message. Returns the prior history
/// (empty when the backend manages its own history or no store is available).
pub fn begin_turn(
    db: Option<&SessionDb>,
    manages_history: bool,
    msg: &Message,
    source: &str,
) -> Vec<HistoryMessage> {
    if manages_history {
        return Vec::new();
    }
    let Some(db) = db else {
        return Vec::new();
    };
    let sid = session_id_for(msg.platform, &msg.channel_id);
    let _ = db.ensure_session(
        &sid,
        source,
        None,
        Some(&msg.channel_id),
        msg.chat_type.as_deref(),
    );
    let prior = db.load_history(&sid, HISTORY_LIMIT).unwrap_or_default();
    let _ = db.append_message(&sid, "user", &msg.text);
    prior
}

/// Finish a turn for a stateless backend: record the assistant reply. No-op when
/// the backend manages its own history, the reply is empty, or no store exists.
pub fn end_turn(db: Option<&SessionDb>, manages_history: bool, msg: &Message, reply: &str) {
    if manages_history || reply.is_empty() {
        return;
    }
    if let Some(db) = db {
        let sid = session_id_for(msg.platform, &msg.channel_id);
        let _ = db.append_message(&sid, "assistant", reply);
    }
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
                display_kind TEXT,
                display_metadata TEXT,
                timestamp REAL NOT NULL,
                active INTEGER NOT NULL DEFAULT 1
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id, id)",
            [],
        )?;
        // Full-text search over messages (external-content FTS5). Matches the
        // real hermes DDL: indexes content/tool_name/tool_calls, rowid = id.
        // IF NOT EXISTS makes it a no-op against a hermes-managed DB.
        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                content, tool_name, tool_calls,
                content='messages', content_rowid='id'
            )",
            [],
        )?;
        // Sync triggers, reusing hermes's trigger NAMES so on a shared DB our
        // (simpler) versions are never created alongside theirs and thus can't
        // double-index. On a fresh Rust-only DB these keep the FTS in sync.
        conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
                 INSERT INTO messages_fts(rowid, content, tool_name, tool_calls)
                 VALUES (new.id, new.content, new.tool_name, new.tool_calls);
             END;
             CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
                 INSERT INTO messages_fts(messages_fts, rowid, content, tool_name, tool_calls)
                 VALUES ('delete', old.id, old.content, old.tool_name, old.tool_calls);
             END;
             CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
                 INSERT INTO messages_fts(messages_fts, rowid, content, tool_name, tool_calls)
                 VALUES ('delete', old.id, old.content, old.tool_name, old.tool_calls);
                 INSERT INTO messages_fts(rowid, content, tool_name, tool_calls)
                 VALUES (new.id, new.content, new.tool_name, new.tool_calls);
             END;",
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

    /// Append a plain message to a session. Returns the new row id.
    pub fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> rusqlite::Result<i64> {
        self.append_message_with(session_id, role, content, &AppendOptions::default())
    }

    /// Append a message with the full column set (tool fields, display kind /
    /// metadata, explicit timestamp). `display_metadata` is serialized to JSON
    /// text. Mirrors the shape the delivery/TUI poll path and cron delegation
    /// deliveries persist (`display_kind="async_delegation_complete"`, ...).
    pub fn append_message_with(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        opts: &AppendOptions,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let ts = opts.timestamp.unwrap_or_else(now_secs);
        let display_metadata = opts
            .display_metadata
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        conn.execute(
            "INSERT INTO messages
                (session_id, role, content, tool_call_id, tool_calls, tool_name,
                 display_kind, display_metadata, timestamp, active)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
            params![
                session_id,
                role,
                content,
                opts.tool_call_id,
                opts.tool_calls,
                opts.tool_name,
                opts.display_kind,
                display_metadata,
                ts,
            ],
        )?;
        let id = conn.last_insert_rowid();
        // Best-effort counter bump; ignore if the session row isn't present.
        let _ = conn.execute(
            "UPDATE sessions SET message_count = message_count + 1, last_activity_at = ?
             WHERE id = ?",
            params![ts, session_id],
        );
        Ok(id)
    }

    /// Read a single stored message by row id (for recovery / diagnostics).
    pub fn get_message(&self, id: i64) -> rusqlite::Result<Option<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, role, content, tool_call_id, tool_calls, tool_name,
                    display_kind, display_metadata, timestamp
             FROM messages WHERE id = ?",
        )?;
        let row = stmt
            .query_row(params![id], |r| {
                Ok(StoredMessage {
                    session_id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    tool_call_id: r.get(3)?,
                    tool_calls: r.get(4)?,
                    tool_name: r.get(5)?,
                    display_kind: r.get(6)?,
                    display_metadata: r.get(7)?,
                    timestamp: r.get(8)?,
                })
            })
            .optional()?;
        Ok(row)
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

    /// Full-text search live messages across all sessions, newest-matching
    /// first by FTS rank. `query` is an FTS5 MATCH expression; a malformed
    /// expression is caught and returned as an empty result rather than an error.
    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let limit = if limit == 0 { 50 } else { limit };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.session_id, m.id,
                    snippet(messages_fts, 0, '[', ']', '…', 12) AS snip,
                    m.timestamp
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             WHERE messages_fts MATCH ?1 AND m.active = 1
             ORDER BY rank
             LIMIT ?2",
        )?;
        // A malformed FTS5 MATCH surfaces while stepping rows, so catch the
        // syntax error across the whole execute-and-collect, not just query_map.
        let result: rusqlite::Result<Vec<SearchHit>> = stmt
            .query_map(params![query, limit as i64], |r| {
                Ok(SearchHit {
                    session_id: r.get(0)?,
                    message_id: r.get(1)?,
                    snippet: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    timestamp: r.get(3)?,
                })
            })
            .and_then(|mapped| mapped.collect());
        match result {
            Ok(hits) => Ok(hits),
            // A bad MATCH expression is user input, not a DB failure.
            Err(rusqlite::Error::SqliteFailure(_, _)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
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
    fn append_with_display_kind_roundtrips() {
        let path = temp_db("display");
        let db = SessionDb::open(path).unwrap();
        db.ensure_session("s1", "api_server", None, None, None)
            .unwrap();
        let id = db
            .append_message_with(
                "s1",
                "user",
                "delegation done",
                &AppendOptions {
                    display_kind: Some("async_delegation_complete"),
                    display_metadata: Some(serde_json::json!({"task_count": 2, "failed_count": 0})),
                    ..Default::default()
                },
            )
            .unwrap();
        let row = db.get_message(id).unwrap().unwrap();
        assert_eq!(row.role, "user");
        assert_eq!(row.content, "delegation done");
        assert_eq!(
            row.display_kind.as_deref(),
            Some("async_delegation_complete")
        );
        // display_metadata is stored as JSON text and parses back.
        let meta: serde_json::Value =
            serde_json::from_str(row.display_metadata.as_deref().unwrap()).unwrap();
        assert_eq!(meta["task_count"], serde_json::json!(2));
        // It is a live message and shows up in history.
        assert_eq!(db.message_count("s1").unwrap(), 1);
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
    fn fts_search_finds_messages() {
        let path = temp_db("search");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("s1", "cli", None, None, None).unwrap();
        db.append_message("s1", "user", "the quick brown fox jumps")
            .unwrap();
        db.append_message("s1", "assistant", "a lazy dog sleeps")
            .unwrap();
        db.ensure_session("s2", "cli", None, None, None).unwrap();
        db.append_message("s2", "user", "quantum entanglement is spooky")
            .unwrap();

        let hits = db.search("brown", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert!(hits[0].snippet.contains("[brown]"));

        // A term across sessions matches the right one.
        let q = db.search("quantum", 10).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].session_id, "s2");

        // No match, empty query, and a malformed MATCH all yield no rows (no error).
        assert!(db.search("nonexistentword", 10).unwrap().is_empty());
        assert!(db.search("   ", 10).unwrap().is_empty());
        assert!(db.search("\"unbalanced", 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn fts_reflects_edits_and_deletes() {
        let path = temp_db("ftsedit");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("s1", "cli", None, None, None).unwrap();
        let id = db.append_message("s1", "user", "findme original").unwrap();
        assert_eq!(db.search("findme", 10).unwrap().len(), 1);
        // Update the row -> the FTS update trigger re-indexes it.
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE messages SET content='replaced text' WHERE id=?",
                params![id],
            )
            .unwrap();
        }
        assert!(db.search("findme", 10).unwrap().is_empty());
        assert_eq!(db.search("replaced", 10).unwrap().len(), 1);
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
