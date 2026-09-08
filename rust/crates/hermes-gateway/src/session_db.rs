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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use hermes_core::{Message, Platform};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

/// Sentinel prefix marking a JSON-encoded structured `content` value in the
/// TEXT `content` column. Ported verbatim from hermes_state.py
/// `_CONTENT_JSON_PREFIX = "\x00json:"`. Only this explicit prefix triggers
/// decoding; an ordinary JSON-looking message remains a string.
pub const CONTENT_JSON_PREFIX: &str = "\0json:";

/// Serialize a model-facing `content` value into its stored TEXT form, ported
/// from hermes_state.py `SessionDB._encode_content`.
///
/// - A plain string is stored verbatim (no sentinel), exactly like Python
///   returns `str` inputs unchanged. A string that merely *looks* like JSON
///   (e.g. `"[1,2]"`) is therefore never re-interpreted on the way back out:
///   there is no auto-detection without the sentinel.
/// - Lists/dicts (multimodal parts: text + image_url) and any other non-string
///   value are serialized as sentinel-prefixed JSON so they survive a TEXT
///   column and decode back to the same value. Python keeps bare int/float/None
///   as native sqlite scalars, which a TEXT-only column cannot; prefixing those
///   too keeps the model-level round-trip faithful for the real `begin_turn`
///   inputs (string and array), which is all that path ever feeds. This is the
///   only place the safe-scalar behavior diverges from Python, and only on the
///   stored bytes, not on the decoded value.
///
/// Note: Rust `String` is always valid UTF-8, so there are no lone surrogates
/// to scrub the way `_encode_content` does for Python's `str`. serde_json emits
/// UTF-8 rather than Python's `ensure_ascii=True` escapes; both forms decode to
/// the identical value, so a store written by either side reads back on the
/// other.
pub fn encode_message_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        other => {
            // serde_json::to_string on an in-memory Value does not fail; the
            // fallback keeps persistence from ever panicking regardless.
            let json = serde_json::to_string(other).unwrap_or_else(|_| other.to_string());
            format!("{CONTENT_JSON_PREFIX}{json}")
        }
    }
}

/// One message in a conversation, as needed to reconstruct history for a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

impl HistoryMessage {
    /// Decode the stored `content` back into the model-facing value, ported from
    /// hermes_state.py `SessionDB._decode_content`.
    ///
    /// A value carrying the JSON sentinel is parsed back to its structured form
    /// (array of text/image_url parts, an object, or a prefixed scalar). A
    /// malformed payload falls back to the raw stored string byte-for-byte,
    /// including the sentinel, matching Python's warn-and-return-`content`
    /// behavior. Anything without the sentinel is returned as a plain string
    /// with no content-sniffing.
    pub fn model_content(&self) -> Value {
        if let Some(rest) = self.content.strip_prefix(CONTENT_JSON_PREFIX) {
            match serde_json::from_str::<Value>(rest) {
                Ok(v) => v,
                // Python logs a warning and returns the raw `content` (the full
                // string, sentinel included). We mirror that exactly.
                Err(_) => Value::String(self.content.clone()),
            }
        } else {
            Value::String(self.content.clone())
        }
    }
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

/// Preserve legacy IDs for unscoped messages. Encode the scoped pair as JSON
/// so delimiters inside an ID cannot alias a different workspace/channel pair.
pub fn message_session_id(msg: &Message) -> String {
    if let Some(id) = msg
        .resolved_session_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        return id.to_owned();
    }
    if let Some(thread) = msg.thread_id.as_deref().filter(|s| !s.is_empty()) {
        return format!(
            "{:?}:thread:{}",
            msg.platform,
            serde_json::to_string(&(
                msg.workspace_id.as_deref().filter(|s| !s.is_empty()),
                &msg.channel_id,
                thread
            ))
            .unwrap()
        );
    }
    match msg.workspace_id.as_deref().filter(|s| !s.is_empty()) {
        Some(team) => format!(
            "{:?}:workspace:{}",
            msg.platform,
            serde_json::to_string(&(team, &msg.channel_id)).unwrap()
        ),
        None => session_id_for(msg.platform, &msg.channel_id),
    }
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
    let sid = message_session_id(msg);
    let _ = db.ensure_session(
        &sid,
        source,
        None,
        Some(&msg.channel_id),
        msg.chat_type.as_deref(),
    );
    let prior = db.load_history(&sid, HISTORY_LIMIT).unwrap_or_default();
    // Persist the structured model content (plain text or an array of typed
    // text/image_url parts) through the existing append path, encoding it to the
    // TEXT column exactly as hermes_state.py does on write. This is the only DB
    // write here; there is no network work and the write critical section stays
    // inside `append_message` unchanged.
    let encoded = encode_message_content(&msg.model_content());
    let _ = db.append_message(&sid, "user", &encoded);
    prior
}

/// Finish a turn for a stateless backend: record the assistant reply. No-op when
/// the backend manages its own history, the reply is empty, or no store exists.
pub fn end_turn(db: Option<&SessionDb>, manages_history: bool, msg: &Message, reply: &str) {
    if manages_history || reply.is_empty() {
        return;
    }
    if let Some(db) = db {
        let sid = message_session_id(msg);
        let _ = db.append_message(&sid, "assistant", reply);
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Ownership is derived from the database path, never the active profile.
fn store_profile_owner(path: &std::path::Path, root: &std::path::Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    let root = std::fs::canonicalize(root).ok()?;
    let parent = path.parent()?;
    if parent == root {
        return Some("default".into());
    }
    if parent.parent()? != root.join("profiles") {
        return None;
    }
    let name = parent.file_name()?.to_str()?;
    // Python's anchored regex allows a single final newline before '$'.
    let checked = name.strip_suffix('\n').unwrap_or(name);
    let valid = (1..=64).contains(&checked.len())
        && checked.as_bytes()[0].is_ascii_alphanumeric()
        && checked
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-');
    valid.then(|| name.to_owned())
}

fn session_row_value(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    use rusqlite::types::{Type, ValueRef};
    let mut data = serde_json::Map::new();
    for (index, name) in row.as_ref().column_names().iter().enumerate() {
        let value = match row.get_ref(index)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => Value::from(n),
            ValueRef::Real(n) => serde_json::Number::from_f64(n)
                .map(Value::Number)
                .ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(index, (*name).into(), Type::Real)
                })?,
            ValueRef::Text(_) => Value::String(row.get(index)?),
            ValueRef::Blob(_) => {
                return Err(rusqlite::Error::InvalidColumnType(
                    index,
                    (*name).into(),
                    Type::Blob,
                ))
            }
        };
        data.insert((*name).into(), value);
    }
    if let Some(resolved) = data.remove("_system_prompt_resolved") {
        if data.contains_key("system_prompt") {
            data.insert("system_prompt".into(), resolved);
        }
    }
    Ok(Value::Object(data))
}

fn bump_conversation_generation(conn: &Connection, id: &str, reason: &str) -> rusqlite::Result<()> {
    if ![
        "session_reset",
        "session_switch",
        "idle",
        "daily",
        "suspended",
        "resume_pending_expired",
    ]
    .contains(&reason)
    {
        return Ok(());
    }
    let peer: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT source, session_key FROM sessions WHERE id = ?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((Some(source), Some(key))) = peer else {
        return Ok(());
    };
    let source = source.trim_matches(crate::python_value::python_whitespace);
    let key = key.trim_matches(crate::python_value::python_whitespace);
    if source.is_empty() || key.is_empty() {
        return Ok(());
    }
    conn.execute("INSERT INTO conversation_generations (source, session_key, generation) VALUES (?, ?, 1)
        ON CONFLICT(source, session_key) DO UPDATE SET generation = conversation_generations.generation + 1", params![source,key])?;
    Ok(())
}

/// Lookup identity. Missing chat fields disable the legacy peer fallback.
#[derive(Default)]
pub struct GatewayPeer<'a> {
    pub source: &'a str,
    pub session_key: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub chat_id: Option<&'a str>,
    pub chat_type: Option<&'a str>,
    pub thread_id: Option<&'a str>,
}

/// Optional presentation updates preserve existing values when absent.
pub struct GatewayPeerRecord<'a> {
    pub peer: GatewayPeer<'a>,
    pub display_name: Option<&'a str>,
    pub origin_json: Option<&'a str>,
    pub include_compression_ancestors: bool,
}

const COMPRESSION_PEER_CTE: &str = r#"
                    WITH RECURSIVE compression_lineage(id) AS (
                        SELECT ?
                        UNION
                        SELECT parent.id
                        FROM compression_lineage lineage
                        JOIN sessions child ON child.id = lineage.id
                        JOIN sessions parent ON parent.id = child.parent_session_id
                        WHERE parent.end_reason = 'compression'
                          AND json_extract(
                              COALESCE(child.model_config, '{}'),
                              '$._branched_from'
                          ) IS NULL
                          AND json_extract(
                              COALESCE(child.model_config, '{}'),
                              '$._delegate_from'
                          ) IS NULL
                          AND COALESCE(child.source, '') != 'tool'
                    )
                "#;
const REPAIR_PEER_SQL: &str = r#"INSERT INTO sessions (
                               id, source, user_id, session_key, chat_id,
                               chat_type, thread_id, display_name, origin_json,
                               profile_name, started_at
                           )
                           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                           ON CONFLICT(id) DO UPDATE SET
                               session_key = COALESCE(sessions.session_key, excluded.session_key),
                               chat_id = COALESCE(sessions.chat_id, excluded.chat_id),
                               chat_type = COALESCE(sessions.chat_type, excluded.chat_type),
                               thread_id = COALESCE(sessions.thread_id, excluded.thread_id),
                               display_name = COALESCE(sessions.display_name, excluded.display_name),
                               origin_json = COALESCE(sessions.origin_json, excluded.origin_json)"#;

// Queries ported from find_latest_gateway_session_for_peer. Both reset fences
// compare against durable activity, preventing recovery behind an explicit reset.
const EXACT_RECOVERY_SQL: &str = r#"
                SELECT s.*,
                       COALESCE(sp.prompt, s.system_prompt)
                           AS _system_prompt_resolved,
                       (COALESCE(s.message_count, 0) > 0 OR EXISTS (
                           SELECT 1 FROM messages WHERE messages.session_id = s.id LIMIT 1
                       )) AS _has_messages
                FROM sessions s
                LEFT JOIN system_prompts sp ON sp.hash = s.system_prompt_hash
                WHERE s.session_key = ?
                  AND s.source = ?
                  AND (s.ended_at IS NULL OR s.end_reason IN ('agent_close', 'ws_orphan_reap', 'superseded_by_resume', 'startup_orphan_reap'))
                  AND NOT EXISTS (
                      SELECT 1 FROM sessions b
                      WHERE b.session_key = s.session_key
                        AND b.source = s.source
                        AND b.ended_at IS NOT NULL
                        AND b.end_reason IN ('session_reset', 'session_switch', 'idle', 'daily', 'suspended', 'resume_pending_expired')
                        AND b.ended_at
                            > COALESCE(s.last_activity_at, s.started_at)
                  )
                ORDER BY _has_messages DESC,
                         COALESCE(s.last_activity_at, s.started_at) DESC
                LIMIT 1
                "#;

const PEER_RECOVERY_SQL: &str = r#"
                SELECT s.*,
                       COALESCE(sp.prompt, s.system_prompt)
                           AS _system_prompt_resolved,
                       (COALESCE(s.message_count, 0) > 0 OR EXISTS (
                           SELECT 1 FROM messages WHERE messages.session_id = s.id LIMIT 1
                       )) AS _has_messages
                FROM sessions s
                LEFT JOIN system_prompts sp ON sp.hash = s.system_prompt_hash
                WHERE s.source = ?
                  AND COALESCE(s.user_id, '') = COALESCE(?, '')
                  AND COALESCE(s.chat_id, '') = COALESCE(?, '')
                  AND COALESCE(s.chat_type, '') = COALESCE(?, '')
                  AND COALESCE(s.thread_id, '') = COALESCE(?, '')
                  AND (? IS NULL OR COALESCE(s.profile_name, ?) = ?)
                  AND (s.ended_at IS NULL OR s.end_reason IN ('agent_close', 'ws_orphan_reap', 'superseded_by_resume', 'startup_orphan_reap'))
                  AND (COALESCE(s.message_count, 0) > 0 OR EXISTS (
                      SELECT 1 FROM messages WHERE messages.session_id = s.id LIMIT 1
                  ))
                  AND NOT EXISTS (
                      SELECT 1 FROM sessions b
                      WHERE b.source = s.source
                        AND COALESCE(b.user_id, '') = COALESCE(s.user_id, '')
                        AND COALESCE(b.chat_id, '') = COALESCE(s.chat_id, '')
                        AND COALESCE(b.chat_type, '') = COALESCE(s.chat_type, '')
                        AND COALESCE(b.thread_id, '') = COALESCE(s.thread_id, '')
                        AND b.ended_at IS NOT NULL
                        AND b.end_reason IN ('session_reset', 'session_switch', 'idle', 'daily', 'suspended', 'resume_pending_expired')
                        AND b.ended_at
                            > COALESCE(s.last_activity_at, s.started_at)
                  )
                ORDER BY COALESCE(s.last_activity_at, s.started_at) DESC
                LIMIT 1
                "#;

/// Session + message store.
pub struct SessionDb {
    db_path: PathBuf,
    conn: Mutex<Connection>,
}

impl SessionDb {
    /// Owning profile selected by the session store. This is internal routing
    /// metadata, not a path accepted from a transport request.
    pub fn profile_home(&self) -> Option<&std::path::Path> {
        self.db_path.parent()
    }
}

/// Metadata supplied by gateway creation and later agent enrichment.
#[derive(Default)]
pub struct SessionCreate<'a> {
    pub peer: GatewayPeer<'a>,
    pub model: Option<&'a str>,
    pub model_config: Option<&'a Value>,
    pub system_prompt: Option<&'a str>,
    pub parent_session_id: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub profile_name: Option<&'a str>,
    pub git_repo_root: Option<&'a str>,
    pub origin_json: Option<&'a str>,
    pub display_name: Option<&'a str>,
}

const CREATE_SESSION_SQL: &str = r#"INSERT INTO sessions (
                   id, source, user_id, session_key, chat_id, chat_type, thread_id,
                   model, model_config, system_prompt, system_prompt_hash,
                   parent_session_id, cwd, profile_name, git_repo_root,
                   origin_json, display_name, started_at
                )
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?)
                   ON CONFLICT(id) DO UPDATE SET
                       model = COALESCE(sessions.model, excluded.model),
                       model_config = CASE
                           WHEN excluded.model_config IS NOT NULL
                                AND json_type(
                                    sessions.model_config, '$._reset_from'
                                ) IS NOT NULL
                                AND json_remove(
                                    sessions.model_config, '$._reset_from'
                                ) = '{}'
                           THEN json_set(
                               excluded.model_config,
                               '$._reset_from',
                               json_extract(
                                   sessions.model_config, '$._reset_from'
                               )
                           )
                           ELSE COALESCE(
                               sessions.model_config, excluded.model_config
                           )
                       END,
                       system_prompt_hash = COALESCE(
                           sessions.system_prompt_hash,
                           excluded.system_prompt_hash
                       ),
                       system_prompt = CASE
                           WHEN sessions.system_prompt_hash IS NULL
                                AND excluded.system_prompt_hash IS NOT NULL
                           THEN NULL
                           ELSE sessions.system_prompt
                       END,
                       session_key = COALESCE(sessions.session_key, excluded.session_key),
                       chat_id = COALESCE(sessions.chat_id, excluded.chat_id),
                       chat_type = COALESCE(sessions.chat_type, excluded.chat_type),
                       thread_id = COALESCE(sessions.thread_id, excluded.thread_id),
                       parent_session_id = COALESCE(sessions.parent_session_id, excluded.parent_session_id),
                       cwd = COALESCE(sessions.cwd, excluded.cwd),
                       profile_name = COALESCE(sessions.profile_name, excluded.profile_name),
                       git_repo_root = COALESCE(sessions.git_repo_root, excluded.git_repo_root),
                       origin_json = COALESCE(sessions.origin_json, excluded.origin_json),
                       display_name = COALESCE(sessions.display_name, excluded.display_name)"#;

const INHERIT_SESSION_CONTEXT_SQL: &str = r#"UPDATE sessions
                       SET cwd = COALESCE(sessions.cwd,
                                 (SELECT p.cwd FROM sessions p
                                   WHERE p.id = sessions.parent_session_id)),
                           git_repo_root = COALESCE(sessions.git_repo_root,
                                           (SELECT p.git_repo_root FROM sessions p
                                             WHERE p.id = sessions.parent_session_id)),
                           git_branch = COALESCE(sessions.git_branch,
                                        (SELECT p.git_branch FROM sessions p
                                          WHERE p.id = sessions.parent_session_id)),
                           profile_name = COALESCE(sessions.profile_name,
                                          (SELECT p.profile_name FROM sessions p
                                            WHERE p.id = sessions.parent_session_id
                                              AND (p.session_key IS NULL OR sessions.session_key IS NULL OR substr(p.session_key, 1, instr(substr(p.session_key, 7), ':') + 6)  = substr(sessions.session_key, 1, instr(substr(sessions.session_key, 7), ':') + 6))))
                     WHERE id = ? AND parent_session_id IS NOT NULL"#;

const INHERIT_COMPRESSION_PEER_SQL: &str = r#"UPDATE sessions
                       SET user_id = COALESCE(sessions.user_id,
                                     (SELECT p.user_id FROM sessions p
                                       WHERE p.id = sessions.parent_session_id)),
                           session_key = COALESCE(sessions.session_key,
                                         (SELECT p.session_key FROM sessions p
                                           WHERE p.id = sessions.parent_session_id)),
                           chat_id = COALESCE(sessions.chat_id,
                                     (SELECT p.chat_id FROM sessions p
                                       WHERE p.id = sessions.parent_session_id)),
                           chat_type = COALESCE(sessions.chat_type,
                                       (SELECT p.chat_type FROM sessions p
                                         WHERE p.id = sessions.parent_session_id)),
                           thread_id = COALESCE(sessions.thread_id,
                                       (SELECT p.thread_id FROM sessions p
                                         WHERE p.id = sessions.parent_session_id)),
                           display_name = COALESCE(sessions.display_name,
                                          (SELECT p.display_name FROM sessions p
                                            WHERE p.id = sessions.parent_session_id)),
                           origin_json = COALESCE(sessions.origin_json,
                                         (SELECT p.origin_json FROM sessions p
                                           WHERE p.id = sessions.parent_session_id))
                     WHERE id = ? AND parent_session_id IS NOT NULL
                       AND EXISTS (
                           SELECT 1 FROM sessions p
                           WHERE p.id = sessions.parent_session_id
                             AND p.end_reason = 'compression'
                       )"#;

#[derive(Default)]
struct SharedGeneration {
    handle: Weak<SessionDb>,
    identity: Option<(u64, u64)>,
}

fn file_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        let identity = (metadata.dev(), metadata.ino());
        (identity.0 != 0 && identity.1 != 0).then_some(identity)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn shared_path(path: PathBuf) -> PathBuf {
    // Resolve existing prefixes too: the database or its final directories
    // may not exist yet, but a symlinked home must still share one registry key.
    let absolute = std::path::absolute(&path).unwrap_or(path);
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            component => resolved.push(component.as_os_str()),
        }
        if let Ok(canonical) = std::fs::canonicalize(&resolved) {
            resolved = canonical;
        }
    }
    resolved
}

impl SessionDb {
    /// Follow all parent links, including delegation, in Python's bounded walk.
    /// A dangling parent remains the root; cycles stop before revisiting an ID.
    pub fn session_lineage_root_to_tip(&self, session_id: &str) -> rusqlite::Result<Vec<String>> {
        if session_id.is_empty() {
            return Ok(vec![String::new()]);
        }
        let conn = self.conn.lock().unwrap();
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut current = session_id.to_owned();
        for _ in 0..100 {
            if current.is_empty() || !seen.insert(current.clone()) {
                break;
            }
            chain.push(current.clone());
            let parent: Option<Option<String>> = conn
                .query_row(
                    "SELECT parent_session_id FROM sessions WHERE id = ?",
                    [&current],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(Some(parent)) = parent else {
                break;
            };
            current = parent;
        }
        chain.reverse();
        if chain.is_empty() {
            chain.push(session_id.to_owned());
        }
        Ok(chain)
    }

    pub fn get_conversation_root(&self, session_id: &str) -> rusqlite::Result<String> {
        Ok(self
            .session_lineage_root_to_tip(session_id)?
            .first()
            .filter(|id| !id.is_empty())
            .cloned()
            .unwrap_or_else(|| session_id.to_owned()))
    }

    /// Follow compression continuations, preferring continuing/live children
    /// over stale siblings. Message recency can be newer than the heartbeat.
    pub fn get_compression_chain(&self, id: &str) -> rusqlite::Result<Vec<String>> {
        let mut chain = if id.is_empty() {
            Vec::new()
        } else {
            vec![id.to_owned()]
        };
        let mut seen: std::collections::BTreeSet<String> = chain.iter().cloned().collect();
        let mut current = id.to_owned();
        for _ in 0..100 {
            // Match Python's per-hop read scope instead of pinning a snapshot
            // across the walk while a running agent extends its continuation.
            let child: Option<String> = self
                .conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT child.id FROM sessions parent
                 JOIN sessions child ON child.parent_session_id = parent.id
                 WHERE parent.id = ? AND parent.end_reason = 'compression'
                   AND json_extract(COALESCE(child.model_config, '{}'), '$._branched_from') IS NULL
                   AND json_extract(COALESCE(child.model_config, '{}'), '$._delegate_from') IS NULL
                   AND COALESCE(child.source, '') != 'tool'
                 ORDER BY CASE WHEN child.end_reason = 'compression' THEN 0
                               WHEN child.ended_at IS NULL THEN 1 ELSE 2 END,
                   COALESCE((SELECT MAX(_act_v.v) FROM (
                       SELECT child.last_activity_at AS v UNION ALL
                       SELECT (SELECT MAX(_act_m.timestamp) FROM messages _act_m
                               WHERE _act_m.session_id = child.id)
                   ) _act_v), child.started_at) DESC,
                   child.started_at DESC, child.id DESC LIMIT 1",
                    [&current],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(child) = child.filter(|id| !id.is_empty()) else {
                break;
            };
            if !seen.insert(child.clone()) {
                break;
            }
            current = child.clone();
            chain.push(child);
        }
        Ok(chain)
    }

    pub fn get_compression_tip(&self, id: &str) -> rusqlite::Result<String> {
        Ok(self
            .get_compression_chain(id)?
            .pop()
            .unwrap_or_else(|| id.to_owned()))
    }

    /// Create or enrich a session atomically, retaining first-writer metadata
    /// and inheriting compression routing before the new row becomes visible.
    pub fn create_session(&self, id: &str, create: &SessionCreate<'_>) -> rusqlite::Result<()> {
        self.create_session_with_context(
            id,
            create,
            || store_profile_owner(&self.db_path, &crate::config_file::hermes_root()),
            now_secs,
        )
    }

    fn create_session_with_context(
        &self,
        id: &str,
        create: &SessionCreate<'_>,
        owner: impl FnOnce() -> Option<String>,
        clock: impl FnOnce() -> f64,
    ) -> rusqlite::Result<()> {
        use sha2::{Digest, Sha256};
        let profile = create
            .profile_name
            .filter(|p| {
                !p.trim_matches(crate::python_value::python_whitespace)
                    .is_empty()
            })
            .map(str::to_owned)
            .or_else(owner);
        let config = create
            .model_config
            .filter(|v| crate::python_value::truthy(v))
            .map(Value::to_string);
        let prompt_hash = create
            .system_prompt
            .map(|prompt| format!("{:x}", Sha256::digest(prompt.as_bytes())));
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        if let Some(hash) = &prompt_hash {
            tx.execute(
                "INSERT OR IGNORE INTO system_prompts (hash,prompt) VALUES (?,?)",
                params![hash, create.system_prompt],
            )?;
        }
        let peer = &create.peer;
        tx.execute(
            CREATE_SESSION_SQL,
            params![
                id,
                peer.source,
                peer.user_id,
                peer.session_key,
                peer.chat_id,
                peer.chat_type,
                peer.thread_id,
                create.model,
                config,
                prompt_hash,
                create.parent_session_id,
                create.cwd,
                profile,
                create.git_repo_root,
                create.origin_json,
                create.display_name,
                clock()
            ],
        )?;
        if prompt_hash.is_some() {
            tx.execute("DELETE FROM system_prompts WHERE NOT EXISTS (SELECT 1 FROM sessions WHERE sessions.system_prompt_hash = system_prompts.hash)", [])?;
        }
        if create.parent_session_id.is_some_and(|p| !p.is_empty()) {
            tx.execute(INHERIT_SESSION_CONTEXT_SQL, [id])?;
            tx.execute(INHERIT_COMPRESSION_PEER_SQL, [id])?;
        }
        tx.commit()
    }

    /// Acquire one writer per resolved path. Each path has its own opening
    /// lock, so schema work never blocks acquisition of unrelated databases.
    /// Arc ownership retires replaced generations until their last user drops.
    pub fn open_shared(path: PathBuf) -> rusqlite::Result<Arc<Self>> {
        static REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<SharedGeneration>>>>> =
            OnceLock::new();
        let path = shared_path(path);
        let slot = {
            let mut registry = REGISTRY.get_or_init(Mutex::default).lock().unwrap();
            registry.entry(path.clone()).or_default().clone()
        };
        let mut generation = slot.lock().unwrap();
        let current = file_identity(&path);
        let replaced = current
            .zip(generation.identity)
            .is_some_and(|(a, b)| a != b);
        if !replaced {
            if let Some(handle) = generation.handle.upgrade() {
                return Ok(handle);
            }
        }
        // Forget the old generation before opening. A failed replacement must
        // never allow a subsequent caller to borrow the retired connection.
        generation.handle = Weak::new();
        generation.identity = None;
        let handle = Arc::new(Self::open(path.clone())?);
        generation.identity = file_identity(&path);
        generation.handle = Arc::downgrade(&handle);
        Ok(handle)
    }

    /// Persist identity atomically. Ordinary refresh repairs missing rows;
    /// lineage refresh only updates existing compression ancestors.
    pub fn record_gateway_session_peer(
        &self,
        id: &str,
        record: &GatewayPeerRecord<'_>,
    ) -> rusqlite::Result<()> {
        self.record_peer_with_root(id, record, &crate::config_file::hermes_root(), now_secs)
    }

    fn record_peer_with_root(
        &self,
        id: &str,
        record: &GatewayPeerRecord<'_>,
        root: &std::path::Path,
        clock: impl FnOnce() -> f64,
    ) -> rusqlite::Result<()> {
        self.record_peer_with_owner(
            id,
            record,
            || store_profile_owner(&self.db_path, root),
            clock,
        )
    }

    // Resolve ownership only when repairing a missing row. Existing sessions
    // retain the profile that owns their history, even after routing changes.
    fn record_peer_with_owner(
        &self,
        id: &str,
        record: &GatewayPeerRecord<'_>,
        owner: impl FnOnce() -> Option<String>,
        clock: impl FnOnce() -> f64,
    ) -> rusqlite::Result<()> {
        let peer = &record.peer;
        let Some(key) = peer.session_key.filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        if id.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let cte = if record.include_compression_ancestors {
            COMPRESSION_PEER_CTE
        } else {
            ""
        };
        let target = if record.include_compression_ancestors {
            "WHERE id IN (SELECT id FROM compression_lineage)"
        } else {
            "WHERE id = ?"
        };
        let sql = format!("{cte} UPDATE sessions SET session_key = ?, source = ?, user_id = ?, chat_id = ?,
            chat_type = ?, thread_id = ?, display_name = COALESCE(?, display_name), origin_json = COALESCE(?, origin_json) {target}");
        if record.include_compression_ancestors {
            tx.execute(
                &sql,
                params![
                    id,
                    key,
                    peer.source,
                    peer.user_id,
                    peer.chat_id,
                    peer.chat_type,
                    peer.thread_id,
                    record.display_name,
                    record.origin_json
                ],
            )?;
        } else {
            tx.execute(
                &sql,
                params![
                    key,
                    peer.source,
                    peer.user_id,
                    peer.chat_id,
                    peer.chat_type,
                    peer.thread_id,
                    record.display_name,
                    record.origin_json,
                    id
                ],
            )?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)",
                [id],
                |r| r.get(0),
            )?;
            if !exists {
                let owner = owner();
                tx.execute(
                    REPAIR_PEER_SQL,
                    params![
                        id,
                        peer.source,
                        peer.user_id,
                        key,
                        peer.chat_id,
                        peer.chat_type,
                        peer.thread_id,
                        record.display_name,
                        record.origin_json,
                        owner,
                        clock()
                    ],
                )?;
            }
        }
        tx.commit()
    }

    pub fn set_expiry_finalized(&self, id: &str, finalized: bool) -> rusqlite::Result<()> {
        if !id.is_empty() {
            self.conn.lock().unwrap().execute(
                "UPDATE sessions SET expiry_finalized = ? WHERE id = ?",
                params![i64::from(finalized), id],
            )?;
        }
        Ok(())
    }

    /// First closure wins. Intentional reset boundaries advance the durable
    /// conversation identity in the same transaction as the session update.
    pub fn end_session(&self, id: &str, reason: &str) -> rusqlite::Result<()> {
        self.end_session_with_clock(id, reason, now_secs)
    }

    fn end_session_with_clock(
        &self,
        id: &str,
        reason: &str,
        clock: impl FnOnce() -> f64,
    ) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE sessions SET ended_at = ?, end_reason = ? WHERE id = ? AND ended_at IS NULL",
            params![clock(), reason, id],
        )?;
        if changed != 0 {
            bump_conversation_generation(&tx, id, reason)?;
        }
        tx.commit()
    }

    /// Accidental closures can become reset boundaries; explicit boundaries
    /// remain intact. Match Python's false-on-write-failure promotion contract.
    pub fn promote_to_session_reset(&self, id: &str, reason: &str) -> bool {
        self.promote_session_at(id, reason, now_secs())
            .unwrap_or(false)
    }

    fn promote_session_at(&self, id: &str, reason: &str, now: f64) -> rusqlite::Result<bool> {
        if id.is_empty() {
            return Ok(false);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let changed = tx.execute("UPDATE sessions SET ended_at = ?, end_reason = ? WHERE id = ? AND
            (ended_at IS NULL OR end_reason IN ('agent_close','ws_orphan_reap','superseded_by_resume','startup_orphan_reap'))",
            params![now,reason,id])?;
        if changed != 0 {
            bump_conversation_generation(&tx, id, reason)?;
        }
        tx.commit()?;
        Ok(changed != 0)
    }

    /// Stamp legacy reset children before clearing the parent's mutable reason.
    /// The stamp and reopen either both commit or both roll back.
    pub fn reopen_session(&self, id: &str) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("UPDATE sessions AS child SET model_config = json_set(
            COALESCE(child.model_config, '{}'), '$._reset_from', child.parent_session_id)
            WHERE child.parent_session_id = ?
            AND json_extract(COALESCE(child.model_config, '{}'), '$._reset_from') IS NULL
            AND EXISTS (SELECT 1 FROM sessions p WHERE p.id = child.parent_session_id
                AND p.end_reason IN ('session_reset','session_switch','idle','daily','suspended','resume_pending_expired')
                AND child.session_key IS NOT NULL AND child.session_key != ''
                AND child.session_key = p.session_key)", [id])?;
        tx.execute(
            "UPDATE sessions SET ended_at = NULL, end_reason = NULL WHERE id = ?",
            [id],
        )?;
        tx.commit()
    }

    /// Exact-key recovery ranks real conversations ahead of empty session rows.
    /// Only a miss uses the complete legacy peer tuple and store-owner fence.
    pub fn find_latest_gateway_session_for_peer(
        &self,
        peer: &GatewayPeer<'_>,
    ) -> rusqlite::Result<Option<Value>> {
        self.find_peer_with_root(peer, &crate::config_file::hermes_root())
    }

    fn find_peer_with_root(
        &self,
        peer: &GatewayPeer<'_>,
        root: &std::path::Path,
    ) -> rusqlite::Result<Option<Value>> {
        let Some(key) = peer.session_key.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let conn = self.conn.lock().unwrap();
        let exact = conn
            .query_row(
                EXACT_RECOVERY_SQL,
                params![key, peer.source],
                session_row_value,
            )
            .optional()?;
        if exact.is_some() || peer.chat_id.is_none() || peer.chat_type.is_none() {
            return Ok(exact);
        }
        let owner = store_profile_owner(&self.db_path, root);
        conn.query_row(
            PEER_RECOVERY_SQL,
            params![
                peer.source,
                peer.user_id,
                peer.chat_id,
                peer.chat_type,
                peer.thread_id,
                owner,
                owner,
                owner
            ],
            session_row_value,
        )
        .optional()
    }

    /// Persist the assembled prompt snapshot, matching update_system_prompt.
    /// Insertion, pointer replacement and unreferenced-prompt cleanup commit
    /// together so readers never observe a hash without its prompt body.
    pub fn update_system_prompt(&self, id: &str, prompt: Option<&str>) -> rusqlite::Result<()> {
        use sha2::{Digest, Sha256};
        let hash = prompt.map(|text| format!("{:x}", Sha256::digest(text.as_bytes())));
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        if let Some(hash) = &hash {
            tx.execute(
                "INSERT OR IGNORE INTO system_prompts (hash,prompt) VALUES (?,?)",
                params![hash, prompt],
            )?;
        }
        tx.execute(
            "UPDATE sessions SET system_prompt_hash=?, system_prompt=NULL WHERE id=?",
            params![hash, id],
        )?;
        tx.execute("DELETE FROM system_prompts WHERE NOT EXISTS (SELECT 1 FROM sessions WHERE sessions.system_prompt_hash=system_prompts.hash)", [])?;
        tx.commit()
    }

    /// Persist the ordered native tool prefix used by this session. The JSON
    /// text shape is shared with Python's `update_session_tool_names`; `None`
    /// clears the pin and an empty slice deliberately stores `[]`.
    pub fn update_session_tool_names(
        &self,
        id: &str,
        tool_names: Option<&[String]>,
    ) -> rusqlite::Result<()> {
        let payload = tool_names
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        self.conn.lock().unwrap().execute(
            "UPDATE sessions SET tool_names=? WHERE id=?",
            params![payload, id],
        )?;
        Ok(())
    }

    /// Read a durable lifecycle row, resolving the deduplicated system prompt
    /// the same way as Python. Token writes here are synchronous, so there is
    /// no pending token-delta queue to flush before this read.
    pub fn get_session(&self, session_id: &str) -> rusqlite::Result<Option<Value>> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT s.*, COALESCE(sp.prompt, s.system_prompt) AS _system_prompt_resolved
             FROM sessions s LEFT JOIN system_prompts sp ON sp.hash = s.system_prompt_hash
             WHERE s.id = ?",
                [session_id],
                session_row_value,
            )
            .optional()
    }

    /// Add only the lifecycle columns needed by recovery. This runs for old
    /// Rust stores as well as fresh databases and preserves wider Python tables.
    fn ensure_recovery_schema(conn: &mut Connection) -> rusqlite::Result<()> {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("CREATE TABLE IF NOT EXISTS system_prompts (hash TEXT PRIMARY KEY, prompt TEXT NOT NULL)", [])?;
        // Never cascade or prune these counters with session history: doing so
        // would allow reuse of an old conversation's prompt-cache identity.
        tx.execute(
            "CREATE TABLE IF NOT EXISTS conversation_generations (
            source TEXT NOT NULL, session_key TEXT NOT NULL,
            generation INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (source, session_key))",
            [],
        )?;
        let columns: Vec<String> = {
            let mut query = tx.prepare("PRAGMA table_info(sessions)")?;
            let rows = query
                .query_map([], |r| r.get(1))?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows?
        };
        for (name, declaration) in [
            ("user_id", "TEXT"),
            ("profile_name", "TEXT"),
            ("origin_json", "TEXT"),
            ("display_name", "TEXT"),
            ("parent_session_id", "TEXT"),
            ("system_prompt", "TEXT"),
            ("system_prompt_hash", "TEXT"),
            ("ended_at", "REAL"),
            ("end_reason", "TEXT"),
            ("expiry_finalized", "INTEGER DEFAULT 0"),
            ("model_config", "TEXT"),
            ("model", "TEXT"),
            ("cwd", "TEXT"),
            ("git_repo_root", "TEXT"),
            ("git_branch", "TEXT"),
            ("tool_names", "TEXT"),
        ] {
            if !columns.iter().any(|column| column == name) {
                // Names and declarations are static schema constants.
                tx.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {name} {declaration}"),
                    [],
                )?;
            }
        }
        tx.commit()
    }

    /// Older installs used session_key alone as the primary key. Add the scope
    /// column if absent, then rebuild atomically so cross-profile keys coexist.
    /// Inspect under the write transaction to serialize concurrent openers.
    fn heal_routing_schema(conn: &mut Connection) -> rusqlite::Result<()> {
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let columns: Vec<(String, i64)> = {
            let mut query = transaction.prepare("PRAGMA table_info(gateway_routing)")?;
            let rows: rusqlite::Result<Vec<(String, i64)>> = query
                .query_map([], |row| Ok((row.get(1)?, row.get(5)?)))?
                .collect();
            rows?
        };
        if !columns.iter().any(|(name, _)| name == "scope") {
            transaction.execute(
                "ALTER TABLE gateway_routing ADD COLUMN scope TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        let mut primary: Vec<_> = columns.iter().filter(|(_, order)| *order > 0).collect();
        primary.sort_by_key(|(_, order)| *order);
        let names: Vec<_> = primary.iter().map(|(name, _)| name.as_str()).collect();
        if names != ["scope", "session_key"] {
            transaction.execute_batch(
                "ALTER TABLE gateway_routing RENAME TO gateway_routing_legacy_pk;
                 CREATE TABLE gateway_routing (
                    scope TEXT NOT NULL DEFAULT '', session_key TEXT NOT NULL,
                    entry_json TEXT NOT NULL, updated_at REAL NOT NULL,
                    PRIMARY KEY (scope, session_key));
                 INSERT OR REPLACE INTO gateway_routing (scope, session_key, entry_json, updated_at)
                    SELECT COALESCE(scope, ''), session_key, entry_json, updated_at
                    FROM gateway_routing_legacy_pk ORDER BY updated_at ASC;
                 DROP TABLE gateway_routing_legacy_pk;",
            )?;
        }
        transaction.commit()
    }

    /// Routing rows keep their serialized entry intact. The caller owns entry
    /// validation and uses its resolved sessions directory as the scope.
    #[allow(dead_code)] // Consumed by the routing store, separate from history.
    pub fn load_gateway_routing_entries(
        &self,
        scope: &str,
    ) -> rusqlite::Result<BTreeMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut query =
            conn.prepare("SELECT session_key, entry_json FROM gateway_routing WHERE scope = ?")?;
        let rows = query
            .query_map([scope], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        rows
    }

    #[allow(dead_code)]
    pub fn save_gateway_routing_entry(
        &self,
        scope: &str,
        key: &str,
        entry_json: &str,
    ) -> rusqlite::Result<()> {
        if key.is_empty() || entry_json.is_empty() {
            return Ok(());
        }
        self.conn.lock().unwrap().execute(
            "INSERT INTO gateway_routing (scope, session_key, entry_json, updated_at)
             VALUES (?, ?, ?, ?) ON CONFLICT(scope, session_key) DO UPDATE SET
             entry_json = excluded.entry_json, updated_at = excluded.updated_at",
            params![scope, key, entry_json, now_secs()],
        )?;
        Ok(())
    }

    /// Remove absent keys and publish replacements in one transaction. A failed
    /// insertion rolls back the delete too; other scopes are never modified.
    #[allow(dead_code)]
    pub fn replace_gateway_routing_entries(
        &self,
        scope: &str,
        entries: &BTreeMap<String, String>,
    ) -> rusqlite::Result<()> {
        let now = now_secs();
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn.transaction()?;
        transaction.execute("DELETE FROM gateway_routing WHERE scope = ?", [scope])?;
        {
            let mut insert = transaction.prepare("INSERT INTO gateway_routing (scope, session_key, entry_json, updated_at) VALUES (?, ?, ?, ?)")?;
            for (key, entry) in entries {
                if !key.is_empty() && !entry.is_empty() {
                    insert.execute(params![scope, key, entry, now])?;
                }
            }
        }
        transaction.commit()
    }

    #[allow(dead_code)]
    pub fn delete_gateway_routing_entries(
        &self,
        scope: &str,
        keys: &[String],
    ) -> rusqlite::Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn.transaction()?;
        {
            let mut delete = transaction
                .prepare("DELETE FROM gateway_routing WHERE scope = ? AND session_key = ?")?;
            for key in keys {
                delete.execute(params![scope, key])?;
            }
        }
        transaction.commit()
    }

    /// Open (or create) the store at `$HERMES_HOME/state.db`.
    pub fn open_default() -> rusqlite::Result<Self> {
        Self::open(crate::config_file::hermes_home().join("state.db"))
    }

    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut conn = Connection::open(&path)?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        Self::ensure_schema(&mut conn)?;
        Ok(Self {
            db_path: path,
            conn: Mutex::new(conn),
        })
    }

    /// Minimal, hermes-compatible schema. No-op against the real (wider) tables.
    fn ensure_schema(conn: &mut Connection) -> rusqlite::Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gateway_routing (
                scope TEXT NOT NULL DEFAULT '',
                session_key TEXT NOT NULL,
                entry_json TEXT NOT NULL,
                updated_at REAL NOT NULL,
                PRIMARY KEY (scope, session_key)
            )",
            [],
        )?;
        Self::heal_routing_schema(conn)?;
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
                last_activity_at REAL,
                tool_names TEXT
            )",
            [],
        )?;
        Self::ensure_recovery_schema(conn)?;
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

    /// Claim a row written by the early Rust history path for durable routing.
    /// Keep its ID and messages intact. The guarded statement prevents lost
    /// updates between competing routes and refuses adoption once this route
    /// has any durable lineage, including an ended reset boundary.
    pub fn claim_legacy_gateway_session(
        &self,
        id: &str,
        legacy_source: &str,
        peer: &GatewayPeer<'_>,
    ) -> rusqlite::Result<bool> {
        let Some(key) = peer.session_key.filter(|key| !key.is_empty()) else {
            return Ok(false);
        };
        let Some(chat) = peer.chat_id else {
            return Ok(false);
        };
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET source = ?1, session_key = ?2, user_id = ?3,
                 chat_type = ?4, thread_id = ?5
             WHERE id = ?6 AND source = ?7 AND chat_id = ?8
               AND session_key IS NULL AND user_id IS NULL
               AND profile_name IS NULL AND origin_json IS NULL
               AND (thread_id IS NULL OR thread_id = ?5)
               AND (chat_type IS NULL OR chat_type = ?4
                    OR (chat_type = 'private' AND ?4 = 'dm'))
               AND (ended_at IS NULL OR end_reason IN
                    ('agent_close', 'ws_orphan_reap', 'superseded_by_resume', 'startup_orphan_reap'))
               AND NOT EXISTS (SELECT 1 FROM sessions newer WHERE newer.session_key = ?2)",
            params![peer.source, key, peer.user_id, peer.chat_type, peer.thread_id,
                id, legacy_source, chat],
        )?;
        Ok(changed == 1)
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

    /// Set a session's `thread_id` (forum-topic / thread root). Best-effort.
    pub fn set_thread_id(&self, session_id: &str, thread_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET thread_id = ? WHERE id = ?",
            params![thread_id, session_id],
        )?;
        Ok(())
    }

    /// Find the session id for a platform chat by origin, or `None`.
    ///
    /// Matches on `chat_id` (narrowed by `thread_id` when given) and returns a
    /// session only when the match is UNAMBIGUOUS: zero or multiple candidates
    /// both return `None` (a wrong guess would contaminate another
    /// participant's session — the mirror deliberately refuses to guess, #2221).
    /// The most recently active row wins when `thread_id` uniquely narrows it.
    pub fn find_session_by_origin(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
    ) -> rusqlite::Result<Option<String>> {
        if chat_id.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock().unwrap();
        let ids: Vec<String> = if let Some(tid) = thread_id.filter(|t| !t.is_empty()) {
            let mut stmt = conn.prepare(
                "SELECT id FROM sessions WHERE chat_id = ?1 AND thread_id = ?2
                 ORDER BY last_activity_at DESC",
            )?;
            let rows = stmt.query_map(params![chat_id, tid], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id FROM sessions WHERE chat_id = ?1 ORDER BY last_activity_at DESC",
            )?;
            let rows = stmt.query_map(params![chat_id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        // Unambiguous match only.
        if ids.len() == 1 {
            Ok(Some(ids.into_iter().next().unwrap()))
        } else {
            Ok(None)
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
    #[test]
    fn footer_keeps_lineage_birth_across_compaction_and_database_reopen() {
        use crate::prompt_footer::Footer;
        let path = temp_db("lineage_footer");
        let db = super::SessionDb::open(path.clone()).unwrap();
        let original = "20250101_233000_original";
        let rotated = "20260907_120000_compressed";
        for id in [original, rotated] {
            db.ensure_session(id, "local", None, None, None).unwrap();
        }
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET parent_session_id=? WHERE id=?",
                rusqlite::params![original, rotated],
            )
            .unwrap();
        let mut footer: Footer = serde_json::from_value(serde_json::json!({
            "now_date":"2026-09-07","start_date":"2026-09-07","iana":"Asia/Kuala_Lumpur",
            "abbreviation":"MYT","offset":"+0800","timeless":false,"pass_session_id":true,
            "session_id":rotated,"model":"test","provider":"test","platform":"cli",
        }))
        .unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-08T02:00:00+08:00").unwrap();
        let machine = chrono::FixedOffset::east_opt(0);
        footer.resolve_start_date(Some(&db), None, &now, machine);
        assert_eq!(footer.start_date.to_string(), "2025-01-02");
        let first = footer.render();
        assert!(first.starts_with("Conversation started: Thursday, January 02, 2025"));
        assert!(first.contains(
            "Today's date (as of the last context rebuild): Tuesday, September 08, 2026"
        ));
        db.update_system_prompt(rotated, Some(&first)).unwrap();
        drop(db);
        let reopened = super::SessionDb::open(path).unwrap();
        let later = chrono::DateTime::parse_from_rfc3339("2026-09-09T02:00:00+08:00").unwrap();
        footer.resolve_start_date(Some(&reopened), None, &later, machine);
        assert_eq!(footer.start_date.to_string(), "2025-01-02");
        // Rebuilding a candidate must not mutate the persisted prompt. Its
        // replacement belongs to the explicit compression/restore lifecycle.
        assert_ne!(footer.render(), first);
        assert_eq!(
            reopened.get_session(rotated).unwrap().unwrap()["system_prompt"],
            first
        );
    }

    #[test]
    fn conversation_root_matches_python_lineage_edge_cases() {
        let path = temp_db("conversation_root");
        let db = super::SessionDb::open(path).unwrap();
        assert_eq!(db.get_conversation_root("").unwrap(), "");
        assert_eq!(db.get_conversation_root("missing").unwrap(), "missing");
        for index in 0..105 {
            db.ensure_session(&index.to_string(), "local", None, None, None)
                .unwrap();
            if index > 0 {
                db.conn
                    .lock()
                    .unwrap()
                    .execute(
                        "UPDATE sessions SET parent_session_id=? WHERE id=?",
                        rusqlite::params![(index - 1).to_string(), index.to_string()],
                    )
                    .unwrap();
            }
        }
        assert_eq!(db.get_conversation_root("3").unwrap(), "0");
        assert_eq!(
            db.session_lineage_root_to_tip("3").unwrap(),
            vec!["0", "1", "2", "3"]
        );
        assert_eq!(db.get_conversation_root("104").unwrap(), "5");
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE sessions SET parent_session_id='3' WHERE id='0'", [])
            .unwrap();
        assert_eq!(
            db.session_lineage_root_to_tip("3").unwrap(),
            vec!["0", "1", "2", "3"]
        );
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET parent_session_id='dangling' WHERE id='0'",
                [],
            )
            .unwrap();
        assert_eq!(db.get_conversation_root("3").unwrap(), "dangling");
    }

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
    fn prompt_snapshot_updates_preserve_shared_bodies_and_rollback_atomically() {
        let path = temp_db("prompt_snapshot");
        let db = SessionDb::open(path.clone()).unwrap();
        for id in ["one", "two"] {
            db.ensure_session(id, "local", None, None, None).unwrap();
            db.update_system_prompt(id, Some(" exact\n\nprompt "))
                .unwrap();
        }
        let count = || {
            db.conn
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM system_prompts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        assert_eq!(count(), 1);
        db.update_system_prompt("one", Some("replacement")).unwrap();
        assert_eq!(count(), 2);
        assert_eq!(
            db.get_session("two").unwrap().unwrap()["system_prompt"],
            " exact\n\nprompt "
        );
        db.update_system_prompt("two", None).unwrap();
        assert_eq!(count(), 1);
        assert!(db.get_session("two").unwrap().unwrap()["system_prompt"].is_null());
        // Empty is a stored snapshot, distinct from clearing the pointer.
        db.update_system_prompt("two", Some("")).unwrap();
        assert_eq!(db.get_session("two").unwrap().unwrap()["system_prompt"], "");
        db.update_system_prompt("missing", Some("unreferenced"))
            .unwrap();
        assert_eq!(count(), 2);
        db.conn.lock().unwrap().execute_batch("CREATE TRIGGER fail_prompt_update BEFORE UPDATE OF system_prompt_hash ON sessions BEGIN SELECT RAISE(ABORT, 'fixture update failure'); END;").unwrap();
        assert!(db
            .update_system_prompt("one", Some("must roll back"))
            .is_err());
        assert_eq!(count(), 2);
        assert_eq!(
            db.get_session("one").unwrap().unwrap()["system_prompt"],
            "replacement"
        );
        drop(db);
        let reopened = SessionDb::open(path.clone()).unwrap();
        assert_eq!(
            reopened.get_session("one").unwrap().unwrap()["system_prompt"],
            "replacement"
        );
        assert_eq!(
            reopened.get_session("two").unwrap().unwrap()["system_prompt"],
            ""
        );
        drop(reopened);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn session_tool_names_round_trip_in_wire_order() {
        let path = temp_db("tool_names");
        let db = SessionDb::open(path).unwrap();
        db.ensure_session("one", "local", None, None, None).unwrap();
        let names = vec!["current_time".into(), "memory_search".into()];
        db.update_session_tool_names("one", Some(&names)).unwrap();
        assert_eq!(
            db.get_session("one").unwrap().unwrap()["tool_names"],
            r#"["current_time","memory_search"]"#
        );
        db.update_session_tool_names("one", Some(&[])).unwrap();
        assert_eq!(db.get_session("one").unwrap().unwrap()["tool_names"], "[]");
        db.update_session_tool_names("one", None).unwrap();
        assert!(db.get_session("one").unwrap().unwrap()["tool_names"].is_null());
    }

    #[test]
    fn legacy_claim_preserves_history_and_respects_route_boundaries() {
        let path = temp_db("legacy_claim");
        let db = SessionDb::open(path.clone()).unwrap();
        let peer = GatewayPeer {
            source: "local",
            session_key: Some("agent:main:local:dm:C"),
            user_id: Some("U"),
            chat_id: Some("C"),
            chat_type: Some("dm"),
            ..Default::default()
        };
        db.ensure_session("cli:C", "cli", None, Some("C"), Some("dm"))
            .unwrap();
        db.append_message("cli:C", "user", "keep this transcript")
            .unwrap();
        let before = db.get_session("cli:C").unwrap().unwrap();
        assert!(db
            .claim_legacy_gateway_session("cli:C", "cli", &peer)
            .unwrap());
        assert!(!db
            .claim_legacy_gateway_session("cli:C", "cli", &peer)
            .unwrap());
        let after = db.get_session("cli:C").unwrap().unwrap();
        assert_eq!(after["session_key"], peer.session_key.unwrap());
        assert_eq!(after["started_at"], before["started_at"]);
        assert_eq!(after["last_activity_at"], before["last_activity_at"]);
        assert_eq!(
            db.load_history("cli:C", 0).unwrap()[0].content,
            "keep this transcript"
        );

        for (id, existing_key, reason) in [
            ("cli:closed", None, Some("session_reset")),
            ("cli:claimed", Some("another-route"), None),
            ("cli:behind-boundary", None, None),
        ] {
            db.ensure_session(id, "cli", existing_key, Some("C"), Some("dm"))
                .unwrap();
            if let Some(reason) = reason {
                db.end_session(id, reason).unwrap();
            }
            // The already adopted row is a destination boundary even if its
            // transcript later closes; none of these may replace it.
            assert!(!db.claim_legacy_gateway_session(id, "cli", &peer).unwrap());
        }
        db.end_session("cli:C", "session_reset").unwrap();
        assert!(!db
            .claim_legacy_gateway_session("cli:behind-boundary", "cli", &peer)
            .unwrap());
        // Check legacy closure and ownership guards without a destination row
        // masking them, as well as the exact channel/source match.
        let fresh = GatewayPeer {
            session_key: Some("fresh-route"),
            ..peer
        };
        assert!(!db
            .claim_legacy_gateway_session("cli:closed", "cli", &fresh)
            .unwrap());
        assert!(!db
            .claim_legacy_gateway_session("cli:claimed", "cli", &fresh)
            .unwrap());
        assert!(!db
            .claim_legacy_gateway_session("cli:behind-boundary", "telegram", &fresh)
            .unwrap());
        assert!(!db
            .claim_legacy_gateway_session(
                "cli:behind-boundary",
                "cli",
                &GatewayPeer {
                    chat_id: Some("other"),
                    ..fresh
                }
            )
            .unwrap());
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn competing_legacy_claims_have_one_owner_across_connections() {
        let path = temp_db("legacy_competing");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("cli:C", "cli", None, Some("C"), Some("dm"))
            .unwrap();
        let other = SessionDb::open(path.clone()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [db, other]
            .into_iter()
            .enumerate()
            .map(|(index, db)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let key = format!("route-{index}");
                    barrier.wait();
                    db.claim_legacy_gateway_session(
                        "cli:C",
                        "cli",
                        &GatewayPeer {
                            source: "local",
                            session_key: Some(&key),
                            user_id: Some("U"),
                            chat_id: Some("C"),
                            chat_type: Some("dm"),
                            ..Default::default()
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        let wins = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(wins, 1);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn compression_walk_prefers_continuations_and_message_recency() {
        let path = temp_db("compression_walk");
        let db = SessionDb::open(path.clone()).unwrap();
        db.conn.lock().unwrap().execute_batch("INSERT INTO sessions (id,source,parent_session_id,started_at,ended_at,end_reason,last_activity_at,model_config) VALUES
            ('root','slack',NULL,0,10,'compression',NULL,NULL),
            ('closed','slack','root',100,200,'ws_orphan_reap',200,NULL),
            ('live','slack','root',20,NULL,NULL,30,NULL),
            ('continuation','slack','root',1,2,'compression',1,NULL),
            ('branch','slack','continuation',100,NULL,NULL,100,'{\"_branched_from\":\"continuation\"}'),
            ('delegate','slack','continuation',100,NULL,NULL,100,'{\"_delegate_from\":\"continuation\"}'),
            ('tool','tool','continuation',100,NULL,NULL,100,NULL),
            ('heartbeat','slack','continuation',3,NULL,NULL,20,NULL),
            ('message','slack','continuation',3,NULL,NULL,5,NULL);").unwrap();
        db.conn.lock().unwrap().execute("INSERT INTO messages(session_id,role,content,timestamp) VALUES ('message','user','fresh',50)", []).unwrap();
        assert_eq!(
            db.get_compression_chain("root").unwrap(),
            ["root", "continuation", "message"]
        );
        assert_eq!(db.get_compression_tip("root").unwrap(), "message");
        assert_eq!(db.get_compression_chain("missing").unwrap(), ["missing"]);
        assert!(db.get_compression_chain("").unwrap().is_empty());
        assert_eq!(db.get_compression_tip("").unwrap(), "");
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET model_config='invalid' WHERE id='message'",
                [],
            )
            .unwrap();
        assert!(db.get_compression_tip("root").is_err());
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn compression_walk_bounds_cycles_and_long_chains() {
        let path = temp_db("compression_bound");
        let db = SessionDb::open(path.clone()).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            for n in 0..105 {
                conn.execute("INSERT INTO sessions(id,source,parent_session_id,started_at,end_reason) VALUES (?,'slack',?,0,'compression')", params![format!("s{n}"), (n>0).then(||format!("s{}", n-1))]).unwrap();
            }
        }
        let chain = db.get_compression_chain("s0").unwrap();
        assert_eq!(chain.len(), 101);
        assert_eq!(chain.last().unwrap(), "s100");
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET parent_session_id='s104' WHERE id='s102'",
                [],
            )
            .unwrap();
        assert_eq!(
            db.get_compression_chain("s102").unwrap(),
            ["s102", "s103", "s104"]
        );
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn creation_enriches_reset_stub_and_rolls_back_prompt_on_invalid_config() {
        let path = temp_db("create_enrich");
        let db = SessionDb::open(path.clone()).unwrap();
        let marker = serde_json::json!({"_reset_from":"parent"});
        let create = SessionCreate {
            peer: GatewayPeer {
                source: "telegram",
                session_key: Some("key"),
                ..Default::default()
            },
            model_config: Some(&marker),
            ..Default::default()
        };
        db.create_session("s", &create).unwrap();
        let config = serde_json::json!({"temperature":0.5});
        let enrich = SessionCreate {
            peer: GatewayPeer {
                source: "unknown",
                session_key: Some("other"),
                ..Default::default()
            },
            model: Some("model"),
            model_config: Some(&config),
            system_prompt: Some("prompt"),
            ..Default::default()
        };
        db.create_session("s", &enrich).unwrap();
        let row = db.get_session("s").unwrap().unwrap();
        assert_eq!(row["source"], "telegram");
        assert_eq!(row["session_key"], "key");
        assert_eq!(row["model"], "model");
        assert_eq!(row["system_prompt"], "prompt");
        assert_eq!(
            serde_json::from_str::<Value>(row["model_config"].as_str().unwrap()).unwrap(),
            serde_json::json!({"temperature":0.5,"_reset_from":"parent"})
        );
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE sessions SET model_config='broken' WHERE id='s'", [])
            .unwrap();
        let failing = SessionCreate {
            system_prompt: Some("orphan"),
            ..enrich
        };
        assert!(db.create_session("s", &failing).is_err());
        let count: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM system_prompts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "failed enrichment rolls back its newly stored prompt"
        );
        assert_eq!(
            db.get_session("s").unwrap().unwrap()["system_prompt"],
            "prompt"
        );
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn creation_inherits_compression_peer_but_not_live_parent_routing() {
        let path = temp_db("create_inherit");
        let db = SessionDb::open(path.clone()).unwrap();
        let parent = SessionCreate {
            peer: GatewayPeer {
                source: "slack",
                session_key: Some("agent:work:slack:dm:C"),
                user_id: Some("U"),
                chat_id: Some("C"),
                ..Default::default()
            },
            cwd: Some("/work"),
            profile_name: Some("work"),
            origin_json: Some("{}"),
            display_name: Some("chat"),
            ..Default::default()
        };
        db.create_session("p", &parent).unwrap();
        let child = SessionCreate {
            peer: GatewayPeer {
                source: "slack",
                ..Default::default()
            },
            parent_session_id: Some("p"),
            ..Default::default()
        };
        db.create_session("delegate", &child).unwrap();
        assert!(db.get_session("delegate").unwrap().unwrap()["session_key"].is_null());
        db.end_session("p", "compression").unwrap();
        db.create_session("compressed", &child).unwrap();
        let row = db.get_session("compressed").unwrap().unwrap();
        assert_eq!(row["session_key"], "agent:work:slack:dm:C");
        assert_eq!(row["user_id"], "U");
        assert_eq!(row["cwd"], "/work");
        assert_eq!(row["profile_name"], "work");
        let other = SessionCreate {
            peer: GatewayPeer {
                source: "slack",
                session_key: Some("agent:other:slack:dm:C"),
                ..Default::default()
            },
            ..child
        };
        db.create_session("other", &other).unwrap();
        assert!(db.get_session("other").unwrap().unwrap()["profile_name"].is_null());
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn resolved_identity_keeps_history_across_transport_changes() {
        let path = temp_db("resolved_identity");
        let db = SessionDb::open(path.clone()).unwrap();
        let mut msg: Message = serde_json::from_value(serde_json::json!({
            "platform":"cli", "channel_id":"first", "sender_id":"user", "text":"hello",
            "resolved_session_id":"untrusted"
        }))
        .unwrap();
        assert!(msg.resolved_session_id.is_none());
        assert_eq!(message_session_id(&msg), "cli:first");
        msg.resolved_session_id = Some("durable-session".into());
        assert!(serde_json::to_value(&msg)
            .unwrap()
            .get("resolved_session_id")
            .is_none());
        assert!(begin_turn(Some(&db), false, &msg, "cli").is_empty());
        end_turn(Some(&db), false, &msg, "reply");
        msg.channel_id = "second".into();
        msg.thread_id = Some("thread".into());
        msg.workspace_id = Some("workspace".into());
        msg.text = "follow-up".into();
        let history = begin_turn(Some(&db), false, &msg, "cli");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].content, "reply");
        assert!(db.get_session("cli:first").unwrap().is_none());
        assert_eq!(message_session_id(&msg), "durable-session");
        end_turn(Some(&db), true, &msg, "bridge owns this");
        assert_eq!(db.load_history("durable-session", 100).unwrap().len(), 3);
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn shared_acquisition_coalesces_threads_and_releases_last_owner() {
        let path = temp_db("shared_threads");
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    SessionDb::open_shared(path).unwrap()
                })
            })
            .collect();
        let handles: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(handles.iter().all(|db| Arc::ptr_eq(db, &handles[0])));
        let alias = path.parent().unwrap().join(".").join("state.db");
        let alias = SessionDb::open_shared(alias).unwrap();
        assert!(Arc::ptr_eq(&alias, &handles[0]));
        let weak = Arc::downgrade(&alias);
        drop(alias);
        drop(handles);
        assert!(weak.upgrade().is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn shared_registry_retires_replaced_files_and_retries_failed_replacement() {
        let path = temp_db("shared_replaced");
        let root = path.parent().unwrap();
        let first = SessionDb::open_shared(path.clone()).unwrap();
        first
            .conn
            .lock()
            .unwrap()
            .pragma_update(None, "journal_mode", "DELETE")
            .unwrap();
        first
            .ensure_session("original", "telegram", None, None, None)
            .unwrap();
        let alias = root.join("alias.db");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(Arc::ptr_eq(&first, &SessionDb::open_shared(alias).unwrap()));
        std::fs::rename(&path, root.join("retired.db")).unwrap();
        std::fs::write(&path, "not a sqlite database").unwrap();
        assert!(SessionDb::open_shared(path.clone()).is_err());
        assert!(SessionDb::open_shared(path.clone()).is_err());
        // Existing owners retain their old generation while new owners retry.
        assert!(first.get_session("original").unwrap().is_some());
        std::fs::remove_file(&path).unwrap();
        let second = SessionDb::open_shared(path.clone()).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(second.get_session("original").unwrap().is_none());
        assert!(Arc::ptr_eq(
            &second,
            &SessionDb::open_shared(path.clone()).unwrap()
        ));
        drop(first);
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn peer_record_repairs_identity_and_preserves_optional_metadata() {
        let base = temp_db("peer_record");
        let root = base.parent().unwrap();
        let path = root.join("profiles/work/state.db");
        let db = SessionDb::open(path.clone()).unwrap();
        let mut record = GatewayPeerRecord {
            peer: GatewayPeer {
                source: "slack",
                session_key: Some("key"),
                user_id: Some("U"),
                chat_id: Some("C"),
                chat_type: Some("group"),
                thread_id: None,
            },
            display_name: Some("name"),
            origin_json: Some("{\"scope_id\":\"T\"}"),
            include_compression_ancestors: false,
        };
        db.record_peer_with_root("s", &record, root, || 100.0)
            .unwrap();
        let row = db.get_session("s").unwrap().unwrap();
        assert_eq!(row["profile_name"], "work");
        assert_eq!(row["started_at"], 100.0);
        record.display_name = None;
        record.origin_json = None;
        record.peer.user_id = None;
        db.record_peer_with_root("s", &record, root, || {
            panic!("existing row must not read clock")
        })
        .unwrap();
        let row = db.get_session("s").unwrap().unwrap();
        assert_eq!(row["display_name"], "name");
        assert_eq!(row["origin_json"], "{\"scope_id\":\"T\"}");
        assert!(row["user_id"].is_null());
        record.display_name = Some("");
        db.record_gateway_session_peer("s", &record).unwrap();
        db.set_expiry_finalized("s", true).unwrap();
        drop(db);
        let db = SessionDb::open(path).unwrap();
        let row = db.get_session("s").unwrap().unwrap();
        assert_eq!(row["display_name"], "");
        assert_eq!(row["expiry_finalized"], 1);
    }

    #[test]
    fn peer_lineage_stops_at_branch_and_failed_json_rolls_back() {
        let db = SessionDb::open(temp_db("peer_lineage")).unwrap();
        db.conn.lock().unwrap().execute_batch("INSERT INTO sessions (id,source,session_key,parent_session_id,started_at,end_reason,model_config)
            VALUES ('root','slack','old',NULL,1,'compression',NULL),
                   ('branch','slack','old','root',2,'compression','{\"_branched_from\":\"root\"}'),
                   ('tip','slack','old','branch',3,NULL,NULL);").unwrap();
        let record = GatewayPeerRecord {
            peer: GatewayPeer {
                source: "slack",
                session_key: Some("new"),
                ..Default::default()
            },
            display_name: None,
            origin_json: None,
            include_compression_ancestors: true,
        };
        db.record_gateway_session_peer("tip", &record).unwrap();
        assert_eq!(
            db.get_session("tip").unwrap().unwrap()["session_key"],
            "new"
        );
        assert_eq!(
            db.get_session("branch").unwrap().unwrap()["session_key"],
            "new"
        );
        assert_eq!(
            db.get_session("root").unwrap().unwrap()["session_key"],
            "old"
        );
        db.record_gateway_session_peer("missing", &record).unwrap();
        assert!(db.get_session("missing").unwrap().is_none());
        db.conn.lock().unwrap().execute("UPDATE sessions SET model_config = 'invalid', session_key = 'unchanged' WHERE id = 'tip'", []).unwrap();
        assert!(db.record_gateway_session_peer("tip", &record).is_err());
        assert_eq!(
            db.get_session("tip").unwrap().unwrap()["session_key"],
            "unchanged"
        );
    }

    #[test]
    fn session_creation_matches_python_sqlite() {
        use rusqlite::types::Value as SqlValue;
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/session-create-goldens.json"))
                .unwrap();
        for case in cases {
            let base = temp_db("create_python");
            let root = base.parent().unwrap();
            let path = match case["owner_profile"].as_str() {
                Some("default") => root.join("state.db"),
                Some(profile) => root.join("profiles").join(profile).join("state.db"),
                None => root.join("external/state.db"),
            };
            let db = SessionDb::open(path.clone()).unwrap();
            {
                let conn = db.conn.lock().unwrap();
                for (table, field) in [
                    ("sessions", "initial_sessions"),
                    ("system_prompts", "initial_prompts"),
                ] {
                    for row in case[field].as_array().unwrap() {
                        let row = row.as_object().unwrap();
                        let names: Vec<_> = row
                            .keys()
                            .map(|k| format!("\"{}\"", k.replace('"', "\"\"")))
                            .collect();
                        let placeholders = vec!["?"; names.len()].join(",");
                        let values: Vec<SqlValue> = row
                            .values()
                            .map(|v| match v {
                                Value::Null => SqlValue::Null,
                                Value::String(s) => SqlValue::Text(s.clone()),
                                Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
                                Value::Number(n) if n.is_i64() => {
                                    SqlValue::Integer(n.as_i64().unwrap())
                                }
                                Value::Number(n) => SqlValue::Real(n.as_f64().unwrap()),
                                v => SqlValue::Text(v.to_string()),
                            })
                            .collect();
                        conn.execute(
                            &format!(
                                "INSERT INTO {table} ({}) VALUES ({placeholders})",
                                names.join(",")
                            ),
                            rusqlite::params_from_iter(values),
                        )
                        .unwrap();
                    }
                }
            }
            for step in case["steps"].as_array().unwrap() {
                let args = &step["args"];
                // Python accepts None as an empty no-op identity.
                let id = args["session_id"].as_str().unwrap_or("");
                let create = SessionCreate {
                    peer: GatewayPeer {
                        source: args["source"].as_str().unwrap(),
                        session_key: args["session_key"].as_str(),
                        user_id: args["user_id"].as_str(),
                        chat_id: args["chat_id"].as_str(),
                        chat_type: args["chat_type"].as_str(),
                        thread_id: args["thread_id"].as_str(),
                    },
                    model: args["model"].as_str(),
                    model_config: args.get("model_config"),
                    system_prompt: args["system_prompt"].as_str(),
                    parent_session_id: args["parent_session_id"].as_str(),
                    cwd: args["cwd"].as_str(),
                    profile_name: args["profile_name"].as_str(),
                    git_repo_root: args["git_repo_root"].as_str(),
                    origin_json: args["origin_json"].as_str(),
                    display_name: args["display_name"].as_str(),
                };
                let result = db
                    .create_session_with_context(
                        id,
                        &create,
                        || case["owner_profile"].as_str().map(str::to_owned),
                        || 100.0,
                    )
                    .map(|()| {
                        if step["op"] == "create_session" {
                            Value::String(id.to_owned())
                        } else {
                            Value::Null
                        }
                    });
                assert_eq!(
                    result.is_err(),
                    !step["error"].is_null(),
                    "{}: {step}",
                    case["name"]
                );
                if let Ok(result) = result {
                    assert_eq!(result, step["result"], "{}", case["name"]);
                }
                let conn = db.conn.lock().unwrap();
                let mut query = conn.prepare("SELECT id,source,user_id,session_key,chat_id,chat_type,thread_id,display_name,origin_json,model,model_config,system_prompt,system_prompt_hash,parent_session_id,started_at,ended_at,end_reason,cwd,git_branch,git_repo_root,profile_name,expiry_finalized FROM sessions ORDER BY id").unwrap();
                let mut rows: Vec<Value> = query
                    .query_map([], session_row_value)
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                for row in &mut rows {
                    if let Some(raw) = row["model_config"].as_str() {
                        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                            row["model_config"] = parsed;
                        }
                    }
                }
                assert_eq!(
                    serde_json::json!(rows),
                    step["sessions"],
                    "{}: {step}",
                    case["name"]
                );
                let mut query = conn
                    .prepare("SELECT hash,prompt FROM system_prompts ORDER BY hash")
                    .unwrap();
                let prompts: Vec<Value> = query
                    .query_map([], session_row_value)
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                assert_eq!(
                    serde_json::json!(prompts),
                    step["system_prompts"],
                    "{}",
                    case["name"]
                );
            }
            drop(db);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn peer_record_operations_match_python_sqlite() {
        use rusqlite::types::Value as SqlValue;
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-peer-record-goldens.json"
        ))
        .unwrap();
        for case in cases {
            let base = temp_db("peer_record_python");
            let root = base.parent().unwrap();
            let path = match case["owner_profile"].as_str() {
                Some("default") => root.join("state.db"),
                Some(profile) => root.join("profiles").join(profile).join("state.db"),
                None => root.join("external/state.db"),
            };
            let db = SessionDb::open(path.clone()).unwrap();
            {
                let conn = db.conn.lock().unwrap();
                for (table, field) in [("sessions", "initial_sessions")] {
                    for row in case[field].as_array().unwrap() {
                        let row = row.as_object().unwrap();
                        let names: Vec<_> = row
                            .keys()
                            .map(|k| format!("\"{}\"", k.replace('"', "\"\"")))
                            .collect();
                        let placeholders = vec!["?"; names.len()].join(",");
                        let values: Vec<SqlValue> = row
                            .values()
                            .map(|v| match v {
                                Value::Null => SqlValue::Null,
                                Value::String(s) => SqlValue::Text(s.clone()),
                                Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
                                Value::Number(n) if n.is_i64() => {
                                    SqlValue::Integer(n.as_i64().unwrap())
                                }
                                Value::Number(n) => SqlValue::Real(n.as_f64().unwrap()),
                                v => SqlValue::Text(v.to_string()),
                            })
                            .collect();
                        conn.execute(
                            &format!(
                                "INSERT INTO {table} ({}) VALUES ({placeholders})",
                                names.join(",")
                            ),
                            rusqlite::params_from_iter(values),
                        )
                        .unwrap();
                    }
                }
            }
            for step in case["steps"].as_array().unwrap() {
                let args = &step["args"];
                // Python accepts None as an empty no-op identity.
                let id = args["session_id"].as_str().unwrap_or("");
                let result: rusqlite::Result<Value> = match step["op"].as_str().unwrap() {
                    "record_gateway_session_peer" => {
                        let record = GatewayPeerRecord {
                            peer: GatewayPeer {
                                source: args["source"].as_str().unwrap(),
                                session_key: args["session_key"].as_str(),
                                user_id: args["user_id"].as_str(),
                                chat_id: args["chat_id"].as_str(),
                                chat_type: args["chat_type"].as_str(),
                                thread_id: args["thread_id"].as_str(),
                            },
                            display_name: args["display_name"].as_str(),
                            origin_json: args["origin_json"].as_str(),
                            include_compression_ancestors: args["include_compression_ancestors"]
                                .as_bool()
                                .unwrap_or(false),
                        };
                        db.record_peer_with_owner(
                            id,
                            &record,
                            || case["owner_profile"].as_str().map(str::to_owned),
                            || 100.0,
                        )
                        .map(|()| Value::Null)
                    }
                    "set_expiry_finalized" => db
                        .set_expiry_finalized(id, args["finalized"].as_bool().unwrap_or(true))
                        .map(|()| Value::Null),
                    _ => unreachable!(),
                };
                assert_eq!(
                    result.is_err(),
                    !step["error"].is_null(),
                    "{}: {step}",
                    case["name"]
                );
                if let Ok(result) = result {
                    assert_eq!(result, step["result"], "{}", case["name"]);
                }
                let conn = db.conn.lock().unwrap();
                let mut query = conn.prepare("SELECT id,source,user_id,session_key,chat_id,chat_type,thread_id,display_name,origin_json,expiry_finalized,parent_session_id,started_at,ended_at,end_reason,model_config,profile_name FROM sessions ORDER BY id").unwrap();
                let mut rows: Vec<Value> = query
                    .query_map([], session_row_value)
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                for row in &mut rows {
                    if let Some(raw) = row["model_config"].as_str() {
                        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                            row["model_config"] = parsed;
                        }
                    }
                }
                assert_eq!(
                    serde_json::json!(rows),
                    step["sessions"],
                    "{}: {step}",
                    case["name"]
                );
            }
            drop(db);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn lifecycle_operations_match_python_sqlite() {
        use rusqlite::types::Value as SqlValue;
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-lifecycle-goldens.json"
        ))
        .unwrap();
        for case in cases {
            let path = temp_db("lifecycle_python");
            let db = SessionDb::open(path.clone()).unwrap();
            {
                let conn = db.conn.lock().unwrap();
                for (table, field) in [
                    ("sessions", "initial_sessions"),
                    ("conversation_generations", "initial_generations"),
                ] {
                    for row in case[field].as_array().unwrap() {
                        let row = row.as_object().unwrap();
                        let names: Vec<_> = row
                            .keys()
                            .map(|k| format!("\"{}\"", k.replace('"', "\"\"")))
                            .collect();
                        let placeholders = vec!["?"; names.len()].join(",");
                        let values: Vec<SqlValue> = row
                            .values()
                            .map(|v| match v {
                                Value::Null => SqlValue::Null,
                                Value::String(s) => SqlValue::Text(s.clone()),
                                Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
                                Value::Number(n) if n.is_i64() => {
                                    SqlValue::Integer(n.as_i64().unwrap())
                                }
                                Value::Number(n) => SqlValue::Real(n.as_f64().unwrap()),
                                v => SqlValue::Text(v.to_string()),
                            })
                            .collect();
                        conn.execute(
                            &format!(
                                "INSERT INTO {table} ({}) VALUES ({placeholders})",
                                names.join(",")
                            ),
                            rusqlite::params_from_iter(values),
                        )
                        .unwrap();
                    }
                }
            }
            for step in case["steps"].as_array().unwrap() {
                let args = &step["args"];
                let id = args["session_id"].as_str().unwrap();
                let result: rusqlite::Result<Value> = match step["op"].as_str().unwrap() {
                    "end_session" => db
                        .end_session_with_clock(id, args["end_reason"].as_str().unwrap(), || 100.0)
                        .map(|()| Value::Null),
                    "reopen_session" => db.reopen_session(id).map(|()| Value::Null),
                    "promote_to_session_reset" => Ok(Value::Bool(
                        db.promote_session_at(
                            id,
                            args["reason"].as_str().unwrap_or("session_reset"),
                            100.0,
                        )
                        .unwrap_or(false),
                    )),
                    _ => unreachable!(),
                };
                assert_eq!(
                    result.is_err(),
                    !step["error"].is_null(),
                    "{}: {step}",
                    case["name"]
                );
                if let Ok(result) = result {
                    assert_eq!(result, step["result"], "{}", case["name"]);
                }
                let conn = db.conn.lock().unwrap();
                let mut query = conn.prepare("SELECT id,source,session_key,parent_session_id,started_at,ended_at,end_reason,model_config FROM sessions ORDER BY id").unwrap();
                let mut rows: Vec<Value> = query
                    .query_map([], session_row_value)
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                for row in &mut rows {
                    if let Some(raw) = row["model_config"].as_str() {
                        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                            row["model_config"] = parsed;
                        }
                    }
                }
                assert_eq!(
                    serde_json::json!(rows),
                    step["sessions"],
                    "{}: {step}",
                    case["name"]
                );
                let mut query = conn.prepare("SELECT source,session_key,generation FROM conversation_generations ORDER BY source,session_key").unwrap();
                let rows: Vec<Value> = query
                    .query_map([], session_row_value)
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                assert_eq!(
                    serde_json::json!(rows),
                    step["conversation_generations"],
                    "{}: {step}",
                    case["name"]
                );
            }
            drop(db);
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn reset_promotion_and_reopen_keep_generation_and_child_lineage() {
        let db = SessionDb::open(temp_db("lifecycle_boundary")).unwrap();
        db.ensure_session("parent", "slack", None, None, None)
            .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET session_key = 'key' WHERE id = 'parent'",
                [],
            )
            .unwrap();
        db.end_session_with_clock("parent", "agent_close", || 10.0)
            .unwrap();
        db.end_session_with_clock("parent", "daily", || 20.0)
            .unwrap();
        assert_eq!(
            db.get_session("parent").unwrap().unwrap()["end_reason"],
            "agent_close"
        );
        assert!(db.promote_session_at("parent", "daily", 30.0).unwrap());
        assert!(!db.promote_session_at("parent", "idle", 40.0).unwrap());
        db.conn.lock().unwrap().execute_batch("INSERT INTO sessions (id,source,session_key,parent_session_id,started_at)
            VALUES ('child','slack','key','parent',31), ('branch','slack','different','parent',31);").unwrap();
        db.reopen_session("parent").unwrap();
        let child = db.get_session("child").unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(child["model_config"].as_str().unwrap()).unwrap()
                ["_reset_from"],
            "parent"
        );
        assert!(db.get_session("branch").unwrap().unwrap()["model_config"].is_null());
        assert!(db.get_session("parent").unwrap().unwrap()["ended_at"].is_null());
        db.end_session_with_clock("parent", "session_reset", || 50.0)
            .unwrap();
        let conn = db.conn.lock().unwrap();
        let generation: i64 = conn.query_row("SELECT generation FROM conversation_generations WHERE source = 'slack' AND session_key = 'key'", [], |r| r.get(0)).unwrap();
        assert_eq!(generation, 2);
        conn.execute("DELETE FROM sessions", []).unwrap();
        assert_eq!(
            conn.query_row("SELECT generation FROM conversation_generations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn failed_generation_and_child_stamp_roll_back_lifecycle_writes() {
        let db = SessionDb::open(temp_db("lifecycle_rollback")).unwrap();
        db.ensure_session("s", "slack", None, None, None).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "UPDATE sessions SET session_key = 'key' WHERE id = 's';
            CREATE TRIGGER reject_generation BEFORE INSERT ON conversation_generations
            BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        assert!(db.end_session_with_clock("s", "daily", || 10.0).is_err());
        assert!(!db.promote_to_session_reset("s", "daily"));
        assert!(db.get_session("s").unwrap().unwrap()["ended_at"].is_null());
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "DROP TRIGGER reject_generation;
            UPDATE sessions SET ended_at = 10, end_reason = 'daily' WHERE id = 's';
            INSERT INTO sessions (id,source,session_key,parent_session_id,started_at,model_config)
            VALUES ('bad','slack','key','s',11,'not-json');",
            )
            .unwrap();
        assert!(db.reopen_session("s").is_err());
        assert_eq!(db.get_session("s").unwrap().unwrap()["end_reason"], "daily");
    }

    #[test]
    fn peer_recovery_matches_python_sqlite_cases() {
        use rusqlite::types::Value as SqlValue;
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/session-peer-goldens.json")).unwrap();
        for case in cases {
            let base = temp_db("peer_python");
            let root = base.parent().unwrap().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let path = match case["owner_profile"].as_str() {
                None => base.clone(),
                Some("default") => root.join("state.db"),
                Some(owner) => root.join("profiles").join(owner).join("state.db"),
            };
            let db = SessionDb::open(path).unwrap();
            {
                let conn = db.conn.lock().unwrap();
                for table in ["system_prompts", "sessions", "messages"] {
                    for row in case[table].as_array().unwrap() {
                        let row = row.as_object().unwrap();
                        let names: Vec<_> = row
                            .keys()
                            .map(|k| format!("\"{}\"", k.replace('"', "\"\"")))
                            .collect();
                        let placeholders = vec!["?"; names.len()].join(",");
                        let values: Vec<SqlValue> = row
                            .values()
                            .map(|v| match v {
                                Value::Null => SqlValue::Null,
                                Value::String(s) => SqlValue::Text(s.clone()),
                                Value::Number(n) if n.is_i64() => {
                                    SqlValue::Integer(n.as_i64().unwrap())
                                }
                                Value::Number(n) => SqlValue::Real(n.as_f64().unwrap()),
                                _ => panic!("unexpected fixture scalar"),
                            })
                            .collect();
                        conn.execute(
                            &format!(
                                "INSERT INTO {table} ({}) VALUES ({placeholders})",
                                names.join(",")
                            ),
                            rusqlite::params_from_iter(values),
                        )
                        .unwrap();
                    }
                }
            }
            let lookup = &case["lookup"];
            let peer = GatewayPeer {
                source: lookup["source"].as_str().unwrap(),
                session_key: lookup["session_key"].as_str(),
                user_id: lookup["user_id"].as_str(),
                chat_id: lookup["chat_id"].as_str(),
                chat_type: lookup["chat_type"].as_str(),
                thread_id: lookup["thread_id"].as_str(),
            };
            let row = db.find_peer_with_root(&peer, &root).unwrap();
            assert_eq!(
                row.as_ref().map(|r| r["id"].clone()).unwrap_or(Value::Null),
                case["expected_session_id"],
                "{}",
                case["name"]
            );
            drop(db);
            std::fs::remove_dir_all(base.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn exact_recovery_prefers_messages_and_respects_strict_reset_boundary() {
        let db = SessionDb::open(temp_db("exact_recovery")).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("INSERT INTO sessions (id, source, session_key, started_at, last_activity_at, message_count)
                VALUES ('empty', 'slack', 'key', 20, 20, 0), ('conversation', 'slack', 'key', 1, 10, 0);").unwrap();
        }
        db.append_message("conversation", "user", "durable conversation")
            .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET last_activity_at = 10 WHERE id = 'conversation'",
                [],
            )
            .unwrap();
        let peer = GatewayPeer {
            source: "slack",
            session_key: Some("key"),
            ..Default::default()
        };
        assert_eq!(
            db.find_latest_gateway_session_for_peer(&peer)
                .unwrap()
                .unwrap()["id"],
            "conversation"
        );
        db.conn.lock().unwrap().execute_batch("DELETE FROM sessions WHERE id = 'empty';
            INSERT INTO sessions (id,source,session_key,started_at,ended_at,end_reason) VALUES ('reset','slack','key',0,10,'session_reset');").unwrap();
        assert_eq!(
            db.find_latest_gateway_session_for_peer(&peer)
                .unwrap()
                .unwrap()["id"],
            "conversation"
        );
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET ended_at = 10.001 WHERE id = 'reset'",
                [],
            )
            .unwrap();
        assert!(db
            .find_latest_gateway_session_for_peer(&peer)
            .unwrap()
            .is_none());
    }

    #[test]
    fn peer_recovery_uses_store_owner_and_never_crosses_threads() {
        let base = temp_db("owned_recovery");
        let root = base.parent().unwrap();
        let db = SessionDb::open(root.join("profiles/work/state.db")).unwrap();
        db.conn.lock().unwrap().execute_batch("INSERT INTO sessions
            (id,source,session_key,user_id,chat_id,chat_type,thread_id,started_at,message_count,profile_name)
            VALUES ('ours','slack','old','U','C','group','thread',1,1,'work'),
                   ('sibling','slack','other','U','C','group','thread',2,1,'other'),
                   ('different-thread','slack','old2','U','C','group','elsewhere',3,1,'work');").unwrap();
        let mut peer = GatewayPeer {
            source: "slack",
            session_key: Some("missing"),
            user_id: Some("U"),
            chat_id: Some("C"),
            chat_type: Some("group"),
            thread_id: Some("thread"),
        };
        assert_eq!(
            db.find_peer_with_root(&peer, root).unwrap().unwrap()["id"],
            "ours"
        );
        peer.chat_id = None;
        assert!(db.find_peer_with_root(&peer, root).unwrap().is_none());
        peer.chat_id = Some("C");
        peer.session_key = Some("other");
        // Exact-key lookup intentionally precedes the fallback owner fence;
        // the SessionStore profile guard validates its namespace afterward.
        assert_eq!(
            db.find_peer_with_root(&peer, root).unwrap().unwrap()["id"],
            "sibling"
        );
    }

    #[test]
    fn recovery_schema_upgrades_existing_rows_without_losing_extra_columns() {
        let path = temp_db("recovery_schema");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE sessions (
            id TEXT PRIMARY KEY, source TEXT NOT NULL, session_key TEXT,
            chat_id TEXT, chat_type TEXT, thread_id TEXT, started_at REAL NOT NULL,
            message_count INTEGER DEFAULT 0, last_activity_at REAL, future_column TEXT);
            INSERT INTO sessions (id, source, started_at, future_column) VALUES ('old', 'slack', 12.5, 'preserved');").unwrap();
        drop(conn);
        let db = SessionDb::open(path.clone()).unwrap();
        let row = db.get_session("old").unwrap().unwrap();
        assert_eq!(row["future_column"], "preserved");
        assert_eq!(row["started_at"], 12.5);
        for column in [
            "user_id",
            "profile_name",
            "origin_json",
            "display_name",
            "parent_session_id",
            "ended_at",
            "end_reason",
            "system_prompt",
            "system_prompt_hash",
            "tool_names",
        ] {
            assert!(row.get(column).unwrap().is_null(), "{column}");
        }
        assert_eq!(row["expiry_finalized"], 0);
        db.append_message("old", "user", "history survives migration")
            .unwrap();
        drop(db);
        let db = SessionDb::open(path).unwrap();
        assert_eq!(
            db.get_session("old").unwrap().unwrap()["future_column"],
            "preserved"
        );
        assert_eq!(
            db.load_history("old", 10).unwrap()[0].content,
            "history survives migration"
        );
        assert!(db.get_session("missing").unwrap().is_none());
    }

    #[test]
    fn lifecycle_reads_resolve_prompts_and_preserve_routing_fields() {
        let path = temp_db("lifecycle_read");
        let db = SessionDb::open(path).unwrap();
        db.ensure_session("s", "slack", None, Some("C"), Some("group"))
            .unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE sessions SET system_prompt = 'inline', system_prompt_hash = 'hash',
                ended_at = 50, end_reason = 'agent_close', profile_name = 'work',
                origin_json = '{\"scope_id\":\"T\"}', parent_session_id = 'parent' WHERE id = 's'",
                [],
            )
            .unwrap();
        }
        let row = db.get_session("s").unwrap().unwrap();
        assert_eq!(row["system_prompt"], "inline");
        assert_eq!(row["ended_at"], 50.0);
        assert_eq!(row["end_reason"], "agent_close");
        assert_eq!(row["profile_name"], "work");
        assert_eq!(row["parent_session_id"], "parent");
        assert_eq!(row["origin_json"], "{\"scope_id\":\"T\"}");
        assert!(row.get("_system_prompt_resolved").is_none());
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO system_prompts VALUES ('hash', 'deduplicated')",
                [],
            )
            .unwrap();
        assert_eq!(
            db.get_session("s").unwrap().unwrap()["system_prompt"],
            "deduplicated"
        );
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE system_prompts SET prompt = '' WHERE hash = 'hash'",
                [],
            )
            .unwrap();
        assert_eq!(db.get_session("s").unwrap().unwrap()["system_prompt"], "");
    }

    #[test]
    fn routing_schema_repairs_match_python() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-routing-schema-goldens.json"
        ))
        .unwrap();
        for case in cases {
            let path = temp_db("routing_schema_python");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(case["sql"].as_str().unwrap()).unwrap();
            drop(conn);
            let db = SessionDb::open(path).unwrap();
            let conn = db.conn.lock().unwrap();
            let mut query = conn.prepare("SELECT scope, session_key, entry_json, updated_at FROM gateway_routing ORDER BY scope, session_key").unwrap();
            let rows: Vec<Value> = query
                .query_map([], |r| {
                    Ok(serde_json::json!([
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, f64>(3)?
                    ]))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(serde_json::json!(rows), case["rows"]);
        }
    }

    #[test]
    fn routing_schema_migration_preserves_legacy_rows_and_allows_scoped_keys() {
        for has_scope in [false, true] {
            let path = temp_db("routing_legacy_pk");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(if has_scope {
                "CREATE TABLE gateway_routing (scope TEXT, session_key TEXT PRIMARY KEY, entry_json TEXT, updated_at REAL);
                 INSERT INTO gateway_routing VALUES (NULL, 'key', 'legacy', 12.5);"
            } else {
                "CREATE TABLE gateway_routing (session_key TEXT PRIMARY KEY, entry_json TEXT, updated_at REAL);
                 INSERT INTO gateway_routing VALUES ('key', 'legacy', 12.5);"
            }).unwrap();
            drop(conn);
            let db = SessionDb::open(path.clone()).unwrap();
            assert_eq!(
                db.load_gateway_routing_entries("").unwrap()["key"],
                "legacy"
            );
            db.save_gateway_routing_entry("profile", "key", "scoped")
                .unwrap();
            drop(db);
            let reopened = SessionDb::open(path).unwrap();
            assert_eq!(
                reopened.load_gateway_routing_entries("").unwrap()["key"],
                "legacy"
            );
            assert_eq!(
                reopened.load_gateway_routing_entries("profile").unwrap()["key"],
                "scoped"
            );
            let conn = reopened.conn.lock().unwrap();
            let timestamp: f64 = conn
                .query_row(
                    "SELECT updated_at FROM gateway_routing WHERE scope = ''",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(timestamp, 12.5);
        }
    }

    #[test]
    fn routing_schema_rebuild_retains_newest_collision_and_rolls_back_failure() {
        let path = temp_db("routing_rebuild");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gateway_routing (scope TEXT, session_key TEXT, entry_json TEXT, updated_at REAL);
             INSERT INTO gateway_routing VALUES ('p', 'key', 'new', 20), ('p', 'key', 'old', 10), ('other', 'key', 'other', 5);"
        ).unwrap();
        drop(conn);
        let db = SessionDb::open(path).unwrap();
        assert_eq!(db.load_gateway_routing_entries("p").unwrap()["key"], "new");
        assert_eq!(
            db.load_gateway_routing_entries("other").unwrap()["key"],
            "other"
        );

        let path = temp_db("routing_rebuild_failure");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gateway_routing (session_key TEXT PRIMARY KEY, entry_json TEXT, updated_at REAL);
             INSERT INTO gateway_routing VALUES ('invalid', NULL, 1);"
        ).unwrap();
        assert!(SessionDb::open(path).is_err());
        // The failed NOT NULL copy restores the original table and even rolls
        // back the scope-column addition. No half-migrated schema escapes.
        let columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('gateway_routing')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(columns, 3);
        let legacy_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'gateway_routing_legacy_pk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_tables, 0);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM gateway_routing WHERE entry_json IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn routing_transitions_match_python_sqlite() {
        let path = temp_db("routing_python");
        let db = SessionDb::open(path).unwrap();
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-routing-db-goldens.json"
        ))
        .unwrap();
        for case in cases {
            let scope = case["scope"].as_str().unwrap();
            match case["op"].as_str().unwrap() {
                "save" => db
                    .save_gateway_routing_entry(
                        scope,
                        case["key"].as_str().unwrap(),
                        case["value"].as_str().unwrap(),
                    )
                    .unwrap(),
                "replace" => db
                    .replace_gateway_routing_entries(
                        scope,
                        &serde_json::from_value(case["entries"].clone()).unwrap(),
                    )
                    .unwrap(),
                "delete" => db
                    .delete_gateway_routing_entries(
                        scope,
                        &serde_json::from_value::<Vec<String>>(case["keys"].clone()).unwrap(),
                    )
                    .unwrap(),
                _ => unreachable!(),
            }
            for (scope, expected) in case["expected"].as_object().unwrap() {
                assert_eq!(
                    serde_json::to_value(db.load_gateway_routing_entries(scope).unwrap()).unwrap(),
                    *expected,
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn routing_rows_survive_reopen_and_keep_scopes_separate() {
        let path = temp_db("routing_scopes");
        let db = SessionDb::open(path.clone()).unwrap();
        db.save_gateway_routing_entry("profile-a", "same", "old")
            .unwrap();
        db.save_gateway_routing_entry("profile-b", "same", "other")
            .unwrap();
        db.save_gateway_routing_entry("profile-a", "same", "new")
            .unwrap();
        db.save_gateway_routing_entry("profile-a", "", "ignored")
            .unwrap();
        db.save_gateway_routing_entry("profile-a", "same", "")
            .unwrap();
        drop(db);
        let db = SessionDb::open(path).unwrap();
        assert_eq!(
            db.load_gateway_routing_entries("profile-a").unwrap(),
            BTreeMap::from([("same".into(), "new".into())])
        );
        db.replace_gateway_routing_entries(
            "profile-a",
            &BTreeMap::from([
                ("replacement".into(), "raw-json".into()),
                ("empty".into(), "".into()),
            ]),
        )
        .unwrap();
        assert_eq!(
            db.load_gateway_routing_entries("profile-a").unwrap(),
            BTreeMap::from([("replacement".into(), "raw-json".into())])
        );
        assert_eq!(
            db.load_gateway_routing_entries("profile-b").unwrap()["same"],
            "other"
        );
        db.delete_gateway_routing_entries("profile-a", &["replacement".into(), "missing".into()])
            .unwrap();
        assert!(db
            .load_gateway_routing_entries("profile-a")
            .unwrap()
            .is_empty());
        assert_eq!(
            db.load_gateway_routing_entries("profile-b").unwrap()["same"],
            "other"
        );
    }

    #[test]
    fn routing_replace_rolls_back_delete_and_partial_insert_on_failure() {
        let path = temp_db("routing_rollback");
        let db = SessionDb::open(path.clone()).unwrap();
        db.save_gateway_routing_entry("scope", "original", "retained")
            .unwrap();
        // Fail after the delete and first insert, exercising SQLite rollback
        // rather than mocking the persistence boundary.
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_route BEFORE INSERT ON gateway_routing
             WHEN NEW.session_key = 'z-fail' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        assert!(db
            .replace_gateway_routing_entries(
                "scope",
                &BTreeMap::from([
                    ("a-first".into(), "inserted-first".into()),
                    ("z-fail".into(), "rejected".into()),
                ])
            )
            .is_err());
        drop(db);
        let db = SessionDb::open(path).unwrap();
        assert_eq!(
            db.load_gateway_routing_entries("scope").unwrap(),
            BTreeMap::from([("original".into(), "retained".into())])
        );
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
    fn encode_content_string_is_verbatim_no_sentinel() {
        // Plain strings store raw, and strings that merely look like JSON are
        // never re-interpreted (no auto-detection without the sentinel).
        assert_eq!(
            encode_message_content(&serde_json::json!("hello world")),
            "hello world"
        );
        let looks_like_json = "[1,2,3]";
        let stored = encode_message_content(&serde_json::json!(looks_like_json));
        assert_eq!(stored, looks_like_json);
        assert!(!stored.starts_with(CONTENT_JSON_PREFIX));
        // Decoded back it stays a plain string, not a parsed array.
        let hm = HistoryMessage {
            role: "user".into(),
            content: stored,
        };
        assert_eq!(hm.model_content(), serde_json::json!("[1,2,3]"));
    }

    #[test]
    fn encode_content_array_and_object_are_prefixed_json() {
        let parts = serde_json::json!([
            {"type": "text", "text": "look at this"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]);
        let stored = encode_message_content(&parts);
        assert!(stored.starts_with(CONTENT_JSON_PREFIX));
        let hm = HistoryMessage {
            role: "user".into(),
            content: stored,
        };
        assert_eq!(hm.model_content(), parts);

        let obj = serde_json::json!({"type": "text", "text": "solo"});
        let hm2 = HistoryMessage {
            role: "user".into(),
            content: encode_message_content(&obj),
        };
        assert_eq!(hm2.model_content(), obj);
    }

    #[test]
    fn model_content_malformed_payload_falls_back_to_raw_string() {
        // A sentinel with a broken JSON body returns the raw stored string
        // (sentinel included), matching Python's warn-and-return-content path.
        let raw = format!("{CONTENT_JSON_PREFIX}{{not valid json");
        let hm = HistoryMessage {
            role: "assistant".into(),
            content: raw.clone(),
        };
        assert_eq!(hm.model_content(), Value::String(raw));
    }

    #[test]
    fn model_content_prefixed_scalar_decodes() {
        // Non-string scalars are prefixed on the way in so they round-trip back
        // to their typed value at the model layer (the safe-scalar limitation
        // only affects on-disk bytes, not the decoded value).
        for v in [
            serde_json::json!(42),
            serde_json::json!(3.5),
            serde_json::json!(true),
            serde_json::json!(null),
        ] {
            let hm = HistoryMessage {
                role: "user".into(),
                content: encode_message_content(&v),
            };
            assert_eq!(hm.model_content(), v);
        }
    }

    #[test]
    fn ascii_and_unicode_string_content_roundtrips_through_db() {
        let path = temp_db("unicode");
        let ascii = "plain ascii reply";
        let unicode = "日本語とemoji 🚀 café";
        {
            let db = SessionDb::open(path.clone()).unwrap();
            db.ensure_session("s1", "cli", None, None, None).unwrap();
            db.append_message(
                "s1",
                "user",
                &encode_message_content(&serde_json::json!(ascii)),
            )
            .unwrap();
            db.append_message(
                "s1",
                "assistant",
                &encode_message_content(&serde_json::json!(unicode)),
            )
            .unwrap();
        }
        // Reopen from disk and confirm replay is byte-for-byte.
        let db = SessionDb::open(path.clone()).unwrap();
        let hist = db.load_history("s1", 0).unwrap();
        assert_eq!(hist[0].model_content(), serde_json::json!(ascii));
        assert_eq!(hist[1].model_content(), serde_json::json!(unicode));
        // Stored verbatim: no sentinel snuck onto plain strings.
        assert_eq!(hist[0].content, ascii);
        assert_eq!(hist[1].content, unicode);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn image_parts_roundtrip_through_sqlite_reopen() {
        let path = temp_db("imgparts");
        let parts = serde_json::json!([
            {"type": "text", "text": "describe this 日本語"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
        ]);
        {
            let db = SessionDb::open(path.clone()).unwrap();
            db.ensure_session("s1", "cli", None, None, None).unwrap();
            db.append_message("s1", "user", &encode_message_content(&parts))
                .unwrap();
        }
        // Drop the handle, reopen the file, and replay the structured content.
        let db = SessionDb::open(path.clone()).unwrap();
        let hist = db.load_history("s1", 0).unwrap();
        assert_eq!(hist.len(), 1);
        assert!(hist[0].content.starts_with(CONTENT_JSON_PREFIX));
        assert_eq!(hist[0].model_content(), parts);
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

#[cfg(test)]
mod golden_corpus {
    use super::*;

    #[test]
    fn structured_codec_matches_python_values() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/content-storage-goldens.json"))
                .unwrap();
        for case in cases {
            let from_python = HistoryMessage {
                role: "user".into(),
                content: case["stored"].as_str().unwrap().into(),
            };
            assert_eq!(from_python.model_content(), case["decoded"], "{case}");
            if let Some(input) = case.get("input") {
                let from_rust = HistoryMessage {
                    role: "user".into(),
                    content: encode_message_content(input),
                };
                assert_eq!(from_rust.model_content(), case["decoded"], "{case}");
            }
        }
    }
}
